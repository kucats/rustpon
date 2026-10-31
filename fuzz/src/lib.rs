//! Bounded libFuzzer invariants and CI seed tests for request parsers.
use rustpon::cookie;
use rustpon::urlencoded::FormParser;
use rustpon::{Error, Limits, Parser, Request, Stats, admission, files, urlencoded};
use std::fmt::Debug;
use std::fs;
use std::io::{self, Cursor, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_INPUT: usize = 8192;

fn limits() -> Limits {
    Limits {
        body_bytes: MAX_INPUT as u64,
        parts: 16,
        fields: 16,
        files: 8,
        field_bytes: 4096,
        field_bytes_total: MAX_INPUT,
        file_bytes: MAX_INPUT as u64,
        file_bytes_total: MAX_INPUT as u64,
        retained_bytes: 32 * 1024,
        memory_capacity: 128 * 1024,
        boundary_comparisons: 16 * MAX_INPUT as u64,
        ..Limits::default()
    }
}

type PartSnapshot = (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>, Vec<u8>, bool);

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    parts: Vec<PartSnapshot>,
    stats: Stats,
}

// A deterministic destination that accepts a prefix, then simulates ENOSPC.
// The caller must discard its partial output; the source must remain retryable.
struct FailAfter {
    bytes: Vec<u8>,
    budget: usize,
}

