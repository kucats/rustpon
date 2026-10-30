//! Explicit trust boundary between parsed multipart bytes and application uploads.
//!
//! An admitted request consumes the raw Request. Files rejected by client-side
//! MAX_FILE_SIZE and empty upload controls are immediately closed; server-side
//! byte quotas fail the whole transaction, closing every staged descriptor.
//! The only public payload accessor is gated on a successful admission.
//!
//! This deliberately does not expose Zend pointers, a filesystem path, or an
//! implementation of PHP's upload registry. It is not an SAPI integration.

use crate::{Payload, Request, files, post};
use std::io::{self, Write};

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// Server-controlled maximum HTTP request body bytes, inclusive.
    pub max_body_bytes: u64,
    /// Server-controlled maximum size of each staged upload, inclusive.
    /// This never comes from MAX_FILE_SIZE in the request body.
    pub max_file_bytes: u64,
    /// Server-controlled aggregate size of all staged uploads, including
    /// uploads later discarded by PHP-style name collisions or form limits.
    /// Enforced on raw bytes before publishing metadata.
    pub max_total_file_bytes: u64,
    /// Additional quotas for the metadata projection.
    pub metadata: files::Limits,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_body_bytes: crate::Limits::default().body_bytes,
            max_file_bytes: crate::Limits::default().file_bytes,
            max_total_file_bytes: crate::Limits::default().file_bytes_total,
            metadata: files::Limits::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    BodyTooLarge,
    FileTooLarge,
    FilesTooLarge,
    Metadata(files::Error),
    Allocation,
}

impl From<files::Error> for Error {
    fn from(error: files::Error) -> Self {
        Self::Metadata(error)
    }
}

/// Successful transaction: raw request is sealed behind a validated accessor.
/// The metadata is a separate testing projection, not real PHP zvals.
#[derive(Debug)]
pub struct AdmittedRequest {
    request: Request,
    metadata: post::PostRequest,
}

/// Typed view of a successfully admitted upload. Integer fields use a
/// 64-bit signed representation compatible with 64-bit PHP zend_long.
/// This is Rust-only metadata; no Zend zval or upload registry is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmittedFile<'a> {
    pub name: &'a [u8],
    pub full_path: &'a [u8],
    pub content_type: &'a [u8],
    pub size: i64,
    pub error: i64,
}

impl AdmittedRequest {
    /// Bounded, path-sanitized $_FILES-shaped metadata projection.
    pub fn metadata(&self) -> &post::PostRequest {
        &self.metadata
    }

    /// Returns typed upload metadata only for admitted, live file descriptors.
    /// Replaced, empty, oversized or otherwise rejected indices return None.
    pub fn accepted_file(&self, index: usize) -> Option<AdmittedFile<'_>> {
        let part = self.request.parts.get(index)?;
        let Payload::File(upload) = &part.payload else {
            return None;
        };
        let full_path = &self.request.arena[part.filename.as_ref()?.clone()];
        let name = full_path
            .rsplit(|byte| *byte == b'/' || *byte == b'\\')
            .next()
            .unwrap_or(full_path);
        let content_type = part
            .content_type
            .as_ref()
            .map(|range| &self.request.arena[range.clone()])
            .unwrap_or(b"");
        Some(AdmittedFile {
            name,
            full_path,
            content_type,
            size: i64::try_from(upload.len()).ok()?,
            error: 0,
        })
    }

    /// Indices in the original wire request that remain eligible for copying.
    /// Rejected/empty uploads never appear, and their descriptors are closed.
    pub fn accepted_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.request
            .parts
            .iter()
            .enumerate()
            .filter_map(|(index, part)| {
                if matches!(part.payload, Payload::File(_)) {
                    Some(index)
                } else {
                    None
                }
            })
    }

    /// Copy a single admitted upload. Invalid/rejected/non-file indices
    /// return None without exposing any raw upload descriptor.
    pub fn copy_accepted(
        &mut self,
        index: usize,
        output: &mut impl Write,
    ) -> io::Result<Option<u64>> {
        self.request
            .upload_mut(index)
            .map(|upload| upload.copy_to(output))
            .transpose()
    }

    /// Return a complete, application-capped upload buffer only after all
    /// staged-inode integrity checks succeed. Unlike copy_accepted, no
    /// destination prefix escapes on failure. Rejected or overwritten file
    /// indices return None without allocating a payload buffer.
    ///
    /// The caller must supply an explicit max_bytes budget. This allocates
    /// a second in-memory copy and is not an atomic filesystem snapshot.
    pub fn copy_accepted_to_vec(
        &mut self,
        index: usize,
        max_bytes: usize,
    ) -> io::Result<Option<Vec<u8>>> {
        self.request
            .upload_mut(index)
            .map(|upload| upload.copy_to_vec(max_bytes))
            .transpose()
    }
}

