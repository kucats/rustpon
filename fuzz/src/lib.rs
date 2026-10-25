//! Bounded libFuzzer invariants and CI seed tests for request parsers.
use rustpon::urlencoded::FormParser;
use rustpon::{Error, Limits, Parser, Request, Stats, urlencoded};
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
                    assert_eq!(upload.copy_to(&mut retry).expect("retry copy"), upload.len());
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
    fn upload_copy_fault_offsets_across_buffer_boundaries() {
        let mut data = b"0--b\r\nContent-Disposition: form-data; name=f; filename=f.bin\r\n\r\n".to_vec();
        data.extend((0..4097).map(|i| (i % 256) as u8));
        data.extend_from_slice(b"\r\n--b--\r\n");
        assert!(data.len() <= MAX_INPUT + 1);
        exercise_multipart(&data);
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
