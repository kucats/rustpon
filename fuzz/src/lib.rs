//! Bounded libFuzzer invariants and CI seed tests for request parsers.
use rustpon::urlencoded::FormParser;
use rustpon::{Error, Limits, Parser, Request, Stats, urlencoded};
use std::fmt::Debug;
use std::fs;
use std::io::Cursor;
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

#[cfg(test)]
mod tests {
    use super::*;

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