fn enforce_server_quotas(request: &Request, policy: Policy) -> Result<(), Error> {
    if request.stats().body_bytes > policy.max_body_bytes {
        return Err(Error::BodyTooLarge);
    }
    let mut total_file_bytes = 0u64;
    for part in request.parts() {
        if let Some(len) = part.file_bytes {
            if len > policy.max_file_bytes || len > i64::MAX as u64 {
                return Err(Error::FileTooLarge);
            }
            total_file_bytes = total_file_bytes
                .checked_add(len)
                .ok_or(Error::FilesTooLarge)?;
            if total_file_bytes > policy.max_total_file_bytes {
                return Err(Error::FilesTooLarge);
            }
        }
    }
    Ok(())
}

/// Opt-in PHP 8.4 post_max_size admission semantics. A nonzero limit smaller
/// than the complete request body publishes an empty FILES projection rather
/// than failing the request. Zero disables the PHP quota. Independent
/// server-controlled hard limits still fail closed, even for discarded bodies.
///
/// This is an after-parse compatibility adapter: the wire parser has already
/// consumed and staged the body. A future SAPI integration must reject early
/// based on Content-Length without staging an oversized upload.
pub fn admit_with_php_post_max_size(
    mut request: Request,
    policy: Policy,
    max_post_bytes: u64,
) -> Result<AdmittedRequest, Error> {
    if max_post_bytes == 0 || request.stats().body_bytes <= max_post_bytes {
        return admit(request, policy);
    }
    enforce_server_quotas(&request, policy)?;
    // Drop staged upload FDs and all raw field bytes before publication.
    // Replacing the vectors also releases their backing allocations.
    request.parts = Vec::new();
    request.arena = Vec::new();
    let metadata = files::project(&request, policy.metadata)?;
    Ok(AdmittedRequest { request, metadata })
}

