use super::*;
use std::io::{Cursor, Seek, SeekFrom};

struct Spool(PathBuf);

impl Spool {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "php-parser-test-{}-{}",
            std::process::id(),
            NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
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

use std::os::unix::fs::DirBuilderExt;

fn body(parts: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (header, value) in parts {
        bytes.extend_from_slice(b"--AaB03x\r\n");
        bytes.extend_from_slice(header);
        bytes.extend_from_slice(b"\r\n\r\n");
        bytes.extend_from_slice(value);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"--AaB03x--\r\n");
    bytes
}

const FIELD: &[u8] = b"Content-Disposition: form-data; name=field";
const FILE: &[u8] = b"Content-Disposition: form-data; name=file; filename=sample.bin\r\nContent-Type: application/octet-stream";

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    // All metadata, field bytes, upload bytes, counters and work measurements.
    parts: Vec<PartSnapshot>,
    stats: Stats,
}

type PartSnapshot = (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>, Vec<u8>, bool);

fn snapshot(mut request: Request) -> Snapshot {
    let mut parts = Vec::new();
    let count = request.parts().len();
    for index in 0..count {
        let view = request.parts().nth(index).unwrap();
        let name = view.name.to_vec();
        let filename = view.filename.map(<[u8]>::to_vec);
        let content_type = view.content_type.map(<[u8]>::to_vec);
        let is_file = view.file_bytes.is_some();
        let mut value = view.value.unwrap_or_default().to_vec();
        if is_file {
            let upload = request.upload_mut(index).unwrap();
            assert_eq!(upload.copy_to(&mut value).unwrap(), upload.len());
        }
        parts.push((name, filename, content_type, value, is_file));
    }
    Snapshot {
        parts,
        stats: request.stats(),
    }
}

fn parse(bytes: &[u8], limits: Limits, spool: &Spool) -> Result<Request, Error> {
    parse_reader(&mut Cursor::new(bytes), b"AaB03x", limits, &spool.0)
}

#[test]
fn every_split_and_random_partition_produces_identical_ir() {
    let spool = Spool::new();
    let bytes = body(&[
        (FIELD, b""),
        (FIELD, b"duplicate\0\xff"),
        (
            b"Content-Disposition: form-data; name=\"items[x][]\"",
            b"brackets",
        ),
        (
            b"Content-Disposition: form-data; name=f; filename=\"\"",
            b"",
        ),
        (FILE, b"binary\0\xff\r\n--AaB03xX!\r\n--AaB03\r\nend"),
    ]);
    let expected = snapshot(parse(&bytes, Limits::default(), &spool).unwrap());
    assert_eq!(expected.parts.len(), 5);
    assert_eq!(expected.parts[1].3, b"duplicate\0\xff");
    assert_eq!(expected.parts[2].0, b"items[x][]");
    assert_eq!(expected.stats.files, 2); // empty filename counts too
    for split in 0..=bytes.len() {
        let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
        parser.feed(&bytes[..split]).unwrap();
        parser.feed(&bytes[split..]).unwrap();
        assert_eq!(
            snapshot(parser.finish().unwrap()),
            expected,
            "split {split}"
        );
        spool.empty();
    }
    let mut seed = 0x1234_5678u64;
    for maximum in [1, 2, 3, 11, 127, bytes.len()] {
        for _ in 0..16 {
            let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
            let mut offset = 0;
            while offset < bytes.len() {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let end = (offset + 1 + seed as usize % maximum).min(bytes.len());
                parser.feed(&bytes[offset..end]).unwrap();
                offset = end;
            }
            assert_eq!(snapshot(parser.finish().unwrap()), expected);
        }
    }
}

#[test]
fn truncation_at_every_byte_and_trailing_data_fail_transactionally() {
    let spool = Spool::new();
    let bytes = body(&[(FIELD, b"complete"), (FILE, b"payload")]);
    for end in 0..bytes.len() {
        // Terminal CRLF is optional; EOF immediately after '--' is valid.
        if end == bytes.len() - 2 {
            continue;
        }
        assert!(
            parse(&bytes[..end], Limits::default(), &spool).is_err(),
            "end {end}"
        );
        spool.empty();
    }
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
    parser.feed(&bytes).unwrap();
    assert_eq!(parser.feed(b"epilogue"), Err(Error::Malformed));
    assert!(parser.request.is_none());
    assert!(parser.active.is_none());
    assert_eq!(parser.feed(&bytes), Err(Error::Malformed));
    assert!(matches!(parser.finish(), Err(Error::Malformed)));
    spool.empty();
}

#[test]
fn cancel_and_request_isolation() {
    let spool = Spool::new();
    let first = body(&[(FILE, b"first")]);
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
    parser.feed(&first).unwrap();
    parser.cancel();
    assert!(parser.request.is_none());
    assert_eq!(parser.feed(b""), Err(Error::Cancelled));
    drop(parser);
    let second = snapshot(parse(&body(&[(FIELD, b"second")]), Limits::default(), &spool).unwrap());
    assert_eq!(second.parts.len(), 1);
    assert_eq!(second.parts[0].3, b"second");
    assert_eq!(second.stats.files, 0);
    // Early disposal midway through an active upload follows the same RAII path.
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
    parser.feed(&first[..first.len() - 16]).unwrap();
    assert!(parser.active.is_some());
    spool.empty();
    drop(parser);
    spool.empty();
}

#[test]
fn exact_and_plus_one_for_every_input_quota() {
    let spool = Spool::new();
    type Case = (Limit, fn(&mut Limits, usize), usize);
    let bytes = body(&[
        (FIELD, b"abc"),
        (FILE, b"defg"),
        (
            b"Content-Disposition: form-data; name=x; filename=\"\"",
            b"",
        ),
    ]);
    let baseline = parse(&bytes, Limits::default(), &spool).unwrap();
    let stats = baseline.stats();
    let retained = baseline.arena.len();
    let largest_headers = FILE.len() + 4;
    let largest_line = FILE
        .split(|b| *b == b'\n')
        .map(|line| {
            if line.ends_with(b"\r") {
                line.len() + 1
            } else {
                line.len() + 2
            }
        })
        .max()
        .unwrap();
    let cases: &[Case] = &[
        (
            Limit::BodyBytes,
            |l, n| l.body_bytes = n as u64,
            bytes.len(),
        ),
        (Limit::Parts, |l, n| l.parts = n, stats.parts),
        (Limit::Fields, |l, n| l.fields = n, stats.fields),
        (Limit::Files, |l, n| l.files = n, stats.files),
        (
            Limit::HeaderLineBytes,
            |l, n| l.header_line_bytes = n,
            largest_line,
        ),
        (Limit::HeadersPerPart, |l, n| l.headers_per_part = n, 2),
        (
            Limit::HeadersTotal,
            |l, n| l.headers_total = n,
            stats.headers,
        ),
        (
            Limit::HeaderBytesPerPart,
            |l, n| l.header_bytes_per_part = n,
            largest_headers,
        ),
        (
            Limit::HeaderBytesTotal,
            |l, n| l.header_bytes_total = n,
            stats.header_bytes,
        ),
        (Limit::NameBytes, |l, n| l.name_bytes = n, 5),
        (Limit::FilenameBytes, |l, n| l.filename_bytes = n, 10),
        (Limit::FieldBytes, |l, n| l.field_bytes = n, 3),
        (Limit::FieldBytesTotal, |l, n| l.field_bytes_total = n, 3),
        (Limit::FileBytes, |l, n| l.file_bytes = n as u64, 4),
        (
            Limit::FileBytesTotal,
            |l, n| l.file_bytes_total = n as u64,
            4,
        ),
        (Limit::RetainedBytes, |l, n| l.retained_bytes = n, retained),
    ];
    drop(baseline);
    for &(limit, set, exact) in cases {
        let mut limits = Limits::default();
        set(&mut limits, exact);
        assert!(
            parse(&bytes, limits, &spool).is_ok(),
            "exact {limit:?}: {exact}"
        );
        set(&mut limits, exact - 1);
        assert_eq!(
            parse(&bytes, limits, &spool).unwrap_err(),
            Error::Limit(limit),
            "plus one {limit:?}"
        );
        spool.empty();
    }
    let limits = Limits {
        memory_capacity: stats.memory_capacity,
        ..Limits::default()
    };
    assert!(Parser::new(b"AaB03x", limits, &spool.0).is_ok());
    let limits = Limits {
        memory_capacity: stats.memory_capacity - 1,
        ..limits
    };
    assert!(matches!(
        Parser::new(b"AaB03x", limits, &spool.0),
        Err(Error::Limit(Limit::MemoryCapacity))
    ));
}

#[test]
fn cumulative_limits_include_empty_files_and_all_fields() {
    let spool = Spool::new();
    let bytes = body(&[(FILE, b"ab"), (FILE, b"cd")]);
    let limits = Limits {
        file_bytes_total: 3,
        ..Limits::default()
    };
    assert!(matches!(
        parse(&bytes, limits, &spool),
        Err(Error::Limit(Limit::FileBytesTotal))
    ));
    let bytes = body(&[(FIELD, b"ab"), (FIELD, b"cd")]);
    let limits = Limits {
        field_bytes_total: 3,
        ..Limits::default()
    };
    assert!(matches!(
        parse(&bytes, limits, &spool),
        Err(Error::Limit(Limit::FieldBytesTotal))
    ));
    let bytes = body(
        &[(
            b"Content-Disposition: form-data; name=f; filename=\"\"".as_slice(),
            b"".as_slice(),
        ); 3],
    );
    for (limits, quota) in [
        (
            Limits {
                files: 2,
                ..Limits::default()
            },
            Limit::Files,
        ),
        (
            Limits {
                parts: 2,
                ..Limits::default()
            },
            Limit::Parts,
        ),
        (
            Limits {
                headers_total: 2,
                ..Limits::default()
            },
            Limit::HeadersTotal,
        ),
    ] {
        assert!(
            matches!(parse(&bytes, limits, &spool), Err(Error::Limit(limit)) if limit == quota)
        );
        spool.empty();
    }
}

