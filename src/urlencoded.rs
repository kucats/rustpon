//! Streaming application/x-www-form-urlencoded input, yielding the same
//! request-owned wire IR as multipart. No PHP calls or global request state.
//!
//! Percent-decoding occurs only after a complete field is seen, so escapes can
//! cross arbitrarily sized feed boundaries without allocating extra buffers.
//! Malformed escapes remain literal, following PHP's url-decoding convention.
//! This strict prototype rejects decoded NUL bytes in field names.

use crate::{Error, Limit, Limits, Part, Payload, Request, Stats, add_u64, add_usize};
use std::io::Read;
use std::ops::Range;

/// Single-request decoder. Only finish() can publish a Request. Dropping or
/// cancelling the decoder releases all retained bytes.
pub struct FormParser {
    limits: Limits,
    request: Option<Request>,
    failed: Option<Error>,
    field_start: usize,
    value_start: Option<usize>,
    raw_name: usize,
    raw_value: usize,
    raw_values_total: usize,
    // A fixed-size set of raw separator bytes (PHP arg_separator.input).
    // Four words cover all 256 byte values without per-request allocation.
    separators: [u64; 4],
}

impl FormParser {
    pub fn new(limits: Limits) -> Result<Self, Error> {
        let requested = limits
            .parts
            .checked_mul(size_of::<Part>())
            .and_then(|n| n.checked_add(limits.retained_bytes))
            .and_then(|n| n.checked_add(size_of::<Self>()))
            .ok_or(Error::InvalidLimits)?;
        if requested > limits.memory_capacity {
            return Err(Error::Limit(Limit::MemoryCapacity));
        }
        let mut arena = Vec::new();
        arena
            .try_reserve_exact(limits.retained_bytes)
            .map_err(|_| Error::Allocation)?;
        let mut parts = Vec::new();
        parts
            .try_reserve_exact(limits.parts)
            .map_err(|_| Error::Allocation)?;
        let actual = size_of::<Self>()
            .checked_add(arena.capacity())
            .and_then(|n| {
                parts
                    .capacity()
                    .checked_mul(size_of::<Part>())
                    .and_then(|p| n.checked_add(p))
            })
            .ok_or(Error::InvalidLimits)?;
        if actual > limits.memory_capacity {
            return Err(Error::Limit(Limit::MemoryCapacity));
        }
        Ok(Self {
            limits,
            request: Some(Request {
                arena,
                parts,
                stats: Stats {
                    memory_capacity: actual,
                    ..Stats::default()
                },
            }),
            failed: None,
            field_start: 0,
            value_start: None,
            raw_name: 0,
            raw_value: 0,
            raw_values_total: 0,
            separators: [1u64 << (b'&' as usize % 64), 0, 0, 0],
        })
    }

    /// Configure the raw separator byte set used by PHP arg_separator.input.
    /// Empty settings fall back to PHP's default '&'. Percent-encoded bytes
    /// are decoded only after splitting and therefore cannot be separators.
    pub fn with_php_arg_separators(mut self, separators: &[u8]) -> Self {
        self.separators = [0; 4];
        let separators = if separators.is_empty() {
            b"&".as_slice()
        } else {
            separators
        };
        for &byte in separators {
            self.separators[usize::from(byte) / 64] |= 1u64 << (byte % 64);
        }
        self
    }

    /// Compatibility shorthand for PHP arg_separator.input='&;'.
    pub fn with_php_semicolon_separator(self) -> Self {
        self.with_php_arg_separators(b"&;")
    }

    fn is_separator(&self, byte: u8) -> bool {
        self.separators[usize::from(byte) / 64] & (1u64 << (byte % 64)) != 0
    }

    fn request(&mut self) -> &mut Request {
        self.request.as_mut().expect("live form request")
    }

    fn abort(&mut self, error: Error) -> Error {
        self.request.take();
        self.failed = Some(error);
        error
    }

