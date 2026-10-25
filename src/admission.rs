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
    /// Additional quotas for the metadata projection.
    pub metadata: files::Limits,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_body_bytes: crate::Limits::default().body_bytes,
            max_file_bytes: crate::Limits::default().file_bytes,
            metadata: files::Limits::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    BodyTooLarge,
    FileTooLarge,
    Metadata(files::Error),
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

impl AdmittedRequest {
    /// Bounded, path-sanitized $_FILES-shaped metadata projection.
    pub fn metadata(&self) -> &post::PostRequest {
        &self.metadata
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
}

/// Server-controlled policy enforcement consumes the raw wire request,
/// including on errors. A metadata error cannot leak its staged file handles.
pub fn admit(mut request: Request, policy: Policy) -> Result<AdmittedRequest, Error> {
    if request.stats().body_bytes > policy.max_body_bytes {
        return Err(Error::BodyTooLarge);
    }
    if request.parts().any(|part| {
        part.file_bytes
            .is_some_and(|len| len > policy.max_file_bytes)
    }) {
        return Err(Error::FileTooLarge);
    }

    // Project before discarding, so all error codes, names and sizes are
    // preserved. No metadata escapes if an adapter quota fails.
    let metadata = files::project(&request, policy.metadata)?;
    let mut form_max = 0i64;
    for index in 0..request.parts.len() {
        let rejected = {
            let part = &request.parts[index];
            let name = &request.arena[part.name.clone()];
            if let Payload::Field(value) = &part.payload {
                if part.filename.is_none() && name.eq_ignore_ascii_case(b"MAX_FILE_SIZE") {
                    form_max = files::parse_form_max(&request.arena[value.clone()]);
                }
                false
            } else {
                let filename = part
                    .filename
                    .as_ref()
                    .map(|span| &request.arena[span.clone()])
                    .expect("file part must carry filename");
                let length = match &part.payload {
                    Payload::File(upload) => upload.len(),
                    Payload::Field(_) => unreachable!("file branch"),
                };
                filename.is_empty() || (form_max != 0 && (form_max < 0 || length > form_max as u64))
            }
        };
        if rejected {
            // Payload replacement drops the Upload/File immediately. The empty
            // range is internal only and never published through AdmittedRequest.
            request.parts[index].payload = Payload::Field(0..0);
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