#[test]
fn strict_grammar_rejects_ambiguous_or_unsupported_headers() {
    let spool = Spool::new();
    let headers: &[&[u8]] = &[
        b"Content-Disposition: form-data; name=a; name=b",
        b"Content-Disposition: form-data; name=a; filename=x; filename=y",
        b"Content-Disposition: form-data; name=a\r\nContent-Disposition: form-data; name=b",
        b"Content-Disposition: form-data; name=a\r\nContent-Type: text/plain\r\nContent-Type: text/html",
        b"Content-Disposition: form-data; name=a\r\nContent-Transfer-Encoding: base64",
        b"Content-Disposition: form-data; name=a\r\nContent-Type: multipart/mixed",
        b"Content-Disposition: form-data; name=a; filename*=UTF-8''x",
        b"Content-Disposition: form-data; name=\"\"",
        b"Content-Disposition: form-data; name=\"unterminated",
        b"Content-Disposition: form-data; name=\"trailing\\",
        b"Content-Disposition: form-data; name=a\r\n folded: x",
        b"Content-Disposition : form-data; name=a",
        b"Content-Disposition: form-data; name=bad\0name",
        b"Content-Disposition: form-data; name=bad\rname",
        b"Content-Disposition: form-data; name=a\nOther: b",
        b"X-No-Disposition: a",
    ];
    for header in headers {
        assert!(
            parse(&body(&[(header, b"x")]), Limits::default(), &spool).is_err(),
            "{header:?}"
        );
    }
    let request = parse(
        &body(&[(
            b"content-disposition: FORM-DATA; NAME = \"a;\\\"b\"; FILENAME=\"C:\\\\x.bin\"",
            b"raw",
        )]),
        Limits::default(),
        &spool,
    )
    .unwrap();
    let view = request.parts().next().unwrap();
    assert_eq!(view.name, b"a;\"b");
    // PHP preserves quoted filename backslashes, including consecutive ones.
    assert_eq!(view.filename, Some(b"C:\\\\x.bin".as_slice()));
}

#[test]
fn php_84_upload_content_type_parameters_and_whitespace() {
    let spool = Spool::new();
    let cases: &[(&[u8], &[u8])] = &[
        (
            b"Content-Disposition: form-data; name=f; filename=f.bin\r\nContent-Type: text/plain; charset=UTF-8",
            b"text/plain",
        ),
        (
            b"Content-Disposition: form-data; name=f; filename=f.bin\r\nContent-Type: text/plain ; charset=\"utf-8\"",
            b"text/plain ",
        ),
        (
            b"Content-Disposition: form-data; name=f; filename=f.bin\r\nContent-Type: TEXT/PLAIN; invalid",
            b"TEXT/PLAIN",
        ),
        (
            b"Content-Disposition: form-data; name=f; filename=f.bin\r\nContent-Type: application/octet-stream",
            b"application/octet-stream",
        ),
    ];
    for &(header, expected) in cases {
        let request = parse(&body(&[(header, b"\0\xff")]), Limits::default(), &spool).unwrap();
        let view = request.parts().next().unwrap();
        assert_eq!(view.content_type, Some(expected));
        assert_eq!(view.file_bytes, Some(2));
        drop(request);
        spool.empty();
    }
    // Nested multipart remains unsupported, regardless of MIME parameters.
    let nested = b"Content-Disposition: form-data; name=f; filename=f.bin\r\nContent-Type: multipart/mixed; boundary=x";
    assert_eq!(
        parse(&body(&[(nested, b"x")]), Limits::default(), &spool).unwrap_err(),
        Error::Unsupported
    );
    spool.empty();
}

#[test]
fn boundary_and_configuration_validation() {
    let spool = Spool::new();
    for boundary in [b"".as_slice(), b"a b", b"a\r\nb", &[b'a'; 71]] {
        assert!(matches!(
            Parser::new(boundary, Limits::default(), &spool.0),
            Err(Error::InvalidBoundary)
        ));
    }
    let boundary = [b'a'; 70];
    let mut bytes = b"--".to_vec();
    bytes.extend_from_slice(&boundary);
    bytes.extend_from_slice(b"--");
    let mut parser = Parser::new(&boundary, Limits::default(), &spool.0).unwrap();
    parser.feed(&bytes).unwrap();
    assert_eq!(parser.finish().unwrap().parts().len(), 0);
    assert!(parse(b"preamble\r\n--AaB03x--", Limits::default(), &spool).is_err());
    for line_bytes in [0, 1, BLOCK + 1] {
        assert!(matches!(
            Parser::new(
                b"x",
                Limits {
                    header_line_bytes: line_bytes,
                    ..Limits::default()
                },
                &spool.0
            ),
            Err(Error::InvalidLimits)
        ));
    }
    assert!(matches!(
        Parser::new(
            b"x",
            Limits {
                parts: usize::MAX,
                ..Limits::default()
            },
            &spool.0
        ),
        Err(Error::InvalidLimits)
    ));
    // Fallible reservation capacity error, not a real system-OOM experiment.
    let limits = Limits {
        retained_bytes: isize::MAX as usize + 1,
        memory_capacity: usize::MAX,
        ..Limits::default()
    };
    assert!(matches!(
        Parser::new(b"x", limits, &spool.0),
        Err(Error::Allocation)
    ));
    fs::set_permissions(&spool.0, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        Parser::new(b"x", Limits::default(), &spool.0),
        Err(Error::InvalidSpoolDirectory)
    ));
    fs::set_permissions(&spool.0, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        add_u64(u64::MAX, 1, u64::MAX, Limit::BodyBytes),
        Err(Error::Limit(Limit::BodyBytes))
    );
    assert_eq!(
        add_usize(usize::MAX, 1, usize::MAX, Limit::Parts),
        Err(Error::Limit(Limit::Parts))
    );
}

#[test]
fn file_creation_write_and_reader_failures_discard_staging() {
    let spool = Spool::new();
    let prefix = b"--AaB03x\r\nContent-Disposition: form-data; name=old; filename=old\r\n\r\nold\r\n--AaB03x\r\n";
    for creation in [true, false] {
        let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
        parser.feed(prefix).unwrap();
        assert_eq!(parser.request.as_ref().unwrap().parts.len(), 1);
        parser.fail_create = creation;
        parser.fail_write = !creation;
        let tail =
            b"Content-Disposition: form-data; name=new; filename=new\r\n\r\nnew\r\n--AaB03x--";
        assert!(matches!(parser.feed(tail), Err(Error::Io(_))));
        assert!(parser.active.is_none());
        assert!(parser.request.is_none());
        spool.empty();
    }
    struct FailingReader {
        first: bool,
        bytes: Vec<u8>,
    }
    impl Read for FailingReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.first {
                self.first = false;
                out[..self.bytes.len()].copy_from_slice(&self.bytes);
                Ok(self.bytes.len())
            } else {
                Err(io::Error::from(io::ErrorKind::ConnectionReset))
            }
        }
    }
    let mut reader = FailingReader {
        first: true,
        bytes: prefix.to_vec(),
    };
    assert_eq!(
        parse_reader(&mut reader, b"AaB03x", Limits::default(), &spool.0).unwrap_err(),
        Error::Io(io::ErrorKind::ConnectionReset)
    );
    spool.empty();
    // Exercise a real filesystem create failure after constructor validation.
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
    fs::remove_dir(&spool.0).unwrap();
    assert!(matches!(
        parser.feed(&body(&[(FILE, b"x")])),
        Err(Error::Io(io::ErrorKind::NotFound))
    ));
    fs::DirBuilder::new().mode(0o700).create(&spool.0).unwrap();
}

