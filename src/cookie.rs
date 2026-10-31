//! Bounded PHP 8.4 HTTP Cookie-header scalar projection.
//!
//! The caller supplies one already-extracted, transport-validated Cookie
//! header value. This experimental slice rejects actual PHP array cookie
//! names; a name with an unterminated '[' is flattened to a scalar as in PHP
//! 8.4. Native SAPI integration and array registration are separate work.
//! PHP preserves the first value for a normalized scalar name.

pub mod array;

use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    HeaderBytes,
    Pairs,
    Nodes,
    NameBytes,
    ValueBytes,
    RetainedBytes,
    Allocation,
    InvalidHeader,
    UnsupportedArrayName,
    AppendExhausted,
    NestingLevel,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub header_bytes: usize,
    pub pairs: usize,
    /// Maximum cumulative array/root slots allocated by Cookie array projection.
    /// Slots replaced or removed later are still charged; not a PHP INI limit.
    pub nodes: usize,
    pub name_bytes: usize,
    pub value_bytes: usize,
    pub retained_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            header_bytes: 64 * 1024,
            pairs: 128,
            nodes: 4096,
            name_bytes: 1024,
            value_bytes: 8192,
            retained_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Cookie {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScalarCookies {
    entries: Vec<Cookie>,
}

impl ScalarCookies {
    pub fn entries(&self) -> &[Cookie] {
        &self.entries
    }

    /// Stable binary-safe JSON tuples: [hex-normalized-name, hex-value].
    /// This intentionally does not model the full PHP array key type system.
    pub fn write_json(&self, writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(b"[")?;
        for (index, entry) in self.entries.iter().enumerate() {
            if index != 0 {
                writer.write_all(b",")?;
            }
            writer.write_all(b"[\"")?;
            for byte in &entry.name {
                write!(writer, "{byte:02x}")?;
            }
            writer.write_all(b"\",\"")?;
            for byte in &entry.value {
                write!(writer, "{byte:02x}")?;
            }
            writer.write_all(b"\"]")?;
        }
        writer.write_all(b"]")
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

// Strict HTTP field values allow HTAB but not VT/FF. PHP 8.4 SAPI still
// registers Cookie brackets containing VT/FF as append indices, so retain
// those legacy cases while rejecting other controls and DEL, including
// after max_input_vars is exhausted. Upstream HTTP framing is authoritative.
fn valid_header_value(header: &[u8]) -> bool {
    header.iter().all(|&byte| {
        matches!(byte, b'\t' | b'\x0b' | b'\x0c')
            || (byte >= b' ' && byte != 0x7f)
    })
}

/// PHP Cookie registration raw-URL-decodes values but does not map '+' to
/// space and does not percent-decode names.
fn decode_cookie_value(raw: &[u8]) -> Result<Vec<u8>, Error> {
    let mut value = Vec::new();
    value
        .try_reserve_exact(raw.len())
        .map_err(|_| Error::Allocation)?;
    let mut offset = 0;
    while offset < raw.len() {
        if raw[offset] == b'%'
            && offset + 2 < raw.len()
            && let (Some(hi), Some(lo)) = (hex(raw[offset + 1]), hex(raw[offset + 2]))
        {
            value.push((hi << 4) | lo);
            offset += 3;
        } else {
            value.push(raw[offset]);
            offset += 1;
        }
    }
    Ok(value)
}

/// Parse scalar cookie names from a validated Cookie header. PHP flattens
/// unterminated '[' syntax into scalar underscores and ignores names whose
/// first non-whitespace byte is '['; completed bracket paths still require
/// array support and are rejected. First registered normalized name wins.
/// Hard input and retained-byte limits apply before copying values. Header
/// framing and multiple Cookie-header merging remain the transport's
/// responsibility.
pub fn parse_scalar_header(header: &[u8], limits: Limits) -> Result<ScalarCookies, Error> {
    parse_scalar_header_inner(header, limits, None)
}

/// Apply PHP 8.4 HTTP SAPI's max_input_vars admission to a Cookie header.
/// Zero admits no variables; duplicates and ignored bracket-root names still
/// consume slots. Independent hard byte/pair limits remain enforced.
pub fn parse_scalar_header_with_php_max_input_vars(
    header: &[u8],
    limits: Limits,
    max_input_vars: usize,
) -> Result<ScalarCookies, Error> {
    parse_scalar_header_inner(header, limits, Some(max_input_vars))
}

fn parse_scalar_header_inner(
    header: &[u8],
    limits: Limits,
    mut php_remaining: Option<usize>,
) -> Result<ScalarCookies, Error> {
    if header.len() > limits.header_bytes {
        return Err(Error::HeaderBytes);
    }
    // Reject malformed HTTP field bytes rather than attempting PHP's
    // transport-dependent behavior for invalid requests.
    if !valid_header_value(header) {
        return Err(Error::InvalidHeader);
    }

    let mut entries: Vec<Cookie> = Vec::new();
    let mut pairs = 0usize;
    let mut retained = 0usize;
    for segment in header.split(|&byte| byte == b';') {
        // A blank component does not register with PHP, but scanning it
        // still consumes parser work. Charge every wire component (including
        // empty components and a trailing separator) to the independent
        // hard pair budget. An entirely absent header has no components.
        if !header.is_empty() {
            pairs = pairs.checked_add(1).ok_or(Error::Pairs)?;
            if pairs > limits.pairs {
                return Err(Error::Pairs);
            }
        }
        let segment = segment.trim_ascii_start();
        if segment.is_empty() {
            continue;
        }
        let name_end = segment.iter().position(|&byte| byte == b'=');
        let (raw_name, raw_value) = match name_end {
            Some(pos) => (&segment[..pos], &segment[pos + 1..]),
            None => (segment, b"".as_slice()),
        };
        // The PHP variable quota stops registration, not independent hard
        // accounting for the total number of wire Cookie pairs.
        let mut skipped_by_php_quota = false;
        if !raw_name.is_empty()
            && let Some(remaining) = php_remaining.as_mut()
        {
            if *remaining == 0 {
                skipped_by_php_quota = true;
            } else {
                *remaining -= 1;
            }
        }
        if skipped_by_php_quota || raw_name.is_empty() {
            continue;
        }
        if raw_name.len() > limits.name_bytes {
            return Err(Error::NameBytes);
        }
        // php_register_variable_ex skips names with an empty root, even if
        // the opening bracket never closes. Count the wire pair but do not
        // allocate or publish a cookie for it.
        if raw_name.first() == Some(&b'[') {
            continue;
        }
        // An unclosed first bracket is *not* an array in PHP 8.4: its
        // remaining name bytes are flattened into a scalar identifier.
        // A closing bracket anywhere after the first '[' instead enters
        // actual PHP array handling, intentionally outside this slice.
        if let Some(open) = raw_name.iter().position(|&byte| byte == b'[')
            && raw_name[open + 1..].contains(&b']')
        {
            return Err(Error::UnsupportedArrayName);
        }
        if raw_value.len() > limits.value_bytes {
            return Err(Error::ValueBytes);
        }
        let mut name = Vec::new();
        name.try_reserve_exact(raw_name.len())
            .map_err(|_| Error::Allocation)?;
        // PHP replaces spaces/dots in scalar names and additionally
        // flattens each '[' when the first bracket never closes.
        name.extend(raw_name.iter().map(|&byte| {
            if matches!(byte, b' ' | b'.' | b'[') {
                b'_'
            } else {
                byte
            }
        }));
        if entries.iter().any(|entry| entry.name == name) {
            continue;
        }

        // Percent decoding can only shorten the value.
        let next = retained
            .checked_add(name.len())
            .and_then(|n| n.checked_add(raw_value.len()))
            .ok_or(Error::RetainedBytes)?;
        if next > limits.retained_bytes {
            return Err(Error::RetainedBytes);
        }
        entries.try_reserve(1).map_err(|_| Error::Allocation)?;
        entries.push(Cookie {
            name,
            value: decode_cookie_value(raw_value)?,
        });
        retained = next;
    }
    Ok(ScalarCookies { entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(header: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        parse_scalar_header(header, Limits::default())
            .unwrap()
            .entries()
            .iter()
            .map(|entry| (entry.name.clone(), entry.value.clone()))
            .collect()
    }

    #[test]
    fn php_cookie_scalars_are_first_wins_after_name_normalization() {
        assert_eq!(
            pairs(b"a.b=first; a_b=second; a=1; a=2"),
            vec![
                (b"a_b".to_vec(), b"first".to_vec()),
                (b"a".to_vec(), b"1".to_vec()),
            ]
        );
        assert_eq!(
            pairs(b"a_b=first; a.b=second"),
            vec![(b"a_b".to_vec(), b"first".to_vec())]
        );
    }

    #[test]
    fn php_cookie_decodes_values_but_not_names_or_pluses() {
        assert_eq!(
            pairs(b" a =hello+%2B%00%FF; %41=raw; no_equals; quoted=\"value\""),
            vec![
                (b"a_".to_vec(), b"hello++\0\xff".to_vec()),
                (b"%41".to_vec(), b"raw".to_vec()),
                (b"no_equals".to_vec(), Vec::new()),
                (b"quoted".to_vec(), b"\"value\"".to_vec()),
            ]
        );
        let output = parse_scalar_header(b"a=%FF", Limits::default()).unwrap();
        let mut json = Vec::new();
        output.write_json(&mut json).unwrap();
        assert_eq!(json, br#"[["61","ff"]]"#);
    }

    #[test]
    fn cookie_scalar_field_value_rejects_controls_but_preserves_htab_and_obs_text() {
        assert_eq!(
            pairs(b"a\tb=one; raw=\xff\x80"),
            vec![
                (b"a\tb".to_vec(), b"one".to_vec()),
                (b"raw".to_vec(), b"\xff\x80".to_vec()),
            ]
        );
        for control in [0x00, 0x01, 0x1f, 0x7f, b'\r', b'\n'] {
            let header = [b'a', b'=', control];
            assert_eq!(
                parse_scalar_header(&header, Limits::default()).unwrap_err(),
                Error::InvalidHeader
            );
            assert_eq!(
                parse_scalar_header_with_php_max_input_vars(&header, Limits::default(), 0)
                    .unwrap_err(),
                Error::InvalidHeader
            );
        }
    }

    #[test]
    fn php_cookie_unterminated_brackets_flatten_without_losing_first_wins() {
        assert_eq!(
            pairs(b"foo[bar=one; foo_bar=two; foo[[baz=3; no_equals[; foo[ a.b=4"),
            vec![
                (b"foo_bar".to_vec(), b"one".to_vec()),
                (b"foo__baz".to_vec(), b"3".to_vec()),
                (b"no_equals_".to_vec(), Vec::new()),
                (b"foo__a_b".to_vec(), b"4".to_vec()),
            ]
        );
        assert_eq!(
            pairs(b"foo_bar=first; foo[bar=second"),
            vec![(b"foo_bar".to_vec(), b"first".to_vec())]
        );
        assert_eq!(
            pairs(b"[bad=ignored; [okay]=ignored; a=1"),
            vec![(b"a".to_vec(), b"1".to_vec())]
        );
    }

    #[test]
    fn php_cookie_completed_array_path_still_fails_closed() {
        assert_eq!(
            parse_scalar_header(b"a=1; nested[child]=2", Limits::default()).unwrap_err(),
            Error::UnsupportedArrayName
        );
        assert_eq!(
            parse_scalar_header(b"a[child]tail=1", Limits::default()).unwrap_err(),
            Error::UnsupportedArrayName
        );
        let small = Limits {
            name_bytes: 3,
            ..Limits::default()
        };
        assert_eq!(
            parse_scalar_header(b"long[=x", small).unwrap_err(),
            Error::NameBytes
        );
    }

    #[test]
    fn php_cookie_header_boundaries_and_unsupported_arrays_fail_closed() {
        assert_eq!(
            parse_scalar_header(b"a=1; x[y]=2", Limits::default()).unwrap_err(),
            Error::UnsupportedArrayName
        );
        for invalid in [b"a=1\r\n".as_slice(), b"a=\0", b"a=1\n"] {
            assert_eq!(
                parse_scalar_header(invalid, Limits::default()).unwrap_err(),
                Error::InvalidHeader
            );
        }
        let tiny = Limits {
            header_bytes: 3,
            ..Limits::default()
        };
        assert_eq!(
            parse_scalar_header(b"a=12", tiny).unwrap_err(),
            Error::HeaderBytes
        );
        let quota = Limits {
            pairs: 2,
            ..Limits::default()
        };
        assert_eq!(
            parse_scalar_header(b"a=1;;b=2;c=3", quota).unwrap_err(),
            Error::Pairs
        );
    }

    #[test]
    fn blank_cookie_components_consume_hard_work_budget_without_php_registration() {
        let limits = Limits {
            pairs: 2,
            ..Limits::default()
        };
        // PHP 8.4 ignores blank Cookie components, including a leading ';'.
        // A finite parser work quota must still reject separator floods.
        assert_eq!(
            parse_scalar_header(b";;; a=1", limits).unwrap_err(),
            Error::Pairs
        );
        assert_eq!(
            parse_scalar_header_with_php_max_input_vars(b";;; a=1", limits, 0).unwrap_err(),
            Error::Pairs
        );
        let accepted = parse_scalar_header_with_php_max_input_vars(b"; a=one", limits, 1).unwrap();
        assert_eq!(accepted.entries().len(), 1);
        assert_eq!(accepted.entries()[0].name.as_slice(), b"a");
        assert_eq!(accepted.entries()[0].value.as_slice(), b"one");
        assert!(
            parse_scalar_header(
                b"",
                Limits {
                    pairs: 0,
                    ..Limits::default()
                }
            )
            .unwrap()
            .entries()
            .is_empty()
        );
    }

    #[test]
    fn php_cookie_variable_quota_counts_duplicates_and_ignored_names() {
        let limits = Limits::default();
        let result = parse_scalar_header_with_php_max_input_vars(
            b"=ignored; a=one; a=two; b=three",
            limits,
            2,
        )
        .unwrap();
        assert_eq!(
            result.entries(),
            &[Cookie {
                name: b"a".to_vec(),
                value: b"one".to_vec(),
            }]
        );
        let ignored =
            parse_scalar_header_with_php_max_input_vars(b"[ignored]=x; a=one; b=two", limits, 2)
                .unwrap();
        assert_eq!(
            ignored.entries(),
            &[Cookie {
                name: b"a".to_vec(),
                value: b"one".to_vec(),
            }]
        );
        assert!(
            parse_scalar_header_with_php_max_input_vars(b"a=one; b=two", limits, 0)
                .unwrap()
                .entries()
                .is_empty()
        );
        let small = Limits {
            name_bytes: 1,
            ..limits
        };
        // PHP quota stops registration before decoding oversized later names.
        assert!(
            parse_scalar_header_with_php_max_input_vars(
                b"a=1; very_long_after_the_php_quota=2",
                small,
                1,
            )
            .is_ok()
        );
    }

    #[test]
    fn php_cookie_variable_quota_cannot_bypass_hard_pair_budget() {
        let limits = Limits {
            pairs: 2,
            name_bytes: 1,
            ..Limits::default()
        };
        let accepted = parse_scalar_header_with_php_max_input_vars(
            b"a=1; very_long_skipped_name=2",
            limits,
            1,
        )
        .unwrap();
        assert_eq!(accepted.entries().len(), 1);

        // The skipped name bypasses PHP registration and name validation,
        // but its pair still consumes the parser's independent hard budget.
        assert_eq!(
            parse_scalar_header_with_php_max_input_vars(
                b"a=1; very_long_skipped_name=2; b=3",
                limits,
                1,
            )
            .unwrap_err(),
            Error::Pairs
        );
        assert_eq!(
            parse_scalar_header_with_php_max_input_vars(b"a=1; =ignored; b=3", limits, 1,)
                .unwrap_err(),
            Error::Pairs
        );
        assert_eq!(
            parse_scalar_header_with_php_max_input_vars(b"a=1; b=2; c=3", limits, 0).unwrap_err(),
            Error::Pairs
        );
    }

    #[test]
    fn php_cookie_bytes_and_retained_limits_are_independent() {
        for (limits, error) in [
            (
                Limits {
                    name_bytes: 1,
                    ..Limits::default()
                },
                Error::NameBytes,
            ),
            (
                Limits {
                    value_bytes: 1,
                    ..Limits::default()
                },
                Error::ValueBytes,
            ),
            (
                Limits {
                    retained_bytes: 3,
                    ..Limits::default()
                },
                Error::RetainedBytes,
            ),
        ] {
            assert_eq!(parse_scalar_header(b"ab=12", limits).unwrap_err(), error);
        }
        // A duplicate does not consume retained-value capacity twice, but
        // still consumes wire and pair quotas.
        let limits = Limits {
            retained_bytes: 3,
            ..Limits::default()
        };
        assert_eq!(
            pairs(b"a=1; a=longer"),
            vec![(b"a".to_vec(), b"1".to_vec())]
        );
        assert!(parse_scalar_header(b"a=1; a=longer", limits).is_ok());
    }
}