    pub fn cancel(&mut self) {
        self.abort(Error::Cancelled);
    }

    /// The caller supplies the raw, dechunked body, not an HTTP message.
    /// '&' separates pairs; only the first '=' separates name from value.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        match self.feed_inner(bytes) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.abort(error)),
        }
    }

    fn feed_inner(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let amount = u64::try_from(bytes.len()).map_err(|_| Error::Limit(Limit::BodyBytes))?;
        let total = add_u64(
            self.request().stats.body_bytes,
            amount,
            self.limits.body_bytes,
            Limit::BodyBytes,
        )?;
        self.request().stats.body_bytes = total;
        for &byte in bytes {
            if self.is_separator(byte) {
                self.complete_field()?;
            } else if byte == b'=' && self.value_start.is_none() {
                self.value_start = Some(self.request().arena.len());
            } else {
                self.raw_byte(byte)?;
            }
        }
        Ok(())
    }

    fn raw_byte(&mut self, byte: u8) -> Result<(), Error> {
        if self.value_start.is_some() {
            self.raw_value = add_usize(
                self.raw_value,
                1,
                self.limits.field_bytes,
                Limit::FieldBytes,
            )?;
            self.raw_values_total = add_usize(
                self.raw_values_total,
                1,
                self.limits.field_bytes_total,
                Limit::FieldBytesTotal,
            )?;
        } else {
            self.raw_name = add_usize(self.raw_name, 1, self.limits.name_bytes, Limit::NameBytes)?;
        }
        add_usize(
            self.request().arena.len(),
            1,
            self.limits.retained_bytes,
            Limit::RetainedBytes,
        )?;
        self.request().arena.push(byte);
        Ok(())
    }

    fn complete_field(&mut self) -> Result<(), Error> {
        let end = self.request().arena.len();
        let value_start = self.value_start.unwrap_or(end);
        if self.field_start == value_start {
            // PHP ignores blank pairs and pairs with an empty name.
            let discard_from = self.field_start;
            self.request().arena.truncate(discard_from);
        } else {
            let part_count = add_usize(
                self.request().stats.parts,
                1,
                self.limits.parts,
                Limit::Parts,
            )?;
            let field_count = add_usize(
                self.request().stats.fields,
                1,
                self.limits.fields,
                Limit::Fields,
            )?;
            let field_start = self.field_start;
            let (name_end, value_end) = {
                let arena = &mut self.request().arena;
                let decoded_name = url_decode(&mut arena[field_start..value_start]);
                let name_end = field_start + decoded_name;
                let raw_value_len = end - value_start;
                arena.copy_within(value_start..end, name_end);
                let moved_end = name_end + raw_value_len;
                arena.truncate(moved_end);
                let decoded_value = url_decode(&mut arena[name_end..moved_end]);
                let value_end = name_end + decoded_value;
                arena.truncate(value_end);
                (name_end, value_end)
            };
            if self.request().arena[field_start..name_end].contains(&0) {
                return Err(Error::Malformed);
            }
            let value_len = value_end - name_end;
            let total = add_usize(
                self.request().stats.field_bytes,
                value_len,
                self.limits.field_bytes_total,
                Limit::FieldBytesTotal,
            )?;
            self.request().parts.push(Part {
                name: Range {
                    start: field_start,
                    end: name_end,
                },
                filename: None,
                content_type: None,
                payload: Payload::Field(name_end..value_end),
            });
            let stats = &mut self.request().stats;
            stats.parts = part_count;
            stats.fields = field_count;
            stats.field_bytes = total;
        }
        self.field_start = self.request().arena.len();
        self.value_start = None;
        self.raw_name = 0;
        self.raw_value = 0;
        Ok(())
    }

    /// Must be called only after transport EOF. Failure consumes the request.
    pub fn finish(mut self) -> Result<Request, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.complete_field()?;
        Ok(self.request.take().expect("live completed form"))
    }
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// Decode into the same allocation. Every encoded sequence maps to at most one
// byte, so writes never outrun unread data.
fn url_decode(bytes: &mut [u8]) -> usize {
    let mut read = 0;
    let mut written = 0;
    while read < bytes.len() {
        if bytes[read] == b'%'
            && read + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[read + 1]), hex(bytes[read + 2]))
        {
            bytes[written] = (hi << 4) | lo;
            read += 3;
            written += 1;
            continue;
        }
        bytes[written] = if bytes[read] == b'+' {
            b' '
        } else {
            bytes[read]
        };
        read += 1;
        written += 1;
    }
    written
}