#[test]
fn short_writes_and_write_errors_are_handled() {
    struct Short {
        bytes: Vec<u8>,
        fail_at: Option<usize>,
    }
    impl Write for Short {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_at == Some(self.bytes.len()) {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            self.bytes.push(bytes[0]);
            Ok(1)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Short {
        bytes: Vec::new(),
        fail_at: None,
    };
    write_chunk(&mut writer, b"abc").unwrap();
    assert_eq!(writer.bytes, b"abc");
    let mut writer = Short {
        bytes: Vec::new(),
        fail_at: Some(1),
    };
    assert_eq!(
        write_chunk(&mut writer, b"abc").unwrap_err().kind(),
        io::ErrorKind::StorageFull
    );
    assert_eq!(writer.bytes, b"a");
}

#[test]
fn overreported_upload_writer_fails_without_panicking_and_remains_retryable() {
    struct Overreport;
    impl Write for Overreport {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len() + 1)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    assert_eq!(
        write_chunk(&mut Overreport, b"abc").unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    let spool = Spool::new();
    let payload = b"binary\0\xffpayload";
    let mut request = parse(&body(&[(FILE, payload)]), Limits::default(), &spool).unwrap();
    let upload = request.upload_mut(0).unwrap();
    assert_eq!(
        upload.copy_to(&mut Overreport).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    let mut output = Vec::new();
    assert_eq!(upload.copy_to(&mut output).unwrap(), payload.len() as u64);
    assert_eq!(output, payload);
    drop(request);
    spool.empty();
}

#[test]
fn bounded_interrupted_and_zero_write_failures() {
    struct InterruptedWriter {
        calls: usize,
        remaining_interrupts: usize,
        output: Vec<u8>,
    }
    impl Write for InterruptedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.remaining_interrupts > 0 {
                self.remaining_interrupts -= 1;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = bytes.len().min(3);
            self.output.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut recovering = InterruptedWriter {
        calls: 0,
        remaining_interrupts: MAX_CONSECUTIVE_INTERRUPTS - 1,
        output: Vec::new(),
    };
    write_chunk(&mut recovering, b"hello").unwrap();
    assert_eq!(recovering.output, b"hello");

    let mut stalled = InterruptedWriter {
        calls: 0,
        remaining_interrupts: MAX_CONSECUTIVE_INTERRUPTS,
        output: Vec::new(),
    };
    assert_eq!(
        write_chunk(&mut stalled, b"hello").unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert_eq!(stalled.calls, MAX_CONSECUTIVE_INTERRUPTS);
    assert!(stalled.output.is_empty());

    struct ZeroWriter;
    impl Write for ZeroWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert_eq!(
        write_chunk(&mut ZeroWriter, b"hello").unwrap_err().kind(),
        io::ErrorKind::WriteZero
    );
    write_chunk(&mut ZeroWriter, b"").unwrap();
}

#[test]
fn upload_copy_fault_injection_preserves_retry_and_bytes() {
    struct FaultWriter {
        bytes: Vec<u8>,
        fail_after: usize,
        error: io::ErrorKind,
        interrupted_once: bool,
    }
    impl Write for FaultWriter {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            if !self.interrupted_once {
                self.interrupted_once = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self.bytes.len() >= self.fail_after {
                return Err(self.error.into());
            }
            let count = input.len().min(7).min(self.fail_after - self.bytes.len());
            self.bytes.extend_from_slice(&input[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let spool = Spool::new();
    // Cross the internal 8192-byte copy buffer and include NUL and high bytes.
    let payload: Vec<u8> = (0..(BLOCK * 2 + 17))
        .map(|index| (index.wrapping_mul(37) % 256) as u8)
        .collect();
    let wire = body(&[(FILE, &payload)]);
    let mut request = parse(&wire, Limits::default(), &spool).unwrap();
    let upload = request.upload_mut(0).unwrap();
    assert_eq!(upload.len(), payload.len() as u64);

    let mut failed = FaultWriter {
        bytes: Vec::new(),
        fail_after: BLOCK + 3,
        error: io::ErrorKind::StorageFull,
        interrupted_once: false,
    };
    assert_eq!(
        upload.copy_to(&mut failed).unwrap_err().kind(),
        io::ErrorKind::StorageFull
    );
    assert_eq!(failed.bytes, payload[..BLOCK + 3]);
    let mut recovered = Vec::new();
    assert_eq!(
        upload.copy_to(&mut recovered).unwrap(),
        payload.len() as u64
    );
    assert_eq!(recovered, payload);

    let mut interrupted = FaultWriter {
        bytes: Vec::new(),
        fail_after: usize::MAX,
        error: io::ErrorKind::Other,
        interrupted_once: false,
    };
    assert_eq!(
        upload.copy_to(&mut interrupted).unwrap(),
        payload.len() as u64
    );
    assert_eq!(interrupted.bytes, payload);

    struct Stall {
        calls: usize,
    }
    impl Write for Stall {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            Err(io::ErrorKind::Interrupted.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut stall = Stall { calls: 0 };
    assert_eq!(
        upload.copy_to(&mut stall).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert_eq!(stall.calls, MAX_CONSECUTIVE_INTERRUPTS);
    let mut zero = Vec::new();
    assert_eq!(upload.copy_to(&mut zero).unwrap(), payload.len() as u64);
    assert_eq!(zero, payload);
    drop(request);
    spool.empty();
}

#[test]
fn closing_candidates_have_explicit_strict_policy() {
    let spool = Spool::new();
    let accepted = body(&[(FILE, b"a\r\n--AaB03xX!b")]);
    assert_eq!(
        snapshot(parse(&accepted, Limits::default(), &spool).unwrap()).parts[0].3,
        b"a\r\n--AaB03xX!b"
    );
    // '--' commits a closing delimiter. Anything except EOF or final CRLF
    // after it is rejected; it is never reinterpreted as payload/epilogue.
    let rejected = body(&[(FILE, b"a\r\n--AaB03x--X!b")]);
    for split in 0..=rejected.len() {
        let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
        let result = parser
            .feed(&rejected[..split])
            .and_then(|()| parser.feed(&rejected[split..]));
        assert_eq!(result, Err(Error::Malformed));
        assert!(parser.request.is_none());
    }
}

#[test]
fn descriptor_lifecycle_in_isolated_subprocess() {
    const CHILD: &str = "PHP_PARSER_FD_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::descriptor_lifecycle_in_isolated_subprocess",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let fd_directory = if Path::new("/proc/self/fd").is_dir() {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let count = || fs::read_dir(fd_directory).unwrap().count();
    let spool = Spool::new();
    let baseline = count();
    let bytes = body(&[(FILE, b"one"), (FILE, b"two")]);
    for _ in 0..128 {
        let request = parse(&bytes, Limits::default(), &spool).unwrap();
        assert_eq!(count(), baseline + 2);
        drop(request);
        assert_eq!(count(), baseline);
        assert!(parse(&bytes[..bytes.len() - 8], Limits::default(), &spool).is_err());
        assert_eq!(count(), baseline);
        let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
        parser.feed(&bytes).unwrap();
        assert_eq!(count(), baseline + 2);
        parser.cancel();
        assert_eq!(count(), baseline);
        drop(parser);
        // Active upload plus an earlier completed part, both staged.
        let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
        parser.feed(&bytes[..bytes.len() - 16]).unwrap();
        assert_eq!(count(), baseline + 2);
        parser.fail_write = true;
        assert!(parser.feed(&bytes[bytes.len() - 16..]).is_err());
        assert_eq!(count(), baseline);
    }

    // A faulty Read implementation may overreport its byte count after a
    // complete multipart body has already staged a live upload descriptor.
    // The adapter must return InvalidData, not panic or leak the descriptor.
    struct OverreportAfterBody<'a> {
        body: Option<&'a [u8]>,
    }
    impl Read for OverreportAfterBody<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if let Some(body) = self.body.take() {
                buffer[..body.len()].copy_from_slice(body);
                Ok(body.len())
            } else {
                Ok(buffer.len() + 1)
            }
        }
    }
    let mut reader = OverreportAfterBody { body: Some(&bytes) };
    assert_eq!(
        parse_reader(&mut reader, b"AaB03x", Limits::default(), &spool.0).unwrap_err(),
        Error::Io(io::ErrorKind::InvalidData)
    );
    assert_eq!(count(), baseline);
    spool.empty();
}

#[test]
fn streaming_large_upload_keeps_fixed_capacity_and_bounded_work() {
    let spool = Spool::new();
    let limits = Limits {
        retained_bytes: 1024,
        memory_capacity: 64 * 1024,
        parts: 2,
        ..Limits::default()
    };
    let mut parser = Parser::new(b"AaB03x", limits, &spool.0).unwrap();
    parser
        .feed(b"--AaB03x\r\nContent-Disposition: form-data; name=f; filename=x\r\n\r\n")
        .unwrap();
    let chunk = b"\r\n--AaB03xX!".repeat(683);
    let repeats = 1024;
    let before = parser.request.as_ref().unwrap().stats.memory_capacity;
    for _ in 0..repeats {
        parser.feed(&chunk).unwrap();
    }
    parser.feed(b"\r\n--AaB03x--\r\n").unwrap();
    let mut request = parser.finish().unwrap();
    assert_eq!(request.stats().memory_capacity, before);
    assert!(before < 64 * 1024);
    assert_eq!(request.stats().file_bytes, (chunk.len() * repeats) as u64);
    assert!(request.stats().boundary_comparisons <= request.stats().body_bytes * 10);
    assert!(request.arena.len() < 10);
    let mut sink = io::sink();
    assert_eq!(
        request.upload_mut(0).unwrap().copy_to(&mut sink).unwrap(),
        (chunk.len() * repeats) as u64
    );
    spool.empty();
    drop(request);
    spool.empty();
}

#[test]
fn deterministic_mutation_and_arbitrary_bytes_smoke() {
    let spool = Spool::new();
    let seed = body(&[(FIELD, b"abc"), (FILE, b"binary\0\xff")]);
    let limits = Limits {
        retained_bytes: 4096,
        parts: 8,
        memory_capacity: 64 * 1024,
        body_bytes: 4096,
        ..Limits::default()
    };
    for index in 0..seed.len() {
        for value in [0, b'\r', b'\n', b'-', b'"', 255] {
            let mut bytes = seed.clone();
            bytes[index] = value;
            let whole = parse(&bytes, limits, &spool).map(snapshot);
            let mut parser = Parser::new(b"AaB03x", limits, &spool.0).unwrap();
            let mut error = None;
            for byte in &bytes {
                if let Err(e) = parser.feed(&[*byte]) {
                    error = Some(e);
                    break;
                }
            }
            let split = match error {
                Some(e) => Err(e),
                None => parser.finish().map(snapshot),
            };
            assert_eq!(whole, split);
            spool.empty();
        }
    }
    let mut state = 7u64;
    for size in 0..512 {
        let bytes: Vec<u8> = (0..size)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                (state >> 32) as u8
            })
            .collect();
        let _ = parse(&bytes, limits, &spool);
        spool.empty();
    }
}

#[test]
fn delimiter_comparison_quota_is_exact_and_fails_transactionally() {
    let spool = Spool::new();
    let bytes = body(&[
        (FIELD, b"some bytes"),
        (FILE, b"binary\r\n--AaB03xX! value"),
    ]);
    let baseline = parse(&bytes, Limits::default(), &spool).unwrap();
    let comparisons = baseline.stats().boundary_comparisons;
    assert!(comparisons > 0);
    drop(baseline);
    let limits = Limits {
        boundary_comparisons: comparisons,
        ..Limits::default()
    };
    assert!(parse(&bytes, limits, &spool).is_ok());
    assert_eq!(
        parse(
            &bytes,
            Limits {
                boundary_comparisons: comparisons - 1,
                ..limits
            },
            &spool
        )
        .unwrap_err(),
        Error::Limit(Limit::BoundaryComparisons)
    );
    spool.empty();
}

#[test]
fn adversarial_near_boundary_patterns_exhaust_cpu_budget_and_release_uploads() {
    let spool = Spool::new();
    let boundary = [b'a'; 70];
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"--");
    bytes.extend_from_slice(&boundary);
    bytes.extend_from_slice(b"\r\nContent-Disposition: form-data; name=f; filename=x\r\n\r\n");
    // Every prefix almost matches the maximum-length boundary. The required
    // suffix does not match, so this remains file data rather than framing.
    for _ in 0..80 {
        bytes.extend_from_slice(b"\r\n--");
        bytes.extend_from_slice(&boundary[..69]);
        bytes.push(b'X');
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"\r\n--");
    bytes.extend_from_slice(&boundary);
    bytes.extend_from_slice(b"--\r\n");

    let limits = Limits {
        boundary_comparisons: 200,
        ..Limits::default()
    };
    let mut parser = Parser::new(&boundary, limits, &spool.0).unwrap();
    assert_eq!(
        parser.feed(&bytes),
        Err(Error::Limit(Limit::BoundaryComparisons))
    );
    assert!(parser.request.is_none());
    assert!(parser.active.is_none());
    assert_eq!(
        parser.feed(b""),
        Err(Error::Limit(Limit::BoundaryComparisons))
    );
    assert!(matches!(
        parser.finish(),
        Err(Error::Limit(Limit::BoundaryComparisons))
    ));
    spool.empty();
    // A fresh request gets a fresh work budget and cannot observe the failed
    // request's field data or upload descriptors.
    let second = parse(&body(&[(FIELD, b"next")]), Limits::default(), &spool).unwrap();
    assert_eq!(
        second.parts().next().unwrap().value,
        Some(b"next".as_slice())
    );
}

#[test]
fn interrupted_reader_has_finite_budget_and_releases_uploads() {
    struct InterruptReader<'a> {
        bytes: &'a [u8],
        offset: usize,
        interrupt_next: bool,
        stall_after: Option<usize>,
    }
    impl io::Read for InterruptReader<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.stall_after.is_some_and(|cutoff| self.offset >= cutoff) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self.interrupt_next {
                self.interrupt_next = false;
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self.offset == self.bytes.len() {
                return Ok(0);
            }
            let limit = self.stall_after.unwrap_or(self.bytes.len());
            let end = (self.offset + 11).min(self.bytes.len()).min(limit);
            let count = end - self.offset;
            output[..count].copy_from_slice(&self.bytes[self.offset..end]);
            self.offset = end;
            self.interrupt_next = true;
            Ok(count)
        }
    }

    let spool = Spool::new();
    let bytes = body(&[(FILE, b"uploaded-binary\0\xff"), (FIELD, b"after-upload")]);
    let expected = snapshot(parse(&bytes, Limits::default(), &spool).unwrap());
    let mut recovering = InterruptReader {
        bytes: &bytes,
        offset: 0,
        interrupt_next: true,
        stall_after: None,
    };
    assert_eq!(
        snapshot(parse_reader(&mut recovering, b"AaB03x", Limits::default(), &spool.0).unwrap()),
        expected
    );
    spool.empty();

    // The reader stalls after staging a file, before transport EOF. No
    // request is published; parser ownership must release its upload FDs.
    let mut stalled = InterruptReader {
        bytes: &bytes,
        offset: 0,
        interrupt_next: false,
        stall_after: Some(bytes.len() - 12),
    };
    assert_eq!(
        parse_reader(&mut stalled, b"AaB03x", Limits::default(), &spool.0).unwrap_err(),
        Error::Io(io::ErrorKind::Interrupted)
    );
    spool.empty();
    assert_eq!(
        snapshot(parse(&bytes, Limits::default(), &spool).unwrap()),
        expected
    );
    spool.empty();
}

#[test]
fn php_upload_quota_drains_oversized_parts_without_retaining_staged_bytes() {
    let spool = Spool::new();
    let large = vec![b'X'; BLOCK * 2 + 5];
    let input = body(&[
        (FILE, &large),
        (
            b"Content-Disposition: form-data; name=next; filename=ok.bin",
            b"ok",
        ),
    ]);
    for quota in [
        None,
        Some(0),
        Some(large.len() as u64),
        Some(large.len() as u64 - 1),
        Some(BLOCK as u64),
    ] {
        for split in [0, 1, BLOCK, input.len()] {
            let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0)
                .unwrap()
                .with_php_upload_max_bytes(quota);
            parser.feed(&input[..split]).unwrap();
            parser.feed(&input[split..]).unwrap();
            let mut request = parser.finish().unwrap();
            let rejected = quota.is_some_and(|max| max != 0 && large.len() as u64 > max);
            assert_eq!(
                request.parts().next().unwrap().file_bytes,
                Some(large.len() as u64)
            );
            let upload = request.upload_mut(0).unwrap();
            assert_eq!(upload.len(), large.len() as u64);
            assert_eq!(upload.discarded, rejected);
            assert_eq!(
                upload.file.as_ref().unwrap().metadata().unwrap().len(),
                if rejected { 0 } else { large.len() as u64 }
            );
            let mut output = Vec::new();
            if rejected {
                assert_eq!(
                    upload.copy_to(&mut output).unwrap_err().kind(),
                    io::ErrorKind::InvalidData
                );
                assert!(output.is_empty());
            } else {
                upload.copy_to(&mut output).unwrap();
                assert_eq!(output, large);
            }
            let mut small = Vec::new();
            request.upload_mut(1).unwrap().copy_to(&mut small).unwrap();
            assert_eq!(small, b"ok");
            let projected = crate::files::project(
                &request,
                crate::files::Limits {
                    max_upload_bytes: quota,
                    ..crate::files::Limits::default()
                },
            )
            .unwrap();
            let expected_error = if rejected { b"1" } else { b"0" };
            assert!(projected.wire().parts().any(|part| {
                part.name == b"file[error]" && part.value == Some(expected_error.as_slice())
            }));
            drop(projected);
            drop(request);
            spool.empty();
        }
    }
}

#[test]
fn php_post_streaming_quota_drains_and_releases_staged_uploads() {
    struct Chunked<'a> {
        bytes: &'a [u8],
        offset: usize,
        chunk: usize,
    }
    impl Read for Chunked<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let remaining = &self.bytes[self.offset..];
            let count = remaining.len().min(self.chunk).min(output.len());
            output[..count].copy_from_slice(&remaining[..count]);
            self.offset += count;
            Ok(count)
        }
    }

    let spool = Spool::new();
    let large = vec![b'X'; BLOCK * 2 + 5];
    let wire = body(&[(FILE, &large), (FIELD, b"after")]);
    let threshold = (wire.len() / 2) as u64;
    for chunk in [1, 17, BLOCK, wire.len()] {
        let mut input = Chunked {
            bytes: &wire,
            offset: 0,
            chunk,
        };
        let request = parse_reader_with_php_limits(
            &mut input,
            b"AaB03x",
            Limits::default(),
            &spool.0,
            None,
            Some(threshold),
        )
        .unwrap();
        assert_eq!(input.offset, wire.len());
        assert_eq!(request.parts().len(), 0);
        assert_eq!(request.stats().body_bytes, wire.len() as u64);
        drop(request);
        spool.empty();
    }

    // Exact size, unlimited zero, and absent PHP quotas all preserve data.
    for quota in [None, Some(0), Some(wire.len() as u64)] {
        let request = parse_reader_with_php_limits(
            &mut Cursor::new(&wire),
            b"AaB03x",
            Limits::default(),
            &spool.0,
            None,
            quota,
        )
        .unwrap();
        assert_eq!(request.parts().len(), 2);
        drop(request);
        spool.empty();
    }

    // PHP compatibility must never override the server's independent cap.
    let hard = Limits {
        body_bytes: wire.len() as u64 - 1,
        ..Limits::default()
    };
    assert_eq!(
        parse_reader_with_php_limits(
            &mut Cursor::new(&wire),
            b"AaB03x",
            hard,
            &spool.0,
            None,
            Some(1),
        )
        .unwrap_err(),
        Error::Limit(Limit::BodyBytes)
    );
    spool.empty();
}

#[test]
fn php_post_streaming_quota_retains_strict_errors_until_exceeded() {
    let spool = Spool::new();
    let wire = body(&[(FILE, b"abc")]);
    let truncated = &wire[..wire.len() - 6];
    assert_eq!(
        parse_reader_with_php_limits(
            &mut Cursor::new(truncated),
            b"AaB03x",
            Limits::default(),
            &spool.0,
            None,
            Some(wire.len() as u64),
        )
        .unwrap_err(),
        Error::Truncated
    );
    spool.empty();

    // Once the PHP quota is crossed, malformed trailing bytes are drained
    // rather than interpreted as multipart framing.
    let mut malformed = wire.clone();
    malformed.extend_from_slice(b"not-valid-epilogue");
    let request = parse_reader_with_php_limits(
        &mut Cursor::new(&malformed),
        b"AaB03x",
        Limits::default(),
        &spool.0,
        None,
        Some(wire.len() as u64),
    )
    .unwrap();
    assert_eq!(request.parts().len(), 0);
    assert_eq!(request.stats().body_bytes, malformed.len() as u64);
    drop(request);
    spool.empty();
}

#[test]
fn php_content_length_preflight_skips_malformed_oversized_multipart() {
    let spool = Spool::new();
    let malformed = b"not even a multipart body";
    for quota in [1, (malformed.len() - 1) as u64] {
        let mut reader = Cursor::new(malformed);
        let request = parse_reader_with_php_content_length(
            &mut reader,
            b"AaB03x",
            Limits::default(),
            &spool.0,
            None,
            Some(quota),
            malformed.len() as u64,
        )
        .unwrap();
        assert_eq!(request.parts().len(), 0);
        assert_eq!(request.stats().body_bytes, malformed.len() as u64);
        assert_eq!(reader.position(), malformed.len() as u64);
        drop(request);
        spool.empty();
    }

    // Preflight is based on declared bytes, not on when a read chunk crosses
    // the quota. An oversized upload is never opened, even if framing is bad.
    let wire = body(&[(FILE, b"payload")]);
    let mut reader = Cursor::new(&wire);
    let request = parse_reader_with_php_content_length(
        &mut reader,
        b"AaB03x",
        Limits::default(),
        &spool.0,
        Some(2),
        Some(1),
        wire.len() as u64,
    )
    .unwrap();
    assert_eq!(request.parts().len(), 0);
    drop(request);
    spool.empty();
}

#[test]
fn php_content_length_exact_boundary_and_transport_isolation() {
    let spool = Spool::new();
    let wire = body(&[(FILE, b"abc"), (FIELD, b"ok")]);
    for php_limit in [None, Some(0), Some(wire.len() as u64)] {
        let mut concatenated = wire.clone();
        concatenated.extend_from_slice(b"NEXT-HTTP-REQUEST");
        let mut reader = Cursor::new(&concatenated);
        let request = parse_reader_with_php_content_length(
            &mut reader,
            b"AaB03x",
            Limits::default(),
            &spool.0,
            None,
            php_limit,
            wire.len() as u64,
        )
        .unwrap();
        assert_eq!(request.parts().len(), 2);
        assert_eq!(reader.position(), wire.len() as u64);
        drop(request);
        spool.empty();
    }

    // A declared empty HTTP body is an empty POST/FILES publication. The
    // following bytes are not consumed as part of this request.
    let mut reader = Cursor::new(&wire);
    let empty = parse_reader_with_php_content_length(
        &mut reader,
        b"AaB03x",
        Limits::default(),
        &spool.0,
        None,
        Some(1),
        0,
    )
    .unwrap();
    assert_eq!(empty.parts().len(), 0);
    assert_eq!(empty.stats().body_bytes, 0);
    assert_eq!(reader.position(), 0);
    drop(empty);
    spool.empty();

    // The declared body must arrive in full, even when PHP will discard it.
    for php_limit in [Some(1), None] {
        let truncated = &wire[..wire.len() - 1];
        assert_eq!(
            parse_reader_with_php_content_length(
                &mut Cursor::new(truncated),
                b"AaB03x",
                Limits::default(),
                &spool.0,
                None,
                php_limit,
                wire.len() as u64,
            )
            .unwrap_err(),
            Error::Truncated
        );
        spool.empty();
    }

    // A smaller Content-Length must not consume the next message.
    let mut reader = Cursor::new(&wire);
    assert_eq!(
        parse_reader_with_php_content_length(
            &mut reader,
            b"AaB03x",
            Limits::default(),
            &spool.0,
            None,
            None,
            (wire.len() - 1) as u64,
        )
        .unwrap_err(),
        Error::Truncated
    );
    assert_eq!(reader.position(), (wire.len() - 1) as u64);
    spool.empty();

    // The independent hard limit is enforced before any read or staging.
    let mut reader = Cursor::new(&wire);
    let hard = Limits {
        body_bytes: wire.len() as u64 - 1,
        ..Limits::default()
    };
    assert_eq!(
        parse_reader_with_php_content_length(
            &mut reader,
            b"AaB03x",
            hard,
            &spool.0,
            None,
            Some(1),
            wire.len() as u64,
        )
        .unwrap_err(),
        Error::Limit(Limit::BodyBytes)
    );
    assert_eq!(reader.position(), 0);
    spool.empty();
}

#[test]
fn php_content_length_preflight_read_errors_fail_closed() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::ConnectionReset.into())
        }
    }
    let spool = Spool::new();
    assert_eq!(
        parse_reader_with_php_content_length(
            &mut Broken,
            b"AaB03x",
            Limits::default(),
            &spool.0,
            None,
            Some(1),
            100,
        )
        .unwrap_err(),
        Error::Io(io::ErrorKind::ConnectionReset)
    );
    spool.empty();
}

