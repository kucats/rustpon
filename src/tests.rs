use super::*;
use std::io::Cursor;

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
                upload.file.metadata().unwrap().len(),
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