/// Standalone blocking-Read adapter with no HTTP transport assumptions.
pub fn parse_reader(reader: &mut impl Read, limits: Limits) -> Result<Request, Error> {
    parse_reader_impl(reader, limits, b"&")
}

/// Opt-in PHP 8.4 `arg_separator.input='&;'` parsing for trusted SAPI settings.
/// The strict default entry point continues to use only '&'.
pub fn parse_reader_with_php_semicolon_separator(
    reader: &mut impl Read,
    limits: Limits,
) -> Result<Request, Error> {
    parse_reader_impl(reader, limits, b"&;")
}

/// Parse using PHP's configured arg_separator.input byte set. The empty
/// setting follows PHP's fallback to '&'; this API does not parse INI syntax.
pub fn parse_reader_with_php_arg_separators(
    reader: &mut impl Read,
    limits: Limits,
    separators: &[u8],
) -> Result<Request, Error> {
    parse_reader_impl(reader, limits, separators)
}

fn parse_reader_impl(
    reader: &mut impl Read,
    limits: Limits,
    separators: &[u8],
) -> Result<Request, Error> {
    let mut parser = FormParser::new(limits)?.with_php_arg_separators(separators);
    let mut buffer = [0u8; 8192];
    let mut consecutive_interrupts = 0usize;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return parser.finish(),
            // A custom Read reporting more than the supplied capacity is
            // invalid; never let its count trigger an out-of-bounds panic.
            Ok(n) if n > buffer.len() => {
                return Err(Error::Io(std::io::ErrorKind::InvalidData));
            }
            Ok(n) => {
                consecutive_interrupts = 0;
                parser.feed(&buffer[..n])?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                consecutive_interrupts += 1;
                if consecutive_interrupts >= crate::MAX_CONSECUTIVE_INTERRUPTS {
                    return Err(Error::Io(std::io::ErrorKind::Interrupted));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post;
    use std::io::Cursor;

    fn parse(body: &[u8], limits: Limits) -> Result<Request, Error> {
        parse_reader(&mut Cursor::new(body), limits)
    }

    fn fields(request: &Request) -> Vec<(Vec<u8>, Vec<u8>)> {
        request
            .parts()
            .map(|p| (p.name.to_vec(), p.value.unwrap().to_vec()))
            .collect()
    }

    #[test]
    fn php_configured_separator_bytes_match_raw_split_semantics() {
        let body = b"a=1|b=2&c=3;d=4";
        let expected = vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2&c=3;d=4".to_vec()),
        ];
        assert_eq!(
            fields(
                &parse_reader_with_php_arg_separators(
                    &mut Cursor::new(body),
                    Limits::default(),
                    b"|",
                )
                .unwrap()
            ),
            expected
        );
        for split in 0..=body.len() {
            let mut parser = FormParser::new(Limits::default())
                .unwrap()
                .with_php_arg_separators(b"|");
            parser.feed(&body[..split]).unwrap();
            parser.feed(&body[split..]).unwrap();
            assert_eq!(fields(&parser.finish().unwrap()), expected, "split {split}");
        }
        // A separator may itself be '='. Separators are applied before the
        // first name/value delimiter, just as in PHP's parse_str().
        let equal = parse_reader_with_php_arg_separators(
            &mut Cursor::new(b"a=1=b=2"),
            Limits::default(),
            b"=",
        )
        .unwrap();
        assert_eq!(
            fields(&equal),
            vec![
                (b"a".to_vec(), Vec::new()),
                (b"1".to_vec(), Vec::new()),
                (b"b".to_vec(), Vec::new()),
                (b"2".to_vec(), Vec::new()),
            ]
        );
        // Raw '+' is a separator, but its percent-encoded spelling is not.
        let plus = parse_reader_with_php_arg_separators(
            &mut Cursor::new(b"a=1%2B2+b=3"),
            Limits::default(),
            b"+",
        )
        .unwrap();
        assert_eq!(
            fields(&plus),
            vec![
                (b"a".to_vec(), b"1+2".to_vec()),
                (b"b".to_vec(), b"3".to_vec()),
            ]
        );
        assert_eq!(
            fields(
                &parse_reader_with_php_arg_separators(
                    &mut Cursor::new(b"a=1&b=2"),
                    Limits::default(),
                    b"",
                )
                .unwrap()
            ),
            fields(&parse(b"a=1&b=2", Limits::default()).unwrap())
        );
        let mut limited = FormParser::new(Limits {
            parts: 1,
            fields: 1,
            ..Limits::default()
        })
        .unwrap()
        .with_php_arg_separators(b"|");
        assert_eq!(limited.feed(b"a=1|b=2"), Ok(()));
        assert!(matches!(limited.finish(), Err(Error::Limit(Limit::Parts))));
    }

    #[test]
    fn php_semicolon_separator_is_opt_in_and_chunk_independent() {
        let body = b"a=one;b%5Bc%5D=two&x=1%3B2;;z=3";
        let expected = vec![
            (b"a".to_vec(), b"one".to_vec()),
            (b"b[c]".to_vec(), b"two".to_vec()),
            (b"x".to_vec(), b"1;2".to_vec()),
            (b"z".to_vec(), b"3".to_vec()),
        ];
        let strict = parse(body, Limits::default()).unwrap();
        assert_eq!(strict.stats().fields, 2);
        assert_eq!(fields(&strict)[0].1, b"one;b[c]=two");
        assert_eq!(
            fields(
                &parse_reader_with_php_semicolon_separator(
                    &mut Cursor::new(body),
                    Limits::default(),
                )
                .unwrap()
            ),
            expected
        );
        for split in 0..=body.len() {
            let mut parser = FormParser::new(Limits::default())
                .unwrap()
                .with_php_semicolon_separator();
            parser.feed(&body[..split]).unwrap();
            parser.feed(&body[split..]).unwrap();
            assert_eq!(fields(&parser.finish().unwrap()), expected, "split {split}");
        }
        let mut parser = FormParser::new(Limits {
            parts: 3,
            fields: 3,
            ..Limits::default()
        })
        .unwrap()
        .with_php_semicolon_separator();
        // The final field is committed at EOF, not during feed().
        assert_eq!(parser.feed(body), Ok(()));
        assert_eq!(parser.finish().unwrap_err(), Error::Limit(Limit::Parts));
    }

    #[test]
    fn decoding_and_php_style_post_projection() {
        let bytes = b"a%5Bx%5D=one&a%5B%5D=two&hello+world=hi%00%ff&bad=%2+&z=%2%41";
        let request = parse(bytes, Limits::default()).unwrap();
        assert_eq!(
            fields(&request),
            vec![
                (b"a[x]".to_vec(), b"one".to_vec()),
                (b"a[]".to_vec(), b"two".to_vec()),
                (b"hello world".to_vec(), b"hi\0\xff".to_vec()),
                (b"bad".to_vec(), b"%2 ".to_vec()),
                (b"z".to_vec(), b"%2A".to_vec()),
            ]
        );
        assert_eq!(request.stats().fields, 5);
        assert_eq!(request.stats().body_bytes, bytes.len() as u64);
        let post = request.into_post(post::Limits::default()).unwrap();
        let mut json = Vec::new();
        post.write_json(&mut json).unwrap();
        let json = String::from_utf8(json).unwrap();
        assert!(json.contains(r#"["s","686900ff"]"#));
        assert!(json.contains(r#"["i","0"]"#));
    }

    #[test]
    fn every_chunk_split_and_duplicate_order() {
        let body = b"&a=1&&a=2&empty&x=&=ignored&eq=a=b&k=%41%2B+%";
        let baseline = parse(body, Limits::default()).unwrap();
        let expected = fields(&baseline);
        assert_eq!(
            expected,
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"a".to_vec(), b"2".to_vec()),
                (b"empty".to_vec(), b"".to_vec()),
                (b"x".to_vec(), b"".to_vec()),
                (b"eq".to_vec(), b"a=b".to_vec()),
                (b"k".to_vec(), b"A+ %".to_vec()),
            ]
        );
        for split in 0..=body.len() {
            let mut parser = FormParser::new(Limits::default()).unwrap();
            parser.feed(&body[..split]).unwrap();
            parser.feed(&body[split..]).unwrap();
            assert_eq!(fields(&parser.finish().unwrap()), expected, "split {split}");
        }
    }

    #[test]
    fn limits_abort_before_publication() {
        let cases = [
            (
                b"ab=1".as_slice(),
                Limits {
                    name_bytes: 1,
                    ..Limits::default()
                },
                Limit::NameBytes,
            ),
            (
                b"a=xyz",
                Limits {
                    field_bytes: 2,
                    ..Limits::default()
                },
                Limit::FieldBytes,
            ),
            (
                b"a=12&b=34",
                Limits {
                    field_bytes_total: 3,
                    ..Limits::default()
                },
                Limit::FieldBytesTotal,
            ),
            (
                b"a=1&b=2",
                Limits {
                    parts: 1,
                    ..Limits::default()
                },
                Limit::Parts,
            ),
            (
                b"a=1&b=2",
                Limits {
                    fields: 1,
                    ..Limits::default()
                },
                Limit::Fields,
            ),
            (
                b"a=1",
                Limits {
                    body_bytes: 2,
                    ..Limits::default()
                },
                Limit::BodyBytes,
            ),
            (
                b"a=1",
                Limits {
                    retained_bytes: 1,
                    ..Limits::default()
                },
                Limit::RetainedBytes,
            ),
        ];
        for (body, limits, expected) in cases {
            assert_eq!(parse(body, limits).unwrap_err(), Error::Limit(expected));
        }
        let mut parser = FormParser::new(Limits::default()).unwrap();
        parser.feed(b"a=1").unwrap();
        parser.cancel();
        assert_eq!(parser.feed(b"&b=2"), Err(Error::Cancelled));
        assert!(matches!(parser.finish(), Err(Error::Cancelled)));
        assert_eq!(
            parse(b"a%00b=1", Limits::default()).unwrap_err(),
            Error::Malformed
        );
        assert_eq!(fields(&parse(b"&&", Limits::default()).unwrap()), vec![]);
    }

    #[test]
    fn capacity_and_reader_errors() {
        let baseline = parse(b"a=1", Limits::default())
            .unwrap()
            .stats()
            .memory_capacity;
        let limits = Limits {
            memory_capacity: baseline,
            ..Limits::default()
        };
        assert!(FormParser::new(limits).is_ok());
        assert!(matches!(
            FormParser::new(Limits {
                memory_capacity: baseline - 1,
                ..limits
            }),
            Err(Error::Limit(Limit::MemoryCapacity))
        ));
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        assert_eq!(
            parse_reader(&mut Broken, Limits::default()).unwrap_err(),
            Error::Io(std::io::ErrorKind::BrokenPipe)
        );
    }

    #[test]
    fn overreported_reader_count_is_a_bounded_error() {
        struct Overreport {
            first: bool,
        }
        impl Read for Overreport {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.first {
                    self.first = false;
                    buffer[..3].copy_from_slice(b"a=1");
                    Ok(3)
                } else {
                    Ok(buffer.len() + 1)
                }
            }
        }
        assert_eq!(
            parse_reader(&mut Overreport { first: true }, Limits::default()).unwrap_err(),
            Error::Io(std::io::ErrorKind::InvalidData)
        );
        assert_eq!(
            fields(&parse(b"a=1", Limits::default()).unwrap()),
            vec![(b"a".to_vec(), b"1".to_vec())]
        );
    }

    #[test]
    fn deterministic_mutations_preserve_chunk_independent_results() {
        let seed = b"a%5Bx%5D=1&b=%00%FF&c=+plus+&a%5B%5D=two&bad=%2+";
        let limits = Limits {
            body_bytes: 1024,
            parts: 16,
            fields: 16,
            name_bytes: 1024,
            field_bytes: 1024,
            field_bytes_total: 1024,
            retained_bytes: 1024,
            memory_capacity: 64 * 1024,
            ..Limits::default()
        };
        let mut state = 0x0052_5553_5450_4f4e_u64;
        for case in 0..256 {
            let mut bytes = if case % 2 == 0 {
                seed.to_vec()
            } else {
                Vec::new()
            };
            if case % 2 == 0 {
                for _ in 0..4 {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let position = (state as usize) % bytes.len();
                    bytes[position] = (state >> 32) as u8;
                }
            } else {
                for _ in 0..(case % 127) {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    bytes.push((state >> 32) as u8);
                }
            }
            let baseline = parse(&bytes, limits).map(|request| fields(&request));
            for size in [1, 3, 17] {
                let mut parser = FormParser::new(limits).unwrap();
                let mut failure = None;
                for chunk in bytes.chunks(size) {
                    if let Err(error) = parser.feed(chunk) {
                        failure = Some(error);
                        break;
                    }
                }
                let actual = match failure {
                    Some(error) => Err(error),
                    None => parser.finish().map(|request| fields(&request)),
                };
                assert_eq!(actual, baseline, "case {case}, chunk {size}");
            }
        }
    }

    #[test]
    fn interrupted_reader_has_finite_budget_and_resumes_after_progress() {
        struct InterruptReader<'a> {
            bytes: &'a [u8],
            offset: usize,
            interrupt_next: bool,
            stall_after: Option<usize>,
        }
        impl Read for InterruptReader<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if self.stall_after.is_some_and(|cutoff| self.offset >= cutoff) {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                if self.interrupt_next {
                    self.interrupt_next = false;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                if self.offset == self.bytes.len() {
                    return Ok(0);
                }
                let limit = self.stall_after.unwrap_or(self.bytes.len());
                let end = (self.offset + 3).min(self.bytes.len()).min(limit);
                let count = end - self.offset;
                output[..count].copy_from_slice(&self.bytes[self.offset..end]);
                self.offset = end;
                self.interrupt_next = true;
                Ok(count)
            }
        }
        let bytes = b"a%5Bx%5D=1&b=two&c=%FF";
        let expected = fields(&parse(bytes, Limits::default()).unwrap());
        let mut recovering = InterruptReader {
            bytes,
            offset: 0,
            interrupt_next: true,
            stall_after: None,
        };
        assert_eq!(
            fields(&parse_reader(&mut recovering, Limits::default()).unwrap()),
            expected
        );
        let mut stalled = InterruptReader {
            bytes,
            offset: 0,
            interrupt_next: false,
            stall_after: Some(12),
        };
        assert_eq!(
            parse_reader(&mut stalled, Limits::default()).unwrap_err(),
            Error::Io(std::io::ErrorKind::Interrupted)
        );
        assert_eq!(fields(&parse(bytes, Limits::default()).unwrap()), expected);
    }
}