#[test]
fn php_partial_eof_publishes_unfinished_fields_with_bounded_lookahead() {
    let spool = Spool::new();
    let prefix = b"--AaB03x\r\nContent-Disposition: form-data; name=tail\r\n\r\n";
    for (tail, expected) in [
        (b"unfinished".as_slice(), b"unfinished".as_slice()),
        (b"unfinished\r", b"unfinished\r"),
        (b"unfinished\r\n", b"unfinished"),
        (b"unfinished\r\n--AaB03x", b"unfinished"),
        (b"unfinished\r\nother", b"unfinished\r\nother"),
        (b"".as_slice(), b"".as_slice()),
    ] {
        let mut wire = prefix.to_vec();
        wire.extend_from_slice(tail);
        assert_eq!(
            parse(&wire, Limits::default(), &spool).unwrap_err(),
            Error::Truncated
        );
        for split in 0..=wire.len() {
            let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
            parser.feed(&wire[..split]).unwrap();
            parser.feed(&wire[split..]).unwrap();
            let request = parser.finish_with_php_partial().unwrap();
            let fields: Vec<_> = request.parts().collect();
            assert_eq!(fields.len(), 1, "split {split}");
            assert_eq!(fields[0].name, b"tail");
            assert_eq!(fields[0].value, Some(expected), "split {split}");
            drop(request);
            spool.empty();
        }
    }
    let wire = [prefix.as_slice(), b"abcdef"].concat();
    assert_eq!(
        parse_reader_with_php_partial(
            &mut Cursor::new(&wire),
            b"AaB03x",
            Limits {
                field_bytes: 2,
                ..Limits::default()
            },
            &spool.0,
            None,
            None,
        )
        .unwrap_err(),
        Error::Limit(Limit::FieldBytes)
    );
    spool.empty();
}