/// Server-controlled policy enforcement consumes the raw wire request,
/// including on errors. A metadata error cannot leak its staged file handles.
pub fn admit(mut request: Request, policy: Policy) -> Result<AdmittedRequest, Error> {
    enforce_server_quotas(&request, policy)?;

    // Project before discarding, so all error codes, names and sizes are
    // preserved. No metadata escapes if an adapter quota fails.
    let metadata = files::project(&request, policy.metadata)?;
    // Re-project with per-wire-index identity values in tmp_name leaves.
    // PHP-style key collision/append/scalar replacement happens in the same
    // bounded POST adapter as the publicly visible FILES metadata.
    // Only identities surviving the final tree may retain upload FDs.
    let identities = files::project_with_ids(&request, policy.metadata)?;
    let mut selected = Vec::new();
    selected
        .try_reserve_exact(request.parts.len())
        .map_err(|_| Error::Allocation)?;
    selected.resize(request.parts.len(), false);
    identities.visit_file_tmp_values(|value| {
        if let Some(number) = value
            .strip_prefix(b"__RUSTPON_UPLOAD_ID_")
            .and_then(|tail| tail.strip_suffix(b"__"))
            .and_then(|ascii| std::str::from_utf8(ascii).ok())
            .and_then(|ascii| ascii.parse::<usize>().ok())
            && let Some(slot) = selected.get_mut(number)
        {
            *slot = true;
        }
    });

    for (index, part) in request.parts.iter_mut().enumerate() {
        if matches!(part.payload, Payload::File(_)) && !selected[index] {
            // The original descriptor is dropped now, not merely hidden by
            // the final metadata tree. Rejected and overwritten uploads can
            // never be copied via the sealed admitted request API.
            part.payload = Payload::Field(0..0);
        }
    }
    Ok(AdmittedRequest { request, metadata })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Limits as WireLimits, Parser};
    use std::fs;
    use std::os::unix::fs::DirBuilderExt;

    struct Spool(std::path::PathBuf);
    impl Spool {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rustpon-admission-{}-{}",
                std::process::id(),
                crate::NEXT_UPLOAD.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }

        fn parse(&self, input: &[u8]) -> Request {
            let mut parser = Parser::new(b"b", WireLimits::default(), &self.0).unwrap();
            parser.feed(input).unwrap();
            parser.finish().unwrap()
        }

        fn empty(&self) {
            assert_eq!(fs::read_dir(&self.0).unwrap().count(), 0);
        }
    }
    impl Drop for Spool {
        fn drop(&mut self) {
            self.empty();
            fs::remove_dir(&self.0).unwrap();
        }
    }

    const BODY: &[u8] = b"--b\r\nContent-Disposition: form-data; name=MAX_FILE_SIZE\r\n\r\n2\r\n--b\r\nContent-Disposition: form-data; name=bad; filename=bad.bin\r\n\r\nabc\r\n--b\r\nContent-Disposition: form-data; name=MAX_FILE_SIZE\r\n\r\n4\r\n--b\r\nContent-Disposition: form-data; name=good; filename=good.bin\r\n\r\nxyz\r\n--b\r\nContent-Disposition: form-data; name=empty; filename=\"\"\r\n\r\n\r\n--b--\r\n";

    #[test]
    fn admission_drops_failed_uploads_before_exposing_payloads() {
        let spool = Spool::new();
        let raw = spool.parse(BODY);
        assert_eq!(raw.stats().files, 3);
        let mut admitted = admit(raw, Policy::default()).unwrap();
        let indices: Vec<_> = admitted.accepted_indices().collect();
        assert_eq!(indices, vec![3]);
        let accepted = admitted.accepted_file(3).unwrap();
        assert_eq!(accepted.name, b"good.bin");
        assert_eq!(accepted.full_path, b"good.bin");
        assert_eq!(accepted.content_type, b"");
        assert_eq!(accepted.size, 3);
        assert_eq!(accepted.error, 0);
        assert!(admitted.accepted_file(1).is_none());
        assert!(admitted.accepted_file(4).is_none());
        let mut accepted = Vec::new();
        assert_eq!(admitted.copy_accepted(3, &mut accepted).unwrap(), Some(3));
        assert_eq!(accepted, b"xyz");
        let mut rejected = Vec::new();
        assert_eq!(admitted.copy_accepted(1, &mut rejected).unwrap(), None);
        assert_eq!(admitted.copy_accepted(4, &mut rejected).unwrap(), None);
        assert!(rejected.is_empty());
        // Only one live upload file descriptor remains.
        let mut retained = 0;
        for entry in indices {
            let mut sink = io::sink();
            retained += admitted.copy_accepted(entry, &mut sink).unwrap().unwrap();
        }
        assert_eq!(retained, 3);
        drop(admitted);
        spool.empty();
    }

    #[test]
    fn admitted_copy_to_vec_enforces_cap_and_survivor_identity() {
        let spool = Spool::new();
        // Same deterministic PHP 8.4 HTTP SAPI oracle payload as
        // file-transactional-vec-oracle in tools/differential_multipart.py.
        let binary: Vec<u8> = (0..8192 + 29)
            .map(|i| ((i * 43 + 251) % 256) as u8)
            .collect();
        let mut body = Vec::new();
        for (name, data) in [
            (b"upload".as_slice(), b"replaced".as_slice()),
            (b"upload".as_slice(), binary.as_slice()),
        ] {
            body.extend_from_slice(b"--b\r\nContent-Disposition: form-data; name=\"");
            body.extend_from_slice(name);
            body.extend_from_slice(b"\"; filename=\"transaction.bin\"\r\n\r\n");
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"--b--\r\n");
        let mut admitted = admit(spool.parse(&body), Policy::default()).unwrap();
        assert_eq!(admitted.accepted_indices().collect::<Vec<_>>(), vec![1]);
        assert_eq!(admitted.copy_accepted_to_vec(0, 0).unwrap(), None);
        assert_eq!(admitted.copy_accepted_to_vec(2, 0).unwrap(), None);
        assert_eq!(
            admitted.copy_accepted_to_vec(1, binary.len() - 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        // A failed cap check must not consume the descriptor or publish a
        // partial prefix. Retry with the exact cap and compare every byte.
        assert_eq!(
            admitted.copy_accepted_to_vec(1, binary.len()).unwrap(),
            Some(binary.clone())
        );
        assert_eq!(
            admitted.copy_accepted_to_vec(1, binary.len() + 1).unwrap(),
            Some(binary)
        );
        drop(admitted);
        spool.empty();
    }

    #[test]
    fn server_quotas_abort_whole_transaction_and_close_every_upload() {
        let spool = Spool::new();
        let bytes = spool.parse(BODY).stats().body_bytes;
        assert!(matches!(
            admit(
                spool.parse(BODY),
                Policy {
                    max_body_bytes: bytes - 1,
                    ..Policy::default()
                }
            ),
            Err(Error::BodyTooLarge)
        ));
        spool.empty();
        assert!(matches!(
            admit(
                spool.parse(BODY),
                Policy {
                    max_file_bytes: 2,
                    ..Policy::default()
                }
            ),
            Err(Error::FileTooLarge)
        ));
        spool.empty();
        assert!(matches!(
            admit(
                spool.parse(BODY),
                Policy {
                    metadata: files::Limits {
                        metadata_bytes: 0,
                        ..files::Limits::default()
                    },
                    ..Policy::default()
                }
            ),
            Err(Error::Metadata(files::Error::MetadataBytes))
        ));
        spool.empty();
        let mut admitted = admit(
            spool.parse(BODY),
            Policy {
                max_body_bytes: bytes,
                max_file_bytes: 3,
                ..Policy::default()
            },
        )
        .unwrap();
        let mut data = Vec::new();
        assert_eq!(admitted.copy_accepted(3, &mut data).unwrap(), Some(3));
        assert_eq!(data, b"xyz");
        drop(admitted);
        spool.empty();
    }

    #[test]
    fn php_post_max_size_publishes_empty_files_and_closes_uploads() {
        let spool = Spool::new();
        let exact = BODY.len() as u64;
        let admitted =
            admit_with_php_post_max_size(spool.parse(BODY), Policy::default(), exact).unwrap();
        assert_eq!(admitted.accepted_indices().collect::<Vec<_>>(), vec![3]);
        drop(admitted);
        spool.empty();

        let rejected =
            admit_with_php_post_max_size(spool.parse(BODY), Policy::default(), exact - 1).unwrap();
        assert_eq!(rejected.accepted_indices().count(), 0);
        let mut json = Vec::new();
        rejected.metadata().write_json(&mut json).unwrap();
        assert_eq!(json.as_slice(), b"[\"a\",[]]");
        // Even while the admitted request is alive, every staged FD is gone.
        spool.empty();
        drop(rejected);
        spool.empty();

        let unlimited =
            admit_with_php_post_max_size(spool.parse(BODY), Policy::default(), 0).unwrap();
        assert_eq!(unlimited.accepted_indices().collect::<Vec<_>>(), vec![3]);
        drop(unlimited);
        spool.empty();
    }

    #[test]
    fn php_post_max_size_never_bypasses_hard_server_quotas() {
        let spool = Spool::new();
        let exact = BODY.len() as u64;
        assert!(matches!(
            admit_with_php_post_max_size(
                spool.parse(BODY),
                Policy {
                    max_body_bytes: exact - 1,
                    ..Policy::default()
                },
                1,
            ),
            Err(Error::BodyTooLarge)
        ));
        spool.empty();
        assert!(matches!(
            admit_with_php_post_max_size(
                spool.parse(BODY),
                Policy {
                    max_file_bytes: 2,
                    ..Policy::default()
                },
                1,
            ),
            Err(Error::FileTooLarge)
        ));
        spool.empty();
        assert!(matches!(
            admit_with_php_post_max_size(
                spool.parse(BODY),
                Policy {
                    max_total_file_bytes: 5,
                    ..Policy::default()
                },
                1,
            ),
            Err(Error::FilesTooLarge)
        ));
        spool.empty();
    }

    #[test]
    fn aggregate_server_upload_quota_includes_rejected_and_overwritten_bytes() {
        let spool = Spool::new();
        // BODY contains two three-byte uploads and an empty filename control.
        // The first upload is rejected by MAX_FILE_SIZE, but still counts
        // toward the server-controlled aggregate admission budget.
        assert!(matches!(
            admit(
                spool.parse(BODY),
                Policy {
                    max_total_file_bytes: 5,
                    ..Policy::default()
                }
            ),
            Err(Error::FilesTooLarge)
        ));
        spool.empty();

        let mut admitted = admit(
            spool.parse(BODY),
            Policy {
                max_total_file_bytes: 6,
                ..Policy::default()
            },
        )
        .unwrap();
        assert_eq!(admitted.accepted_indices().collect::<Vec<_>>(), vec![3]);
        let mut bytes = Vec::new();
        assert_eq!(admitted.copy_accepted(3, &mut bytes).unwrap(), Some(3));
        assert_eq!(bytes, b"xyz");
        drop(admitted);
        spool.empty();

        // An overwritten wire upload must still consume ingress resources.
        let duplicate = b"--b\r\nContent-Disposition: form-data; name=f; filename=a\r\n\r\nabc\r\n--b\r\nContent-Disposition: form-data; name=f; filename=b\r\n\r\ndef\r\n--b--\r\n";
        assert!(matches!(
            admit(
                spool.parse(duplicate),
                Policy {
                    max_total_file_bytes: 5,
                    ..Policy::default()
                }
            ),
            Err(Error::FilesTooLarge)
        ));
        spool.empty();
        let admitted = admit(
            spool.parse(duplicate),
            Policy {
                max_total_file_bytes: 6,
                ..Policy::default()
            },
        )
        .unwrap();
        assert_eq!(admitted.accepted_indices().collect::<Vec<_>>(), vec![1]);
        drop(admitted);
        spool.empty();
    }

    #[test]
    fn file_descriptor_count_returns_to_baseline_after_admission() {
        const CHILD: &str = "RUSTPON_ADMISSION_FD_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "admission::tests::file_descriptor_count_returns_to_baseline_after_admission",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                child.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr)
            );
            return;
        }
        let fd_path = if std::path::Path::new("/proc/self/fd").is_dir() {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        let count = || fs::read_dir(fd_path).unwrap().count();
        let spool = Spool::new();
        let baseline = count();
        for _ in 0..32 {
            let raw = spool.parse(BODY);
            assert_eq!(count(), baseline + 3);
            let admitted = admit(raw, Policy::default()).unwrap();
            assert_eq!(count(), baseline + 1);
            assert_eq!(admitted.accepted_indices().collect::<Vec<_>>(), vec![3]);
            drop(admitted);
            assert_eq!(count(), baseline);
            let denied = admit(
                spool.parse(BODY),
                Policy {
                    max_file_bytes: 1,
                    ..Policy::default()
                },
            );
            assert!(matches!(denied, Err(Error::FileTooLarge)));
            assert_eq!(count(), baseline);
        }
    }

    #[test]
    fn only_php_visible_uploads_survive_overwrites_and_nested_replacements() {
        let spool = Spool::new();
        type UploadNames<'a> = &'a [&'a [u8]];
        type Case<'a> = (&'a str, UploadNames<'a>, &'a [usize]);
        let cases: &[Case<'_>] = &[
            ("flat-duplicate", &[b"f", b"f"], &[1]),
            ("nested-to-flat", &[b"f[x]", b"f"], &[1]),
            ("flat-to-nested", &[b"f", b"f[x]"], &[1]),
            ("normalized-roots", &[b"a.b", b"a_b"], &[1]),
            ("same-index", &[b"f[0]", b"f[0]"], &[1]),
            ("append-array", &[b"f[]", b"f[]"], &[0, 1]),
            ("distinct-nested", &[b"f[x]", b"f[y]"], &[0, 1]),
            ("string-number", &[b"f[0]", b"f[00]"], &[0, 1]),
        ];
        for &(label, names, survivors) in cases {
            let mut data = Vec::new();
            for (index, name) in names.iter().enumerate() {
                data.extend_from_slice(b"--b\r\nContent-Disposition: form-data; name=\"");
                data.extend_from_slice(name);
                data.extend_from_slice(b"\"; filename=\"sample.bin\"\r\n\r\n");
                data.extend_from_slice(if index == 0 { b"first" } else { b"second" });
                data.extend_from_slice(b"\r\n");
            }
            data.extend_from_slice(b"--b--\r\n");
            let mut admitted = admit(spool.parse(&data), Policy::default()).unwrap();
            assert_eq!(
                admitted.accepted_indices().collect::<Vec<_>>(),
                survivors,
                "{label}"
            );
            for &index in survivors {
                let file = admitted.accepted_file(index).unwrap();
                assert_eq!(file.name, b"sample.bin");
                assert_eq!(file.error, 0);
                assert_eq!(file.size, if index == 0 { 5 } else { 6 });
            }
            for index in 0..names.len() {
                let mut content = Vec::new();
                let copied = admitted.copy_accepted(index, &mut content).unwrap();
                if survivors.contains(&index) {
                    assert_eq!(copied, Some(content.len() as u64), "{label}");
                    assert_eq!(
                        content,
                        if index == 0 {
                            &b"first"[..]
                        } else {
                            &b"second"[..]
                        },
                        "{label}"
                    );
                } else {
                    assert_eq!(copied, None, "{label}");
                    assert!(content.is_empty(), "{label}");
                }
            }
            drop(admitted);
            spool.empty();
        }
    }

    #[test]
    fn php_max_file_uploads_skips_suffix_and_closes_descriptors() {
        let spool = Spool::new();
        let body = b"--b\r\nContent-Disposition: form-data; name=empty; filename=\"\"\r\n\r\n\r\n--b\r\nContent-Disposition: form-data; name=f1; filename=one.bin\r\n\r\none\r\n--b\r\nContent-Disposition: form-data; name=f2; filename=two.bin\r\n\r\ntwo\r\n--b\r\nContent-Disposition: form-data; name=f3; filename=three.bin\r\n\r\nthree\r\n--b\r\nContent-Disposition: form-data; name=tail\r\n\r\nvisible\r\n--b--\r\n";
        for (limit, expected) in [
            (0, &[][..]),
            (1, &[1][..]),
            (2, &[1, 2][..]),
            (3, &[1, 2, 3][..]),
        ] {
            let policy = Policy {
                metadata: files::Limits {
                    max_file_uploads: Some(limit),
                    ..files::Limits::default()
                },
                ..Policy::default()
            };
            let mut admitted = admit(spool.parse(body), policy).unwrap();
            let indices: Vec<_> = admitted.accepted_indices().collect();
            assert_eq!(indices.as_slice(), expected, "limit={limit}");
            for index in 0..4 {
                let mut data = Vec::new();
                let copied = admitted.copy_accepted(index, &mut data).unwrap();
                assert_eq!(
                    copied.is_some(),
                    expected.contains(&index),
                    "limit={limit}, index={index}"
                );
            }
            let mut typed = Vec::new();
            admitted
                .metadata()
                .write_php_files_json(&mut typed)
                .unwrap();
            let typed = String::from_utf8(typed).unwrap();
            assert_eq!(typed.contains("656d707479"), limit > 0); // empty control
            assert_eq!(typed.contains("74687265652e62696e"), limit >= 3); // three.bin
            drop(admitted);
            spool.empty();
        }
        // The default strict metadata path has no PHP file-count quota.
        let admitted = admit(spool.parse(body), Policy::default()).unwrap();
        assert_eq!(
            admitted.accepted_indices().collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        drop(admitted);
        spool.empty();
    }

    #[test]
    fn combined_php_upload_quotas_count_rejected_files_and_release_fds() {
        let spool = Spool::new();
        let body = b"--b\r\nContent-Disposition: form-data; name=f; filename=a.bin\r\n\r\nabc\r\n--b\r\nContent-Disposition: form-data; name=g; filename=b.bin\r\n\r\nx\r\n--b\r\nContent-Disposition: form-data; name=h; filename=c.bin\r\n\r\ny\r\n--b--\r\n";
        let metadata_limits = files::Limits {
            max_file_uploads: Some(2),
            max_upload_bytes: Some(2),
            ..files::Limits::default()
        };
        let mut admitted = admit(
            spool.parse(body),
            Policy {
                metadata: metadata_limits,
                ..Policy::default()
            },
        )
        .unwrap();
        assert_eq!(admitted.accepted_indices().collect::<Vec<_>>(), vec![1]);
        let pairs: Vec<_> = admitted
            .metadata()
            .wire()
            .parts()
            .map(|part| (part.name.to_vec(), part.value.unwrap().to_vec()))
            .collect();
        assert!(
            pairs
                .iter()
                .any(|(name, value)| name == b"f[error]" && value == b"1")
        );
        assert!(
            pairs
                .iter()
                .any(|(name, value)| name == b"g[error]" && value == b"0")
        );
        assert!(!pairs.iter().any(|(name, _)| name.starts_with(b"h[")));
        assert!(admitted.accepted_file(0).is_none());
        assert!(admitted.accepted_file(2).is_none());
        let mut bytes = Vec::new();
        assert_eq!(admitted.copy_accepted(0, &mut bytes).unwrap(), None);
        assert!(bytes.is_empty());
        assert_eq!(admitted.copy_accepted(1, &mut bytes).unwrap(), Some(1));
        assert_eq!(bytes, b"x");
        drop(admitted);
        spool.empty();

        // The oversized first upload consumes the only available file slot.
        let admitted = admit(
            spool.parse(body),
            Policy {
                metadata: files::Limits {
                    max_file_uploads: Some(1),
                    ..metadata_limits
                },
                ..Policy::default()
            },
        )
        .unwrap();
        assert_eq!(admitted.accepted_indices().count(), 0);
        drop(admitted);
        spool.empty();
    }

    #[test]
    fn repeated_requests_do_not_retain_rejected_descriptors() {
        let spool = Spool::new();
        for _ in 0..32 {
            let admitted = admit(spool.parse(BODY), Policy::default()).unwrap();
            assert_eq!(admitted.accepted_indices().collect::<Vec<_>>(), vec![3]);
            drop(admitted);
            spool.empty();
        }
    }
}