impl Write for FailAfter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let available = self.budget.saturating_sub(self.bytes.len());
        if available == 0 {
            return Err(io::ErrorKind::StorageFull.into());
        }
        let count = data.len().min(available);
        self.bytes.extend_from_slice(&data[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn snapshot(mut request: Request) -> Snapshot {
    let count = request.parts().len();
    let mut parts = Vec::with_capacity(count);
    for index in 0..count {
        let (name, filename, content_type, value, is_file) = {
            let part = request.parts().nth(index).expect("valid part index");
            (
                part.name.to_vec(),
                part.filename.map(<[u8]>::to_vec),
                part.content_type.map(<[u8]>::to_vec),
                part.value.unwrap_or_default().to_vec(),
                part.file_bytes.is_some(),
            )
        };
        let value = if is_file {
            let mut output = Vec::new();
            let upload = request.upload_mut(index).expect("file part");
            assert_eq!(
                upload.copy_to(&mut output).expect("staged copy"),
                upload.len()
            );
            // Inject write failure at several data-dependent offsets, then
            // prove each failed copy leaves the staged source intact.
            // The fuzz limit keeps the extra I/O and allocations bounded.
            if !output.is_empty() {
                for budget in [0, 1, output.len() / 2, output.len() - 1] {
                    let mut sink = FailAfter {
                        bytes: Vec::new(),
                        budget,
                    };
                    let error = upload.copy_to(&mut sink).expect_err("injected ENOSPC");
                    assert_eq!(error.kind(), io::ErrorKind::StorageFull);
                    assert_eq!(sink.bytes, output[..budget]);
                    let mut retry = Vec::new();
                    assert_eq!(
                        upload.copy_to(&mut retry).expect("retry copy"),
                        upload.len()
                    );
                    assert_eq!(retry, output, "failed copy damaged staged source");
                }
            }
            output
        } else {
            value
        };
        parts.push((name, filename, content_type, value, is_file));
    }
    Snapshot {
        parts,
        stats: request.stats(),
    }
}

fn equivalent<T: Debug + Eq>(expected: &Result<T, Error>, actual: &Result<T, Error>) {
    match (expected, actual) {
        (Ok(lhs), Ok(rhs)) => assert_eq!(lhs, rhs, "chunk partition changed accepted request"),
        (Err(_), Err(_)) => {
            // Quota preflight may reject in a different order for large chunks.
        }
        _ => panic!("chunk partition changed acceptance: {expected:?} vs {actual:?}"),
    }
}

fn form_chunks(body: &[u8], chunk_size: usize) -> Result<Snapshot, Error> {
    let mut parser = FormParser::new(limits())?;
    for chunk in body.chunks(chunk_size) {
        parser.feed(chunk)?;
    }
    parser.finish().map(snapshot)
}

pub fn exercise_urlencoded(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let expected = form_chunks(data, data.len().max(1));
    for size in [1, 2, 7, 31, 257] {
        equivalent(&expected, &form_chunks(data, size));
    }
    let reader = urlencoded::parse_reader(&mut Cursor::new(data), limits()).map(snapshot);
    equivalent(&expected, &reader);
}

/// Fuzz the standalone Cookie scalar/array registration paths, including
/// PHP 8.4 input-variable and nesting INI quotas. Every invariant compares
/// the same complete raw header with independent API settings; malformed HTTP
/// field values are expected to fail closed, never to panic or publish JSON.
pub fn exercise_cookie(data: &[u8]) {
    if data.is_empty() || data.len() > MAX_INPUT + 1 {
        return;
    }
    // The leading byte steers policy combinations, leaving the rest as an
    // uninterpreted binary HTTP Cookie field value.
    let header = &data[1..];
    let limits = cookie::Limits {
        header_bytes: MAX_INPUT,
        pairs: 128,
        nodes: 512,
        name_bytes: 1024,
        value_bytes: 4096,
        retained_bytes: MAX_INPUT,
    };

    // At most 128 nonempty pairs can pass the independent wire quota.
    // A PHP variable quota of 128 must therefore be equivalent to no quota,
    // even with duplicate keys, malformed bracket paths and node limits.
    let scalar_default = cookie::parse_scalar_header(header, limits);
    assert_eq!(
        scalar_default,
        cookie::parse_scalar_header_with_php_max_input_vars(header, limits, 128)
    );
    let tree_default = cookie::array::parse_one_level_header(header, limits);
    assert_eq!(
        tree_default,
        cookie::array::parse_one_level_header_with_php_ini_limits(
            header, limits, Some(128), 64
        )
    );

    let variable_quota = match data[0] % 5 {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        _ => 128,
    };
    let nesting = [0, 1, 2, 64, 128][(data[0] as usize / 5) % 5];
    let scalar = cookie::parse_scalar_header_with_php_max_input_vars(
        header,
        limits,
        variable_quota,
    );
    assert_eq!(
        scalar,
        cookie::parse_scalar_header_with_php_max_input_vars(header, limits, variable_quota)
    );
    if let Ok(parsed) = scalar {
        let mut json = Vec::new();
        parsed.write_json(&mut json).expect("scalar JSON to Vec");
        assert!(json.starts_with(b"[") && json.ends_with(b"]"));
    }

    let tree = cookie::array::parse_one_level_header_with_php_ini_limits(
        header,
        limits,
        Some(variable_quota),
        nesting,
    );
    assert_eq!(
        tree,
        cookie::array::parse_one_level_header_with_php_ini_limits(
            header, limits, Some(variable_quota), nesting
        )
    );
    if let Ok(parsed) = tree {
        let mut json = Vec::new();
        parsed.write_json(&mut json).expect("array JSON to Vec");
        assert!(json.starts_with(b"[\"a\",[") && json.ends_with(b"]]"));
    }

    // With no admitted PHP variables, the nesting setting must have no
    // observable effect. The independent raw header/pair caps still apply.
    assert_eq!(
        cookie::array::parse_one_level_header_with_php_ini_limits(
            header, limits, Some(0), 0
        ),
        cookie::array::parse_one_level_header_with_php_ini_limits(
            header, limits, Some(0), 128
        )
    );
}

struct Spool(PathBuf);

impl Spool {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("rustpon-fuzz-{}-{nonce}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("private fuzz spool");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn assert_empty(&self) {
        assert_eq!(
            fs::read_dir(&self.0).expect("spool directory").count(),
            0,
            "staging file remained in private spool"
        );
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

fn multipart_chunks(
    body: &[u8],
    boundary: &[u8],
    chunk_size: usize,
    spool: &Spool,
) -> Result<Snapshot, Error> {
    let mut parser = Parser::new(boundary, limits(), spool.path())?;
    for chunk in body.chunks(chunk_size) {
        parser.feed(chunk)?;
    }
    parser.finish().map(snapshot)
}

pub fn exercise_multipart(data: &[u8]) {
    if data.is_empty() || data.len() > MAX_INPUT + 1 {
        return;
    }
    const BOUNDARIES: [&[u8]; 4] = [b"b", b"AaB03x", b"----rustpon", b"long-boundary-0123456789"];
    let boundary = BOUNDARIES[(data[0] % 4) as usize];
    let body = &data[1..];
    let spool = Spool::new();
    let expected = multipart_chunks(body, boundary, body.len().max(1), &spool);
    spool.assert_empty();
    for size in [1, 2, 7, 31, 257] {
        equivalent(&expected, &multipart_chunks(body, boundary, size, &spool));
        spool.assert_empty();
    }
    let reader = rustpon::parse_reader(&mut Cursor::new(body), boundary, limits(), spool.path())
        .map(snapshot);
    equivalent(&expected, &reader);
    spool.assert_empty();
}

/// Exercise the authorization boundary as well as the wire parser.
/// Admission consumes its Request; every path must close staged descriptors.
pub fn exercise_admission(data: &[u8]) {
    if data.is_empty() || data.len() > MAX_INPUT + 1 {
        return;
    }
    const BOUNDARIES: [&[u8]; 4] = [b"b", b"AaB03x", b"----rustpon", b"long-boundary-0123456789"];
    let boundary = BOUNDARIES[(data[0] % 4) as usize];
    let body = &data[1..];
    let spool = Spool::new();
    let Ok(mut raw) =
        rustpon::parse_reader(&mut Cursor::new(body), boundary, limits(), spool.path())
    else {
        spool.assert_empty();
        return;
    };
    let body_bytes = raw.stats().body_bytes;
    let mut originals = Vec::new();
    let part_count = raw.parts().len();
    for index in 0..part_count {
        let is_file = raw
            .parts()
            .nth(index)
            .expect("part index")
            .file_bytes
            .is_some();
        if is_file {
            let mut bytes = Vec::new();
            raw.upload_mut(index)
                .expect("staged upload")
                .copy_to(&mut bytes)
                .expect("raw copy");
            originals.push(Some(bytes));
        } else {
            originals.push(None);
        }
    }

    // Independently project PHP-shaped metadata before consuming raw.
    let projected = files::project(&raw, files::Limits::default()).map(|tree| {
        let mut json = Vec::new();
        tree.write_php_files_json(&mut json)
            .expect("projection JSON");
        json
    });
    match (
        projected,
        admission::admit(raw, admission::Policy::default()),
    ) {
        (Ok(expected_json), Ok(mut admitted)) => {
            let mut actual_json = Vec::new();
            admitted
                .metadata()
                .write_php_files_json(&mut actual_json)
                .expect("admitted JSON");
            assert_eq!(
                actual_json, expected_json,
                "admission changed FILES metadata"
            );

            let accepted: Vec<_> = admitted.accepted_indices().collect();
            for (index, original) in originals.iter().enumerate() {
                let mut copied = Vec::new();
                let result = admitted
                    .copy_accepted(index, &mut copied)
                    .expect("admitted copy");
                if accepted.contains(&index) {
                    let expected = original.as_ref().expect("accepted non-file");
                    assert_eq!(result, Some(expected.len() as u64));
                    assert_eq!(&copied, expected, "admission changed upload bytes");
                    let file = admitted.accepted_file(index).expect("typed accepted file");
                    assert_eq!(file.size, expected.len() as i64);
                    assert_eq!(file.error, 0);
                } else {
                    assert_eq!(result, None, "rejected upload became copyable");
                    assert!(admitted.accepted_file(index).is_none());
                    assert!(copied.is_empty());
                }
            }
            drop(admitted);
        }
        (Err(_), Err(_)) => {} // Invalid PHP metadata fails closed.
        (expected, actual) => {
            panic!("metadata/admission disagreement: {expected:?} vs {actual:?}")
        }
    }
    spool.assert_empty();

    // Strictly smaller server body quota must reject before exposing data.
    if body_bytes > 0 {
        let raw = rustpon::parse_reader(&mut Cursor::new(body), boundary, limits(), spool.path())
            .expect("repeat parse");
        assert!(matches!(
            admission::admit(
                raw,
                admission::Policy {
                    max_body_bytes: body_bytes - 1,
                    ..admission::Policy::default()
                }
            ),
            Err(admission::Error::BodyTooLarge)
        ));
        spool.assert_empty();
    }

    // Server upload size takes precedence over client MAX_FILE_SIZE.
    let largest = originals.iter().flatten().map(Vec::len).max().unwrap_or(0);
    if largest > 0 {
        let raw = rustpon::parse_reader(&mut Cursor::new(body), boundary, limits(), spool.path())
            .expect("repeat parse");
        assert!(matches!(
            admission::admit(
                raw,
                admission::Policy {
                    max_file_bytes: (largest - 1) as u64,
                    ..admission::Policy::default()
                }
            ),
            Err(admission::Error::FileTooLarge)
        ));
        spool.assert_empty();
    }

    // Aggregate server quota charges *all* staged uploads, even if PHP's
    // final FILES tree would overwrite or reject some of them.
    let total_file_bytes: usize = originals.iter().flatten().map(Vec::len).sum();
    if total_file_bytes > 0 {
        let raw = rustpon::parse_reader(&mut Cursor::new(body), boundary, limits(), spool.path())
            .expect("repeat parse");
        assert!(matches!(
            admission::admit(
                raw,
                admission::Policy {
                    max_total_file_bytes: (total_file_bytes - 1) as u64,
                    ..admission::Policy::default()
                }
            ),
            Err(admission::Error::FilesTooLarge)
        ));
        spool.assert_empty();
    }

    // Metadata quota failure cannot publish FILES or retain descriptors.
    if originals.iter().any(Option::is_some) {
        let raw = rustpon::parse_reader(&mut Cursor::new(body), boundary, limits(), spool.path())
            .expect("repeat parse");
        assert!(
            admission::admit(
                raw,
                admission::Policy {
                    metadata: files::Limits {
                        metadata_bytes: 0,
                        ..files::Limits::default()
                    },
                    ..admission::Policy::default()
                }
            )
            .is_err()
        );
        spool.assert_empty();
    }
}

/// Fuzz transport cancellation at every lifecycle phase: before any bytes,
/// during headers, after a staged upload, and after the closing delimiter.
/// A cancelled or failed request must stay terminal, and a fresh request
/// must remain independent of its predecessor.
pub fn exercise_lifecycle(data: &[u8]) {
    if data.is_empty() || data.len() > MAX_INPUT + 2 {
        return;
    }
    if data[0] % 2 == 0 {
        let body = &data[1..];
        if body.len() > MAX_INPUT {
            return;
        }
        let expected = form_chunks(body, body.len().max(1));
        for cut in [
            0,
            1.min(body.len()),
            body.len() / 2,
            body.len().saturating_sub(1),
            body.len(),
        ] {
            let mut parser = FormParser::new(limits()).expect("bounded form parser");
            match parser.feed(&body[..cut]) {
                Ok(()) => {
                    parser.cancel();
                    assert_eq!(parser.feed(&body[cut..]), Err(Error::Cancelled));
                    assert!(matches!(parser.finish(), Err(Error::Cancelled)));
                }
                Err(error) => {
                    assert_eq!(parser.feed(&body[cut..]), Err(error));
                    assert!(matches!(parser.finish(), Err(actual) if actual == error));
                }
            }
            equivalent(&expected, &form_chunks(body, body.len().max(1)));
        }
        return;
    }

    if data.len() < 2 {
        return;
    }
    const BOUNDARIES: [&[u8]; 4] = [b"b", b"AaB03x", b"----rustpon", b"long-boundary-0123456789"];
    let boundary = BOUNDARIES[(data[1] % 4) as usize];
    let body = &data[2..];
    let spool = Spool::new();
    let expected = multipart_chunks(body, boundary, body.len().max(1), &spool);
    spool.assert_empty();
    for cut in [
        0,
        1.min(body.len()),
        body.len() / 2,
        body.len().saturating_sub(1),
        body.len(),
    ] {
        let mut parser = Parser::new(boundary, limits(), spool.path()).expect("bounded multipart");
        match parser.feed(&body[..cut]) {
            Ok(()) => {
                parser.cancel();
                assert_eq!(parser.feed(&body[cut..]), Err(Error::Cancelled));
                assert!(matches!(parser.finish(), Err(Error::Cancelled)));
            }
            Err(error) => {
                assert_eq!(parser.feed(&body[cut..]), Err(error));
                assert!(matches!(parser.finish(), Err(actual) if actual == error));
            }
        }
        spool.assert_empty();
        equivalent(
            &expected,
            &multipart_chunks(body, boundary, body.len().max(1), &spool),
        );
        spool.assert_empty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_ini_quota_and_nesting_seed_smoke() {
        let deeply_nested = {
            let mut bytes = b"a=old; a".to_vec();
            for _ in 0..129 {
                bytes.extend_from_slice(b"[x]");
            }
            bytes.extend_from_slice(b"=skip; b=after");
            bytes
        };
        for header in [
            b"a[x]=%00%ff; a[x]=second; a[y]=%2b; b=tail".as_slice(),
            b"a[9223372036854775807]=max; a[]=skip; b=ok",
            b"a[x][y]=first; a[x][=second; a[x][z]=third",
            b"bad=\r\nmalformed",
            b"a=1; a=2; b=3",
            deeply_nested.as_slice(),
        ] {
            for selector in 0..25 {
                let mut input = vec![selector];
                input.extend_from_slice(header);
                exercise_cookie(&input);
            }
        }
    }

    #[test]
    fn urlencoded_seed_smoke() {
        for input in [
            b"a=1&b%5Bx%5D=%00%ff&c=hello+world".as_slice(),
            b"x=%&x=%2&x=%2B&empty=&=ignored".as_slice(),
            b"".as_slice(),
        ] {
            exercise_urlencoded(input);
        }
    }

    #[test]
    fn upload_copy_fault_offsets_across_buffer_boundaries() {
        let mut data =
            b"0--b\r\nContent-Disposition: form-data; name=f; filename=f.bin\r\n\r\n".to_vec();
        data.extend((0..4097).map(|i| (i % 256) as u8));
        data.extend_from_slice(b"\r\n--b--\r\n");
        assert!(data.len() <= MAX_INPUT + 1);
        exercise_multipart(&data);
    }

    #[test]
    fn admission_rejects_oversize_and_closes_overwritten_uploads() {
        for input in [
            b"0--b\r\nContent-Disposition: form-data; name=MAX_FILE_SIZE\r\n\r\n2\r\n--b\r\nContent-Disposition: form-data; name=f; filename=first.bin\r\n\r\nabc\r\n--b\r\nContent-Disposition: form-data; name=MAX_FILE_SIZE\r\n\r\n10\r\n--b\r\nContent-Disposition: form-data; name=f; filename=second.bin\r\n\r\n\0\xff\r\n--b--\r\n".as_slice(),
            b"0--b\r\nContent-Disposition: form-data; name=f[]; filename=zero.bin\r\n\r\n\r\n--b\r\nContent-Disposition: form-data; name=f[]; filename=two.bin\r\n\r\n\0\xff\r\n--b--\r\n".as_slice(),
            b"0--b\r\nContent-Disposition: form-data; name=f; filename=bad.bin\r\n\r\ntruncated".as_slice(),
        ] {
            exercise_admission(input);
        }
    }

    #[test]
    fn lifecycle_cancel_rejects_later_feeds_and_isolates_new_requests() {
        for input in [
            b"0a=1&b%5Bx%5D=%00%ff".as_slice(),
            b"0x=1&y=2&x=3".as_slice(),
            b"10--b\r\nContent-Disposition: form-data; name=f; filename=f.bin\r\n\r\nabc\0def\r\n--b--\r\n".as_slice(),
            b"10--b\r\nContent-Disposition: form-data; name=x\r\n\r\ntruncated".as_slice(),
            b"10--b--\r\n".as_slice(),
        ] {
            exercise_lifecycle(input);
        }
    }

    #[test]
    fn multipart_seed_smoke() {
        for input in [
            b"0--b\r\nContent-Disposition: form-data; name=x\r\n\r\nhello\r\n--b--\r\n".as_slice(),
            b"0--b\r\nContent-Disposition: form-data; name=f; filename=f.bin\r\nContent-Type: application/octet-stream\r\n\r\na\0b\r\n--b--\r\n".as_slice(),
            b"1--AaB03x\r\nContent-Disposition: form-data; name=x\r\n\r\ntruncated".as_slice(),
        ] {
            exercise_multipart(input);
        }
    }
}