#[test]
fn php_partial_eof_field_header_preserves_completed_file() {
    let spool = Spool::new();
    let full = body(&[
        (FILE, b"binary"),
        (b"Content-Disposition: form-data; name=tail", b"ignored"),
    ]);
    let marker = b"Content-Disposition: form-data; name=tail\r\n";
    let end = full
        .windows(marker.len())
        .position(|w| w == marker)
        .unwrap()
        + marker.len();
    let wire = &full[..end];
    assert_eq!(
        parse(wire, Limits::default(), &spool).unwrap_err(),
        Error::Truncated
    );
    for split in 0..=wire.len() {
        let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
        parser.feed(&wire[..split]).unwrap();
        parser.feed(&wire[split..]).unwrap();
        let mut request = parser.finish_with_php_partial().unwrap();
        let parts: Vec<_> = request.parts().collect();
        assert_eq!(parts.len(), 2, "split {split}");
        assert_eq!(parts[0].file_bytes, Some(6));
        assert_eq!(parts[1].name, b"tail");
        assert_eq!(parts[1].value, Some(b"".as_slice()));
        let mut copied = Vec::new();
        request.upload_mut(0).unwrap().copy_to(&mut copied).unwrap();
        assert_eq!(copied, b"binary");
        drop(request);
        spool.empty();
    }
    // A truncated disposition line has not established a field name.
    assert_eq!(
        parse_reader_with_php_partial(
            &mut Cursor::new(&wire[..wire.len() - 2]),
            b"AaB03x",
            Limits::default(),
            &spool.0,
            None,
            None,
        )
        .unwrap_err(),
        Error::Truncated
    );
    spool.empty();
}

