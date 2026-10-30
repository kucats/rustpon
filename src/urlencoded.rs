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
    // Opt-in PHP parse_str() behavior: decoded NUL terminates the name.
    php_nul_name_truncation: bool,
    // Preserve syntactically present but empty decoded names so the POST
    // adapter can count them toward PHP max_input_vars in wire order.
    php_count_ignored_names: bool,
    // PHP 8.4 HTTP SAPI counts even separator-only empty pairs toward
    // max_input_vars, unlike CLI parse_str(). Only opt in for SAPI quotas.
    php_count_blank_pairs: bool,
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
            php_nul_name_truncation: false,
            php_count_ignored_names: false,
            php_count_blank_pairs: false,
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

    /// PHP 8.4 parse_str() truncates a percent-decoded variable name at its
    /// first NUL byte. This is opt-in: the strict parser rejects such names.
    /// Raw input limits are still enforced before any decoding or truncation.
    pub fn with_php_nul_name_truncation(mut self) -> Self {
        self.php_nul_name_truncation = true;
        self
    }

    /// PHP counts nonblank pairs with empty decoded names against
    /// max_input_vars even though they do not appear in $_POST. Preserve
    /// those pairs as empty-name wire entries for the projection adapter.
    /// This is opt-in; the strict wire parser continues to discard them.
    pub fn with_php_input_var_counting(mut self) -> Self {
        self.php_count_ignored_names = true;
        self
    }

    fn with_php_sapi_blank_pair_counting(mut self) -> Self {
        self.php_count_ignored_names = true;
        self.php_count_blank_pairs = true;
        self
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
        if self.field_start == value_start
            && !(self.php_count_ignored_names && self.value_start.is_some())
            && !self.php_count_blank_pairs
        {
            // Blank pairs never count. In strict mode empty names are also
            // discarded; PHP counting mode retains explicit '=value' pairs.
            let discard_from = self.field_start;
            self.request().arena.truncate(discard_from);
        } else {
            let field_start = self.field_start;
            let truncate_nul = self.php_nul_name_truncation;
            let (name_end, value_end) = {
                let arena = &mut self.request().arena;
                let decoded_name = url_decode(&mut arena[field_start..value_start]);
                let name_len = if truncate_nul {
                    arena[field_start..field_start + decoded_name]
                        .iter()
                        .position(|&byte| byte == 0)
                        .unwrap_or(decoded_name)
                } else {
                    decoded_name
                };
                let name_end = field_start + name_len;
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
            if name_end == field_start && !self.php_count_ignored_names {
                // Strict wire IR omits names that decode to empty.
                self.request().arena.truncate(field_start);
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
    parse_reader_impl(reader, limits, b"&", false, false)
}

/// Opt-in PHP 8.4 `arg_separator.input='&;'` parsing for trusted SAPI settings.
/// The strict default entry point continues to use only '&'.
pub fn parse_reader_with_php_semicolon_separator(
    reader: &mut impl Read,
    limits: Limits,
) -> Result<Request, Error> {
    parse_reader_impl(reader, limits, b"&;", false, false)
}

/// Parse using PHP's configured arg_separator.input byte set. The empty
/// setting follows PHP's fallback to '&'; this API does not parse INI syntax.
pub fn parse_reader_with_php_arg_separators(
    reader: &mut impl Read,
    limits: Limits,
    separators: &[u8],
) -> Result<Request, Error> {
    parse_reader_impl(reader, limits, separators, false, false)
}

/// Opt-in PHP 8.4 parse_str() handling of percent-decoded NULs in field
/// names, with the same configurable raw separator set. The strict default
/// rejects decoded NULs rather than silently shortening a variable name.
pub fn parse_reader_with_php_nul_name_truncation(
    reader: &mut impl Read,
    limits: Limits,
    separators: &[u8],
) -> Result<Request, Error> {
    parse_reader_impl(reader, limits, separators, true, false)
}

/// Opt-in PHP max_input_vars-compatible accounting for empty variable names.
/// Blank separator-only pairs remain ignored; explicit '=value' pairs and
/// percent-decoded NUL names occupy an input-variable slot without becoming
/// visible POST entries. Use with into_post_with_max_input_vars or
/// into_post_with_php_limits; independent wire resource limits still apply.
pub fn parse_reader_with_php_input_var_counting(
    reader: &mut impl Read,
    limits: Limits,
    separators: &[u8],
    php_nul_name_truncation: bool,
) -> Result<Request, Error> {
    parse_reader_impl(reader, limits, separators, php_nul_name_truncation, true)
}

/// HTTP GET's raw QUERY_STRING is parsed with the caller's arg_separator.input
/// set (empty means '&'); unlike URL-encoded HTTP POST it honors that INI
/// value. Names truncate at decoded NUL, as in PHP 8.4 SAPI registration.
/// The caller validates/extracts the query string and enforces transport limits.
pub fn parse_reader_with_php_query_string(
    reader: &mut impl Read,
    limits: Limits,
    arg_separators: &[u8],
) -> Result<Request, Error> {
    parse_reader_impl(reader, limits, arg_separators, true, false)
}

/// GET max_input_vars counts nonblank wire pairs, even ignored empty names.
/// Blank separator-only pairs are excluded. Project with the exact quota N,
/// not the N+1 allowance measured for PHP 8.4 HTTP POST.
pub fn parse_reader_with_php_query_string_counting_vars(
    reader: &mut impl Read,
    limits: Limits,
    arg_separators: &[u8],
) -> Result<Request, Error> {
    parse_reader_impl(reader, limits, arg_separators, true, true)
}

/// Parse exactly `query_length` bytes of an already-extracted PHP GET
/// QUERY_STRING. The reader is not advanced into a subsequent request, even
/// when more data is available. An early EOF is a transactional Truncated
/// error; the independent body quota is checked before reading any bytes.
/// The caller remains responsible for HTTP framing and header validation.
pub fn parse_reader_with_php_query_string_length(
    reader: &mut impl Read,
    limits: Limits,
    query_length: u64,
    arg_separators: &[u8],
) -> Result<Request, Error> {
    parse_reader_with_php_query_string_length_impl(
        reader,
        limits,
        query_length,
        arg_separators,
        false,
    )
}

/// Length-bounded GET parsing with PHP max_input_vars accounting, including
/// ignored empty names. Use the exact GET variable quota during projection.
pub fn parse_reader_with_php_query_string_length_counting_vars(
    reader: &mut impl Read,
    limits: Limits,
    query_length: u64,
    arg_separators: &[u8],
) -> Result<Request, Error> {
    parse_reader_with_php_query_string_length_impl(
        reader,
        limits,
        query_length,
        arg_separators,
        true,
    )
}

fn parse_reader_with_php_query_string_length_impl(
    reader: &mut impl Read,
    limits: Limits,
    query_length: u64,
    arg_separators: &[u8],
    count_ignored_names: bool,
) -> Result<Request, Error> {
    if query_length > limits.body_bytes {
        return Err(Error::Limit(Limit::BodyBytes));
    }
    if query_length == 0 {
        return Ok(Request {
            arena: Vec::new(),
            parts: Vec::new(),
            stats: Stats::default(),
        });
    }
    let mut bounded = reader.take(query_length);
    let request = parse_reader_impl(&mut bounded, limits, arg_separators, true, count_ignored_names)?;
    if bounded.limit() != 0 {
        return Err(Error::Truncated);
    }
    Ok(request)
}

fn parse_reader_impl(
    reader: &mut impl Read,
    limits: Limits,
    separators: &[u8],
    php_nul_name_truncation: bool,
    php_count_ignored_names: bool,
) -> Result<Request, Error> {
    let mut parser = FormParser::new(limits)?.with_php_arg_separators(separators);
    if php_count_ignored_names {
        parser = parser.with_php_input_var_counting();
    }
    if php_nul_name_truncation {
        parser = parser.with_php_nul_name_truncation();
    }
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

/// Parse a URL-encoded HTTP request body using a transport-validated
/// Content-Length. The reader is never advanced beyond the declared length.
/// This is a bounded PHP 8.4-style admission adapter, not an HTTP parser or
/// a replacement for Zend's SAPI input handling.
///
/// A zero length publishes an empty request. A nonzero PHP post_max_size
/// exceeded by Content-Length also publishes an empty request, after draining
/// exactly that many bytes without allocating a parser or projecting fields.
/// The independent hard body limit is checked before reading. Early EOF,
/// malformed readers and persistent interruptions fail closed.
///
/// Accepted bodies use PHP's decoded-NUL name truncation and retain ignored
/// empty names for later max_input_vars accounting. The caller must validate
/// HTTP framing and reject ambiguous Content-Length headers upstream.
pub fn parse_reader_with_php_content_length(
    reader: &mut impl Read,
    limits: Limits,
    content_length: u64,
    php_post_max_bytes: Option<u64>,
) -> Result<Request, Error> {
    parse_reader_with_php_content_length_impl(
        reader,
        limits,
        content_length,
        php_post_max_bytes,
        false,
    )
}

/// PHP 8.4 HTTP SAPI max_input_vars accounting differs from CLI parse_str():
/// SAPI counts separator-only blank pairs and admits up to N+1 tokens for a
/// configured limit N. Preserve those blank wire entries here; the caller
/// must pass max_input_vars.saturating_add(1) to the POST projection.
/// Without a SAPI variable quota, prefer parse_reader_with_php_content_length.
pub fn parse_reader_with_php_content_length_sapi_vars(
    reader: &mut impl Read,
    limits: Limits,
    content_length: u64,
    php_post_max_bytes: Option<u64>,
) -> Result<Request, Error> {
    parse_reader_with_php_content_length_impl(
        reader,
        limits,
        content_length,
        php_post_max_bytes,
        true,
    )
}

fn parse_reader_with_php_content_length_impl(
    reader: &mut impl Read,
    limits: Limits,
    content_length: u64,
    php_post_max_bytes: Option<u64>,
    sapi_count_blank_pairs: bool,
) -> Result<Request, Error> {
    if content_length > limits.body_bytes {
        return Err(Error::Limit(Limit::BodyBytes));
    }
    if content_length == 0 {
        return Ok(Request {
            arena: Vec::new(),
            parts: Vec::new(),
            stats: Stats::default(),
        });
    }
    let over_php_limit = php_post_max_bytes.is_some_and(|max| max != 0 && content_length > max);
    let mut parser = if over_php_limit {
        None
    } else {
        let mut parser = FormParser::new(limits)?
            .with_php_nul_name_truncation()
            .with_php_input_var_counting();
        if sapi_count_blank_pairs {
            parser = parser.with_php_sapi_blank_pair_counting();
        }
        Some(parser)
    };
    let mut remaining = content_length;
    let mut buffer = [0u8; 8192];
    let mut consecutive_interrupts = 0usize;
    while remaining != 0 {
        let allowed = remaining.min(buffer.len() as u64) as usize;
        match reader.read(&mut buffer[..allowed]) {
            Ok(0) => return Err(Error::Truncated),
            Ok(count) if count > allowed => {
                return Err(Error::Io(std::io::ErrorKind::InvalidData));
            }
            Ok(count) => {
                remaining -= count as u64;
                consecutive_interrupts = 0;
                if let Some(parser) = parser.as_mut() {
                    parser.feed(&buffer[..count])?;
                }
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
    match parser {
        Some(parser) => parser.finish(),
        None => Ok(Request {
            arena: Vec::new(),
            parts: Vec::new(),
            stats: Stats {
                body_bytes: content_length,
                ..Stats::default()
            },
        }),
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
    fn php_query_string_honors_ini_separators_and_decoded_nul_names() {
        let body = b"a=1|n%00tail=2;%7C=encoded&x=3";
        let request =
            parse_reader_with_php_query_string(&mut Cursor::new(body), Limits::default(), b"|;")
                .unwrap();
        assert_eq!(
            fields(&request),
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"n".to_vec(), b"2".to_vec()),
                (b"|".to_vec(), b"encoded&x=3".to_vec()),
            ]
        );
        let fallback = parse_reader_with_php_query_string(
            &mut Cursor::new(b"a=1|b=2&c=3"),
            Limits::default(),
            b"",
        )
        .unwrap();
        assert_eq!(
            fields(&fallback),
            vec![
                (b"a".to_vec(), b"1|b=2".to_vec()),
                (b"c".to_vec(), b"3".to_vec()),
            ]
        );
    }

    #[test]
    fn php_query_string_quota_charges_empty_names_not_blank_pairs() {
        let request = parse_reader_with_php_query_string_counting_vars(
            &mut Cursor::new(b"a=1||=ignored;b=2|c=3"),
            Limits::default(),
            b"|;",
        )
        .unwrap();
        assert_eq!(
            fields(&request),
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"".to_vec(), b"ignored".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
                (b"c".to_vec(), b"3".to_vec()),
            ]
        );
        let limited = request
            .into_post_with_max_input_vars(post::Limits::default(), 2)
            .unwrap();
        let mut output = Vec::new();
        limited.write_json(&mut output).unwrap();
        let json = String::from_utf8(output).unwrap();
        assert!(json.contains(r#"["s","61"],["s","31"]"#));
        assert!(!json.contains(r#"["s","62"]"#));
        assert!(!json.contains(r#"["s","63"]"#));
    }

    #[test]
    fn php_query_string_length_is_exact_and_transactional() {
        let query = b"a=1|b=2";
        let mut wire = query.to_vec();
        wire.extend_from_slice(b"|unexpected=3");
        let mut reader = Cursor::new(wire);
        let request = parse_reader_with_php_query_string_length(
            &mut reader,
            Limits::default(),
            query.len() as u64,
            b"|",
        )
        .unwrap();
        assert_eq!(reader.position(), query.len() as u64);
        assert_eq!(request.stats().body_bytes, query.len() as u64);
        assert_eq!(
            fields(&request),
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
            ]
        );

        let mut short = Cursor::new(b"a=1".to_vec());
        assert_eq!(
            parse_reader_with_php_query_string_length(
                &mut short,
                Limits::default(),
                4,
                b"&",
            )
            .unwrap_err(),
            Error::Truncated
        );

        let mut oversized = Cursor::new(b"a=1".to_vec());
        assert_eq!(
            parse_reader_with_php_query_string_length(
                &mut oversized,
                Limits {
                    body_bytes: 2,
                    ..Limits::default()
                },
                3,
                b"&",
            )
            .unwrap_err(),
            Error::Limit(Limit::BodyBytes)
        );
        assert_eq!(oversized.position(), 0);

        let mut empty = Cursor::new(b"NEXT".to_vec());
        let request = parse_reader_with_php_query_string_length(
            &mut empty,
            Limits::default(),
            0,
            b"&",
        )
        .unwrap();
        assert_eq!(empty.position(), 0);
        assert_eq!(request.parts().len(), 0);
    }

    #[test]
    fn php_query_string_length_keeps_empty_name_quota_positions() {
        let query = b"a=1|=ignored|b=2|c=3";
        let mut reader = Cursor::new(query);
        let request = parse_reader_with_php_query_string_length_counting_vars(
            &mut reader,
            Limits::default(),
            query.len() as u64,
            b"|",
        )
        .unwrap();
        let limited = request
            .into_post_with_max_input_vars(post::Limits::default(), 2)
            .unwrap();
        let mut output = Vec::new();
        limited.write_json(&mut output).unwrap();
        let json = String::from_utf8(output).unwrap();
        assert!(json.contains(r#"["s","61"],["s","31"]"#));
        assert!(!json.contains(r#"["s","62"]"#));
        assert!(!json.contains(r#"["s","63"]"#));
    }

    #[test]
    fn php_content_length_admission_is_exact_and_preserves_next_message() {
        // PHP 8.4 CLI parse_str() projects x%00tail as x and b[] as b[0].
        let body = b"a=1&x%00tail=2&b%5B%5D=3";
        let mut wire = body.to_vec();
        wire.extend_from_slice(b"NEXT");
        let mut reader = Cursor::new(wire);
        let request = parse_reader_with_php_content_length(
            &mut reader,
            Limits::default(),
            body.len() as u64,
            Some(body.len() as u64),
        )
        .unwrap();
        assert_eq!(reader.position(), body.len() as u64);
        assert_eq!(request.stats().body_bytes, body.len() as u64);
        assert_eq!(
            fields(&request),
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"x".to_vec(), b"2".to_vec()),
                (b"b[]".to_vec(), b"3".to_vec()),
            ]
        );

        let mut reader = Cursor::new(b"=ignored&a=1".to_vec());
        let request =
            parse_reader_with_php_content_length(&mut reader, Limits::default(), 12, Some(0))
                .unwrap();
        assert_eq!(fields(&request)[0].0, b"".to_vec());
    }

    #[test]
    fn php_content_length_preflight_drains_without_parsing_or_overreading() {
        let mut reader = Cursor::new(b"bad=%00&still=hereNEXT".to_vec());
        let request =
            parse_reader_with_php_content_length(&mut reader, Limits::default(), 18, Some(17))
                .unwrap();
        assert_eq!(reader.position(), 18);
        assert_eq!(request.stats().body_bytes, 18);
        assert_eq!(request.stats().parts, 0);
        assert!(fields(&request).is_empty());

        let mut reader = Cursor::new(b"a=1".to_vec());
        let request =
            parse_reader_with_php_content_length(&mut reader, Limits::default(), 0, Some(1))
                .unwrap();
        assert_eq!(reader.position(), 0);
        assert!(fields(&request).is_empty());

        let mut reader = Cursor::new(b"a=1".to_vec());
        assert_eq!(
            parse_reader_with_php_content_length(
                &mut reader,
                Limits {
                    body_bytes: 2,
                    ..Limits::default()
                },
                3,
                None,
            )
            .unwrap_err(),
            Error::Limit(Limit::BodyBytes)
        );
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn php_http_sapi_var_counting_retains_blank_pairs_in_wire_order() {
        let body = b"&&a=1&b=2&c=3";
        let mut reader = Cursor::new(body);
        let request = parse_reader_with_php_content_length_sapi_vars(
            &mut reader,
            Limits::default(),
            body.len() as u64,
            None,
        )
        .unwrap();
        assert_eq!(reader.position(), body.len() as u64);
        assert_eq!(
            fields(&request),
            vec![
                (Vec::new(), Vec::new()),
                (Vec::new(), Vec::new()),
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
                (b"c".to_vec(), b"3".to_vec()),
            ]
        );
        // HTTP SAPI max_input_vars=2 accepts three wire tokens, including
        // the two empty pairs. Only a is published.
        let post = request
            .into_post_with_max_input_vars(post::Limits::default(), 3)
            .unwrap();
        let mut json = Vec::new();
        post.write_json(&mut json).unwrap();
        let json = String::from_utf8(json).unwrap();
        assert!(json.contains(r#"["s","61"],["s","31"]"#));
        assert!(!json.contains(r#"["s","62"]"#));

        // The ordinary Content-Length API retains parse_str-like blank
        // suppression when no SAPI max_input_vars policy is requested.
        let request = parse_reader_with_php_content_length(
            &mut Cursor::new(body),
            Limits::default(),
            body.len() as u64,
            None,
        )
        .unwrap();
        assert_eq!(request.parts().len(), 3);
    }

    #[test]
    fn php_content_length_rejects_short_and_misbehaving_readers() {
        for php_max in [None, Some(1)] {
            let mut short = Cursor::new(b"a=1".to_vec());
            assert_eq!(
                parse_reader_with_php_content_length(&mut short, Limits::default(), 4, php_max,)
                    .unwrap_err(),
                Error::Truncated
            );
        }
        struct Overreport;
        impl Read for Overreport {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                Ok(buffer.len() + 1)
            }
        }
        assert_eq!(
            parse_reader_with_php_content_length(&mut Overreport, Limits::default(), 3, None,)
                .unwrap_err(),
            Error::Io(std::io::ErrorKind::InvalidData)
        );
        assert_eq!(
            parse_reader_with_php_content_length(&mut Overreport, Limits::default(), 3, Some(1),)
                .unwrap_err(),
            Error::Io(std::io::ErrorKind::InvalidData)
        );
    }

    #[test]
    fn php_decoded_nul_names_are_opt_in_and_chunk_independent() {
        let body = b"a%00suffix=one&%00empty=ignored&a%5Bx%00y%5D=two&z=hi%00there";
        assert_eq!(
            parse(body, Limits::default()).unwrap_err(),
            Error::Malformed
        );
        let expected = vec![
            (b"a".to_vec(), b"one".to_vec()),
            (b"a[x".to_vec(), b"two".to_vec()),
            (b"z".to_vec(), b"hi\0there".to_vec()),
        ];
        let parsed = parse_reader_with_php_nul_name_truncation(
            &mut Cursor::new(body),
            Limits::default(),
            b"&",
        )
        .unwrap();
        assert_eq!(fields(&parsed), expected);
        assert_eq!(parsed.stats().fields, 3);
        for split in 0..=body.len() {
            let mut parser = FormParser::new(Limits::default())
                .unwrap()
                .with_php_nul_name_truncation();
            parser.feed(&body[..split]).unwrap();
            parser.feed(&body[split..]).unwrap();
            assert_eq!(fields(&parser.finish().unwrap()), expected, "split {split}");
        }
        let pipe = parse_reader_with_php_nul_name_truncation(
            &mut Cursor::new(b"a%00b=1|b=2"),
            Limits::default(),
            b"|",
        )
        .unwrap();
        assert_eq!(
            fields(&pipe),
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
            ]
        );
        // Truncation must never bypass raw-name or body resource budgets.
        let limits = Limits {
            name_bytes: 4,
            ..Limits::default()
        };
        assert_eq!(
            parse_reader_with_php_nul_name_truncation(
                &mut Cursor::new(b"a%00long=1"),
                limits,
                b"&",
            )
            .unwrap_err(),
            Error::Limit(Limit::NameBytes)
        );
        let limits = Limits {
            parts: 1,
            fields: 1,
            ..Limits::default()
        };
        let parsed = parse_reader_with_php_nul_name_truncation(
            &mut Cursor::new(b"%00drop=1&a=2"),
            limits,
            b"&",
        )
        .unwrap();
        assert_eq!(fields(&parsed), vec![(b"a".to_vec(), b"2".to_vec())]);
    }

    #[test]
    fn php_input_var_counting_retains_ignored_names_in_wire_order() {
        let body = b"a=1&=ignored&b=2";
        let request = parse_reader_with_php_input_var_counting(
            &mut Cursor::new(body),
            Limits::default(),
            b"&",
            false,
        )
        .unwrap();
        assert_eq!(
            fields(&request),
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (Vec::new(), b"ignored".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
            ]
        );
        let post = request
            .into_post_with_max_input_vars(post::Limits::default(), 2)
            .unwrap();
        let mut json = Vec::new();
        post.write_json(&mut json).unwrap();
        let json = String::from_utf8(json).unwrap();
        assert!(json.contains(r#"["s","61"],["s","31"]"#));
        assert!(!json.contains(r#"["s","62"]"#));

        // A separator-only empty pair does not consume a PHP variable slot.
        let request = parse_reader_with_php_input_var_counting(
            &mut Cursor::new(b"a=1&&b=2"),
            Limits::default(),
            b"&",
            false,
        )
        .unwrap();
        assert_eq!(request.parts().len(), 2);
        assert_eq!(
            request
                .into_post_with_max_input_vars(post::Limits::default(), 2)
                .unwrap()
                .stats()
                .fields,
            2
        );

        // NUL-truncated empty names also consume a slot, even though the
        // resulting $_POST has no entry for that name.
        let request = parse_reader_with_php_input_var_counting(
            &mut Cursor::new(b"%00discard=1&a=2"),
            Limits::default(),
            b"&",
            true,
        )
        .unwrap();
        assert_eq!(fields(&request)[0].0, b"");
        let post = request
            .into_post_with_max_input_vars(post::Limits::default(), 1)
            .unwrap();
        let mut json = Vec::new();
        post.write_json(&mut json).unwrap();
        assert_eq!(json, b"[\"a\",[]]");

        // Every possible two-feed split must yield the same projection.
        for split in 0..=body.len() {
            let mut parser = FormParser::new(Limits::default())
                .unwrap()
                .with_php_input_var_counting();
            parser.feed(&body[..split]).unwrap();
            parser.feed(&body[split..]).unwrap();
            assert_eq!(fields(&parser.finish().unwrap())[1].0, b"", "split {split}");
        }
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
