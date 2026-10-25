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
        })
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
            match byte {
                b'&' => self.complete_field()?,
                b'=' if self.value_start.is_none() => {
                    self.value_start = Some(self.request().arena.len());
                }
                _ => self.raw_byte(byte)?,
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
        if bytes[read] == b'%' && read + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[read + 1]), hex(bytes[read + 2])) {
                bytes[written] = (hi << 4) | lo;
                read += 3;
                written += 1;
                continue;
            }
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
    let mut parser = FormParser::new(limits)?;
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return parser.finish(),
            Ok(n) => parser.feed(&buffer[..n])?,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
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
                    retained_bytes: 2,
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
}