#[test]
fn php_partial_eof_registers_error_three_and_erases_staged_upload() {
    let spool = Spool::new();
    let full = body(&[(FIELD, b"kept"), (FILE, b"unfinished-file-payload")]);
    let suffix = b"\r\n--AaB03x--\r\n";
    assert!(full.ends_with(suffix));
    let partial = &full[..full.len() - suffix.len()];
    assert_eq!(
        parse(partial, Limits::default(), &spool).unwrap_err(),
        Error::Truncated
    );
    spool.empty();
    for split in [0, 1, partial.len() / 2, partial.len()] {
        let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
        parser.feed(&partial[..split]).unwrap();
        parser.feed(&partial[split..]).unwrap();
        let mut raw = parser.finish_with_php_partial().unwrap();
        assert_eq!(raw.parts().len(), 2);
        assert_eq!(raw.parts().next().unwrap().value, Some(b"kept".as_slice()));
        let projected = files::project(&raw, files::Limits::default()).unwrap();
        for (name, value) in [
            (b"file[error]".as_slice(), b"3".as_slice()),
            (b"file[size]", b"0"),
            (b"file[type]", b""),
            (b"file[tmp_name]", b""),
        ] {
            assert!(
                projected
                    .wire()
                    .parts()
                    .any(|p| p.name == name && p.value == Some(value))
            );
        }
        let mut output = Vec::new();
        assert_eq!(
            raw.upload_mut(1)
                .unwrap()
                .copy_to(&mut output)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(output.is_empty());
        drop(projected);
        let admitted = admission::admit(raw, admission::Policy::default()).unwrap();
        assert_eq!(admitted.accepted_indices().count(), 0);
        drop(admitted);
        spool.empty();
    }
}

#[test]
fn php_partial_eof_obeys_upload_quota_and_independent_hard_limit() {
    let spool = Spool::new();
    let full = body(&[(FILE, b"abcdef")]);
    let suffix = b"\r\n--AaB03x--\r\n";
    let partial = &full[..full.len() - suffix.len()];
    for (quota, expected) in [(None, b"3".as_slice()), (Some(2), b"1".as_slice())] {
        let raw = parse_reader_with_php_partial(
            &mut Cursor::new(partial),
            b"AaB03x",
            Limits::default(),
            &spool.0,
            quota,
            None,
        )
        .unwrap();
        let metadata = files::project(
            &raw,
            files::Limits {
                max_upload_bytes: quota,
                ..files::Limits::default()
            },
        )
        .unwrap();
        assert!(
            metadata
                .wire()
                .parts()
                .any(|p| { p.name == b"file[error]" && p.value == Some(expected) })
        );
        drop(metadata);
        drop(raw);
        spool.empty();
    }
    let hard = Limits {
        file_bytes: 2,
        ..Limits::default()
    };
    assert_eq!(
        parse_reader_with_php_partial(
            &mut Cursor::new(partial),
            b"AaB03x",
            hard,
            &spool.0,
            None,
            None,
        )
        .unwrap_err(),
        Error::Limit(Limit::FileBytes)
    );
    spool.empty();
    let raw = parse_reader_with_php_partial(
        &mut Cursor::new(&full),
        b"AaB03x",
        Limits::default(),
        &spool.0,
        None,
        None,
    )
    .unwrap();
    assert_eq!(raw.parts().len(), 1);
    drop(raw);
    spool.empty();
}

#[test]
fn php_partial_eof_after_complete_file_header_registers_error_three() {
    let spool = Spool::new();
    let full = body(&[(FIELD, b"kept"), (FILE, b"unreceived")]);
    let marker = b"\r\nContent-Type: application/octet-stream";
    let header_end = full
        .windows(marker.len())
        .position(|w| w == marker)
        .unwrap();
    // A complete Content-Disposition line, but no Content-Type or blank line.
    let partial = &full[..header_end + 2];
    assert_eq!(
        parse(partial, Limits::default(), &spool).unwrap_err(),
        Error::Truncated
    );
    for split in 0..=partial.len() {
        let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
        parser.feed(&partial[..split]).unwrap();
        parser.feed(&partial[split..]).unwrap();
        let mut raw = parser.finish_with_php_partial().unwrap();
        assert_eq!(raw.parts().len(), 2);
        assert_eq!(raw.parts().next().unwrap().value, Some(b"kept".as_slice()));
        let projection = files::project(&raw, files::Limits::default()).unwrap();
        for (name, value) in [
            (b"file[error]".as_slice(), b"3".as_slice()),
            (b"file[size]", b"0"),
            (b"file[type]", b""),
            (b"file[tmp_name]", b""),
        ] {
            assert!(
                projection
                    .wire()
                    .parts()
                    .any(|p| p.name == name && p.value == Some(value))
            );
        }
        assert_eq!(
            raw.upload_mut(1)
                .unwrap()
                .copy_to(&mut Vec::new())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        drop(projection);
        let admitted = admission::admit(raw, admission::Policy::default()).unwrap();
        assert_eq!(admitted.accepted_indices().count(), 0);
        drop(admitted);
        spool.empty();
    }
    // An unfinished disposition line is not a validated upload header.
    let without_crlf = &partial[..partial.len() - 2];
    assert_eq!(
        parse_reader_with_php_partial(
            &mut Cursor::new(without_crlf),
            b"AaB03x",
            Limits::default(),
            &spool.0,
            None,
            None,
        )
        .unwrap_err(),
        Error::Truncated
    );
    spool.empty();
}

#[test]
fn php_write_failure_preserves_later_fields_and_uploads_without_exposing_prefix() {
    let spool = Spool::new();
    let good_file = b"Content-Disposition: form-data; name=good; filename=good.bin";
    // Force a write error at the 8192-byte flush, not just at a delimiter.
    let bad = vec![b'X'; BLOCK + 19];
    let wire = body(&[(FILE, &bad), (FIELD, b"still-visible"), (good_file, b"ok")]);
    let next = b"Content-Disposition: form-data; name=field";
    let split = wire.windows(next.len()).position(|w| w == next).unwrap();
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0)
        .unwrap()
        .with_php_upload_write_recovery();
    parser.fail_write = true;
    parser.feed(&wire[..split]).unwrap();
    assert!(matches!(
        &parser.request.as_ref().unwrap().parts[0].payload,
        Payload::File(upload) if upload.write_failed && upload.discarded
    ));
    parser.fail_write = false;
    // Exercise the rest of the request with one-byte feeds after the failure.
    for &byte in &wire[split..] {
        parser.feed(&[byte]).unwrap();
    }
    let mut request = parser.finish().unwrap();
    assert_eq!(request.parts().len(), 3);
    assert_eq!(
        request.parts().nth(1).unwrap().value,
        Some(b"still-visible".as_slice())
    );
    let mut denied = Vec::new();
    assert_eq!(
        request
            .upload_mut(0)
            .unwrap()
            .copy_to(&mut denied)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(denied.is_empty());
    let mut copied = Vec::new();
    assert_eq!(
        request.upload_mut(2).unwrap().copy_to(&mut copied).unwrap(),
        2
    );
    assert_eq!(copied, b"ok");
    let metadata = crate::files::project(&request, crate::files::Limits::default()).unwrap();
    let mut json = Vec::new();
    metadata.write_php_files_json(&mut json).unwrap();
    let json = String::from_utf8(json).unwrap();
    // The typed PHP-shaped projection carries a native integer error=7.
    assert!(
        json.contains("[[\"s\",\"6572726f72\"],[\"i\",\"7\"]]"),
        "{json}"
    );
    drop(metadata);
    let mut admitted =
        crate::admission::admit(request, crate::admission::Policy::default()).unwrap();
    assert_eq!(admitted.accepted_indices().collect::<Vec<_>>(), vec![2]);
    assert!(admitted.accepted_file(0).is_none());
    assert_eq!(admitted.copy_accepted(0, &mut denied).unwrap(), None);
    copied.clear();
    assert_eq!(admitted.copy_accepted(2, &mut copied).unwrap(), Some(2));
    assert_eq!(copied, b"ok");
    drop(admitted);
    spool.empty();
}

#[test]
fn php_missing_temp_directory_recovers_and_keeps_later_uploads() {
    let spool = Spool::new();
    let good = b"Content-Disposition: form-data; name=good; filename=good.bin";
    let missing = b"Content-Disposition: form-data; name=missing; filename=lost.bin";
    let later = b"Content-Disposition: form-data; name=later; filename=later.bin";
    let wire = body(&[
        (good, b"first"),
        (missing, b"unavailable"),
        (FIELD, b"still-visible"),
        (later, b"last"),
    ]);
    let missing_marker = b"--AaB03x\r\nContent-Disposition: form-data; name=missing";
    let later_marker = b"--AaB03x\r\nContent-Disposition: form-data; name=later";
    let missing_at = wire
        .windows(missing_marker.len())
        .position(|window| window == missing_marker)
        .unwrap();
    let later_at = wire
        .windows(later_marker.len())
        .position(|window| window == later_marker)
        .unwrap();
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0)
        .unwrap()
        .with_php_upload_write_recovery();
    parser.feed(&wire[..missing_at]).unwrap();
    parser.fail_no_tmp_dir = true;
    parser.feed(&wire[missing_at..later_at]).unwrap();
    parser.fail_no_tmp_dir = false;
    for byte in &wire[later_at..] {
        parser.feed(&[*byte]).unwrap();
    }
    let mut request = parser.finish().unwrap();
    assert_eq!(request.parts().len(), 4);
    assert_eq!(
        request.parts().nth(2).unwrap().value,
        Some(b"still-visible".as_slice())
    );
    let projection = crate::files::project(&request, crate::files::Limits::default()).unwrap();
    for (name, value) in [
        (b"missing[error]".as_slice(), b"6".as_slice()),
        (b"missing[size]", b"0"),
        (b"missing[type]", b""),
        (b"missing[tmp_name]", b""),
        (b"good[error]", b"0"),
        (b"later[error]", b"0"),
    ] {
        assert!(
            projection
                .wire()
                .parts()
                .any(|part| part.name == name && part.value == Some(value)),
            "{name:?}"
        );
    }
    let mut typed = Vec::new();
    projection.write_php_files_json(&mut typed).unwrap();
    let typed = String::from_utf8(typed).unwrap();
    assert!(
        typed.contains("[[\"s\",\"6572726f72\"],[\"i\",\"6\"]]"),
        "{typed}"
    );
    drop(projection);
    let mut denied = Vec::new();
    assert_eq!(
        request
            .upload_mut(1)
            .unwrap()
            .copy_to(&mut denied)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(denied.is_empty());
    let mut admitted =
        crate::admission::admit(request, crate::admission::Policy::default()).unwrap();
    assert_eq!(admitted.accepted_indices().collect::<Vec<_>>(), vec![0, 3]);
    assert!(admitted.accepted_file(1).is_none());
    assert_eq!(admitted.copy_accepted(1, &mut denied).unwrap(), None);
    let mut copied = Vec::new();
    assert_eq!(admitted.copy_accepted(0, &mut copied).unwrap(), Some(5));
    assert_eq!(copied, b"first");
    copied.clear();
    assert_eq!(admitted.copy_accepted(3, &mut copied).unwrap(), Some(4));
    assert_eq!(copied, b"last");
    drop(admitted);
    spool.empty();
}

#[test]
fn php_missing_temp_directory_on_disk_recovers_without_staging() {
    let spool = Spool::new();
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0)
        .unwrap()
        .with_php_upload_write_recovery();
    fs::remove_dir(&spool.0).unwrap();
    let wire = body(&[(FILE, b"cannot-stage"), (FIELD, b"kept")]);
    parser.feed(&wire).unwrap();
    let mut request = parser.finish().unwrap();
    fs::DirBuilder::new().mode(0o700).create(&spool.0).unwrap();
    let projection = crate::files::project(&request, crate::files::Limits::default()).unwrap();
    assert!(
        projection
            .wire()
            .parts()
            .any(|part| { part.name == b"file[error]" && part.value == Some(b"6".as_slice()) })
    );
    assert_eq!(
        request.parts().nth(1).unwrap().value,
        Some(b"kept".as_slice())
    );
    assert_eq!(
        request
            .upload_mut(0)
            .unwrap()
            .copy_to(&mut Vec::new())
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    drop(projection);
    drop(request);
    spool.empty();
}

#[test]
fn php_missing_temp_directory_precedes_upload_max_filesize() {
    let spool = Spool::new();
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0)
        .unwrap()
        .with_php_upload_max_bytes(Some(1))
        .with_php_upload_write_recovery();
    parser.fail_no_tmp_dir = true;
    parser.feed(&body(&[(FILE, b"abc")])).unwrap();
    let request = parser.finish().unwrap();
    let metadata = crate::files::project(
        &request,
        crate::files::Limits {
            max_upload_bytes: Some(1),
            ..crate::files::Limits::default()
        },
    )
    .unwrap();
    assert!(
        metadata
            .wire()
            .parts()
            .any(|part| { part.name == b"file[error]" && part.value == Some(b"6".as_slice()) })
    );
    drop(metadata);
    let admitted = crate::admission::admit(request, crate::admission::Policy::default()).unwrap();
    assert_eq!(admitted.accepted_indices().count(), 0);
    drop(admitted);
    spool.empty();

    // Even with no writable temp directory, an empty filename is error=4.
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0)
        .unwrap()
        .with_php_upload_write_recovery();
    parser.fail_no_tmp_dir = true;
    let empty_name = b"Content-Disposition: form-data; name=file; filename=\"\"";
    parser.feed(&body(&[(empty_name, b"")])).unwrap();
    let request = parser.finish().unwrap();
    let metadata = crate::files::project(&request, crate::files::Limits::default()).unwrap();
    assert!(
        metadata
            .wire()
            .parts()
            .any(|part| { part.name == b"file[error]" && part.value == Some(b"4".as_slice()) })
    );
    drop(metadata);
    drop(request);
    spool.empty();

    // Permission-denied create failures are not evidence of a missing
    // directory; PHP-mode recovery must not mask these I/O failures.
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0)
        .unwrap()
        .with_php_upload_write_recovery();
    parser.fail_create = true;
    assert_eq!(
        parser.feed(&body(&[(FILE, b"abc")])),
        Err(Error::Io(io::ErrorKind::PermissionDenied))
    );
    spool.empty();
}

#[test]
fn upload_spool_replacement_and_permission_changes_fail_closed() {
    let spool = Spool::new();
    let other = Spool::new();
    let wire = body(&[(FILE, b"must-not-stage")]);

    // The directory was private at construction, but became public before
    // the first upload. Both strict and opt-in PHP paths must reject it.
    for php_mode in [false, true] {
        let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
        if php_mode {
            parser = parser.with_php_upload_write_recovery();
        }
        fs::set_permissions(&spool.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(parser.feed(&wire), Err(Error::InvalidSpoolDirectory));
        fs::set_permissions(&spool.0, fs::Permissions::from_mode(0o700)).unwrap();
        spool.empty();
    }

    // Replacing the trusted directory with a symlink to another private
    // directory is forbidden even when the target itself is mode 0700.
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
    let saved = spool.0.with_extension("saved");
    fs::rename(&spool.0, &saved).unwrap();
    std::os::unix::fs::symlink(&other.0, &spool.0).unwrap();
    assert_eq!(parser.feed(&wire), Err(Error::InvalidSpoolDirectory));
    fs::remove_file(&spool.0).unwrap();
    fs::rename(&saved, &spool.0).unwrap();
    spool.empty();
    other.empty();

    // A fresh 0700 directory at the same path is not the originally
    // validated inode and must not silently become a new staging target.
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &spool.0).unwrap();
    fs::rename(&spool.0, &saved).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&spool.0).unwrap();
    assert_eq!(parser.feed(&wire), Err(Error::InvalidSpoolDirectory));
    fs::remove_dir(&spool.0).unwrap();
    fs::rename(&saved, &spool.0).unwrap();
    spool.empty();

    // An unchanged directory still permits normal staged upload copying.
    let mut request = parse(&wire, Limits::default(), &spool).unwrap();
    let mut copied = Vec::new();
    assert_eq!(
        request.upload_mut(0).unwrap().copy_to(&mut copied).unwrap(),
        14
    );
    assert_eq!(copied, b"must-not-stage");
    drop(request);
    spool.empty();
}

#[test]
fn canonical_spool_path_survives_parent_symlink_retarget() {
    let original = Spool::new();
    let replacement = Spool::new();
    let original_uploads = original.0.join("uploads");
    let replacement_uploads = replacement.0.join("uploads");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&original_uploads)
        .unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&replacement_uploads)
        .unwrap();
    let alias = std::env::temp_dir().join(format!(
        "rustpon-spool-parent-{}-{}",
        std::process::id(),
        NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed)
    ));
    std::os::unix::fs::symlink(&original.0, &alias).unwrap();

    // The final spool component is a real directory; only its ancestor
    // is a symlink. Resolve it once and retain the original target.
    let mut parser = Parser::new(b"AaB03x", Limits::default(), &alias.join("uploads")).unwrap();
    assert_eq!(
        parser.spool_path,
        fs::canonicalize(&original_uploads).unwrap()
    );

    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&replacement.0, &alias).unwrap();
    parser.feed(&body(&[(FILE, b"original-target")])).unwrap();
    let mut request = parser.finish().unwrap();
    let mut copied = Vec::new();
    assert_eq!(
        request.upload_mut(0).unwrap().copy_to(&mut copied).unwrap(),
        15
    );
    assert_eq!(copied, b"original-target");
    drop(request);

    assert_eq!(fs::read_dir(&original_uploads).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&replacement_uploads).unwrap().count(), 0);
    fs::remove_file(&alias).unwrap();
    fs::remove_dir(&original_uploads).unwrap();
    fs::remove_dir(&replacement_uploads).unwrap();
    original.empty();
    replacement.empty();
}

#[test]
fn post_create_spool_retarget_fails_before_upload_bytes_are_written() {
    let spool = Spool::new();
    let other = Spool::new();
    let original = fs::symlink_metadata(&spool.0).unwrap();
    let saved = spool.0.with_extension("post-create-saved");
    let mut path = spool.0.clone();

    // Swap the parent after create_new has opened an empty file but before
    // anonymous_upload unlinks it. A pathname-only preflight misses this.
    let result = anonymous_upload_with_hook(&mut path, original.dev(), original.ino(), || {
        fs::rename(&spool.0, &saved).unwrap();
        std::os::unix::fs::symlink(&other.0, &spool.0).unwrap();
    });
    assert_eq!(result.unwrap_err(), Error::InvalidSpoolDirectory);
    assert_eq!(path, spool.0);
    other.empty();

    // The open descriptor is closed and the orphan in the moved directory
    // is zero length: no untrusted upload bytes have been written.
    let created: Vec<_> = fs::read_dir(&saved)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(fs::metadata(&created[0]).unwrap().len(), 0);
    fs::remove_file(&created[0]).unwrap();
    fs::remove_file(&spool.0).unwrap();
    fs::rename(&saved, &spool.0).unwrap();
    spool.empty();
}

#[test]
fn post_create_filename_swap_never_unlinks_a_different_inode() {
    let spool = Spool::new();
    let original = fs::symlink_metadata(&spool.0).unwrap();
    let sentinel = spool.0.join("sentinel");
    let parked = spool.0.join("parked");
    fs::write(&sentinel, b"must-survive").unwrap();
    let mut temporary_path = None;
    let mut path = spool.0.clone();

    let result = anonymous_upload_with_hook(&mut path, original.dev(), original.ino(), || {
        let created = fs::read_dir(&spool.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| {
                entry
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("php-request-")
            })
            .unwrap();
        fs::rename(&created, &parked).unwrap();
        std::os::unix::fs::symlink(&sentinel, &created).unwrap();
        temporary_path = Some(created);
    });
    assert_eq!(result.unwrap_err(), Error::InvalidSpoolDirectory);
    assert_eq!(fs::read(&sentinel).unwrap(), b"must-survive");
    assert_eq!(fs::metadata(&parked).unwrap().len(), 0);

    fs::remove_file(temporary_path.unwrap()).unwrap();
    fs::remove_file(parked).unwrap();
    fs::remove_file(sentinel).unwrap();
    spool.empty();
}

#[test]
fn post_create_hardlink_is_rejected_before_any_upload_data() {
    let spool = Spool::new();
    let original = fs::symlink_metadata(&spool.0).unwrap();
    let exposed = spool.0.join("exposed");
    let mut path = spool.0.clone();

    // An attacker with directory access links the just-created inode under
    // another name. Unlinking the generated name alone is insufficient.
    let result = anonymous_upload_with_hook(&mut path, original.dev(), original.ino(), || {
        let created = fs::read_dir(&spool.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| {
                entry
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("php-request-")
            })
            .unwrap();
        fs::hard_link(&created, &exposed).unwrap();
    });
    assert_eq!(result.unwrap_err(), Error::InvalidSpoolDirectory);
    assert_eq!(fs::metadata(&exposed).unwrap().len(), 0);
    fs::remove_file(exposed).unwrap();
    spool.empty();
}

#[test]
fn staged_inode_requires_private_mode_and_matching_directory_owner() {
    let spool = Spool::new();
    let path = spool.0.join("owner-mode-check");
    fs::write(&path, b"").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let directory_owner = fs::metadata(&spool.0).unwrap().uid();
    let metadata = fs::metadata(&path).unwrap();
    assert!(private_staging_inode(&metadata, Some(directory_owner)));
    assert!(!private_staging_inode(
        &metadata,
        Some(directory_owner.wrapping_add(1))
    ));
    assert!(!private_staging_inode(&metadata, None));

    // Another local account must not be able to read a staged inode,
    // even if the spool directory itself is still private.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!private_staging_inode(
        &fs::metadata(&path).unwrap(),
        Some(directory_owner)
    ));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(path).unwrap();
    spool.empty();
}

#[test]
fn post_create_staged_inode_permission_relaxation_is_rejected() {
    let spool = Spool::new();
    let original = fs::symlink_metadata(&spool.0).unwrap();
    let mut path = spool.0.clone();

    // Simulate a same-UID actor changing the created inode's mode before
    // unlink. It must be rejected while still empty, without upload bytes.
    let result = anonymous_upload_with_hook(&mut path, original.dev(), original.ino(), || {
        let created = fs::read_dir(&spool.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| {
                entry
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("php-request-")
            })
            .unwrap();
        fs::set_permissions(created, fs::Permissions::from_mode(0o644)).unwrap();
    });
    assert_eq!(result.unwrap_err(), Error::InvalidSpoolDirectory);
    assert_eq!(path, spool.0);
    let created: Vec<_> = fs::read_dir(&spool.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(fs::metadata(&created[0]).unwrap().len(), 0);
    fs::remove_file(&created[0]).unwrap();
    spool.empty();
}

#[test]
fn post_unlink_directory_swap_is_rejected_before_upload_bytes() {
    let spool = Spool::new();
    let replacement = Spool::new();
    let original = fs::symlink_metadata(&spool.0).unwrap();
    let saved = spool.0.with_extension("post-unlink-saved");
    let mut path = spool.0.clone();

    // The inode has already been unlinked, so no directory entry can reveal
    // its contents. A directory swap must still prevent accepting the FD.
    let result = anonymous_upload_with_hooks(
        &mut path,
        original.dev(),
        original.ino(),
        || {},
        || {
            fs::rename(&spool.0, &saved).unwrap();
            std::os::unix::fs::symlink(&replacement.0, &spool.0).unwrap();
        },
    );
    assert_eq!(result.unwrap_err(), Error::InvalidSpoolDirectory);
    assert_eq!(path, spool.0);
    assert_eq!(fs::read_dir(&saved).unwrap().count(), 0);
    replacement.empty();

    fs::remove_file(&spool.0).unwrap();
    fs::rename(&saved, &spool.0).unwrap();
    spool.empty();
}

#[test]
fn post_unlink_permission_relaxation_is_rejected() {
    let spool = Spool::new();
    let original = fs::symlink_metadata(&spool.0).unwrap();
    let mut path = spool.0.clone();

    let result = anonymous_upload_with_hooks(
        &mut path,
        original.dev(),
        original.ino(),
        || {},
        || {
            fs::set_permissions(&spool.0, fs::Permissions::from_mode(0o755)).unwrap();
        },
    );
    assert_eq!(result.unwrap_err(), Error::InvalidSpoolDirectory);
    assert_eq!(path, spool.0);
    fs::set_permissions(&spool.0, fs::Permissions::from_mode(0o700)).unwrap();
    spool.empty();

    // The same path and inode are still usable once permissions are private.
    let file = anonymous_upload_with_hooks(&mut path, original.dev(), original.ino(), || {}, || {})
        .unwrap();
    assert_eq!(file.metadata().unwrap().nlink(), 0);
    assert_eq!(file.metadata().unwrap().len(), 0);
    drop(file);
    spool.empty();
}

#[test]
fn upload_copy_rejects_permission_and_length_tampering_before_output() {
    let spool = Spool::new();
    let mut request = parse(&body(&[(FILE, b"xyz")]), Limits::default(), &spool).unwrap();
    let upload = request.upload_mut(0).unwrap();
    let mut output = Vec::new();

    assert_eq!(upload.copy_to(&mut output).unwrap(), 3);
    assert_eq!(output, b"xyz");
    output.clear();

    let file = upload.file.as_ref().unwrap();
    assert_eq!(file.metadata().unwrap().nlink(), 0);
    file.set_permissions(fs::Permissions::from_mode(0o644))
        .unwrap();
    assert_eq!(
        upload.copy_to(&mut output).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert!(output.is_empty());

    upload
        .file
        .as_ref()
        .unwrap()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    upload.file.as_ref().unwrap().set_len(4).unwrap();
    assert_eq!(
        upload.copy_to(&mut output).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert!(output.is_empty());

    upload.file.as_ref().unwrap().set_len(3).unwrap();
    assert_eq!(upload.copy_to(&mut output).unwrap(), 3);
    assert_eq!(output, b"xyz");
    drop(request);
    spool.empty();
}

#[test]
fn upload_copy_rejects_linked_file_even_with_private_permissions() {
    let spool = Spool::new();
    let named = spool.0.join("linked-upload");
    fs::write(&named, b"xyz").unwrap();
    fs::set_permissions(&named, fs::Permissions::from_mode(0o600)).unwrap();
    let file = OpenOptions::new().read(true).open(&named).unwrap();
    assert_eq!(file.metadata().unwrap().nlink(), 1);
    let mut upload = Upload {
        file: Some(file),
        length: 3,
        discarded: false,
        partial: false,
        write_failed: false,
        no_tmp_dir: false,
    };
    let mut output = Vec::new();
    assert_eq!(
        upload.copy_to(&mut output).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert!(output.is_empty());
    drop(upload);
    fs::remove_file(named).unwrap();
    spool.empty();
}

#[test]
fn upload_copy_detects_source_mutation_during_destination_write() {
    struct TamperingWriter {
        output: Vec<u8>,
        source: File,
        mutate: fn(&File),
        mutated: bool,
    }

    impl Write for TamperingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(bytes);
            if !self.mutated {
                (self.mutate)(&self.source);
                self.mutated = true;
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn expose(file: &File) {
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .unwrap();
    }

    fn restore_private(file: &File) {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
    }

    fn extend(file: &File) {
        file.set_len((BLOCK * 3 + 12) as u64).unwrap();
    }

    fn restore_length(file: &File) {
        file.set_len((BLOCK * 3 + 11) as u64).unwrap();
    }

    let spool = Spool::new();
    // More than three blocks: a mutation after block one must prevent
    // subsequent blocks from reaching the destination writer.
    let payload = vec![0x5a; BLOCK * 3 + 11];
    let mut request = parse(&body(&[(FILE, &payload)]), Limits::default(), &spool).unwrap();
    let upload = request.upload_mut(0).unwrap();
    for (mutate, restore) in [
        (expose as fn(&File), restore_private as fn(&File)),
        (extend as fn(&File), restore_length as fn(&File)),
    ] {
        let mut writer = TamperingWriter {
            output: Vec::new(),
            source: upload.file.as_ref().unwrap().try_clone().unwrap(),
            mutate,
            mutated: false,
        };
        assert_eq!(
            upload.copy_to(&mut writer).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(writer.mutated);
        // Only the first block is emitted; the remaining blocks are never
        // read or written after an observable mutation. Discard the prefix.
        assert_eq!(writer.output, payload[..BLOCK]);
        restore(upload.file.as_ref().unwrap());
        let mut recovered = Vec::new();
        assert_eq!(
            upload.copy_to(&mut recovered).unwrap(),
            payload.len() as u64
        );
        assert_eq!(recovered, payload);
    }
    drop(request);
    spool.empty();
}

#[test]
fn upload_copy_detects_same_length_rewrite_during_destination_write() {
    use std::os::unix::fs::FileExt;

    struct RewritingWriter {
        output: Vec<u8>,
        source: File,
        rewritten: bool,
    }

    impl Write for RewritingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(bytes);
            if !self.rewritten {
                // Rewrite an already-copied byte without changing file length,
                // link count or permissions. Force a distinct mtime even on
                // filesystems with coarse timestamp resolution.
                self.source.write_at(b"!", 0)?;
                self.source
                    .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))?;
                self.rewritten = true;
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let spool = Spool::new();
    let payload = vec![b'x'; BLOCK * 2 + 1];
    let mut request = parse(&body(&[(FILE, &payload)]), Limits::default(), &spool).unwrap();
    let upload = request.upload_mut(0).unwrap();
    let mut writer = RewritingWriter {
        output: Vec::new(),
        source: upload.file.as_ref().unwrap().try_clone().unwrap(),
        rewritten: false,
    };
    assert_eq!(
        upload.copy_to(&mut writer).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert!(writer.rewritten);
    // The destination has received one block, which must be discarded.
    // A timestamp change stops the copy before the remaining blocks.
    assert_eq!(writer.output, payload[..BLOCK]);

    // Repairing the source allows a new copy with its own metadata baseline.
    upload.file.as_ref().unwrap().write_at(b"x", 0).unwrap();
    let mut recovered = Vec::new();
    assert_eq!(
        upload.copy_to(&mut recovered).unwrap(),
        payload.len() as u64
    );
    assert_eq!(recovered, payload);
    drop(request);
    spool.empty();
}

#[test]
fn upload_copy_is_independent_of_shared_file_offsets() {
    struct SeekingWriter {
        output: Vec<u8>,
        source: File,
        seek_to_end: bool,
        writes: usize,
    }

    impl Write for SeekingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(bytes);
            self.writes += 1;
            // A cloned descriptor shares the underlying kernel file offset.
            // Redirect it after every write, without modifying file contents.
            let target = if self.seek_to_end {
                SeekFrom::End(0)
            } else {
                SeekFrom::Start(0)
            };
            self.source.seek(target)?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let spool = Spool::new();
    let payload: Vec<u8> = (0..BLOCK * 2 + 17)
        .map(|index| ((index * 31 + 17) % 251) as u8)
        .collect();
    let mut request = parse(&body(&[(FILE, &payload)]), Limits::default(), &spool).unwrap();
    let upload = request.upload_mut(0).unwrap();

    // A positional copy must not alter another handle's shared offset.
    let mut observer = upload.file.as_ref().unwrap().try_clone().unwrap();
    observer.seek(SeekFrom::Start(7)).unwrap();
    let mut copied = Vec::new();
    assert_eq!(upload.copy_to(&mut copied).unwrap(), payload.len() as u64);
    assert_eq!(copied, payload);
    assert_eq!(observer.stream_position().unwrap(), 7);

    // A writer that seeks a clone to EOF used to cause a premature EOF;
    // seeking back to zero used to duplicate the first block.
    for seek_to_end in [true, false] {
        let mut writer = SeekingWriter {
            output: Vec::new(),
            source: upload.file.as_ref().unwrap().try_clone().unwrap(),
            seek_to_end,
            writes: 0,
        };
        assert_eq!(upload.copy_to(&mut writer).unwrap(), payload.len() as u64);
        assert!(writer.writes >= 3);
        assert_eq!(writer.output, payload);
    }

    drop(request);
    spool.empty();
}
