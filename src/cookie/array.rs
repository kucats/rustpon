//! Experimental bounded PHP 8.4 Cookie registration with configurable nesting.
//!
//! PHP 8.4 defaults max_input_nesting_level to 64; explicit settings up to
//! 128 are supported. A deeper path removes
//! its previously registered root without publishing a partial value.
//! PHP 8.4 ignores trailing text and incomplete later brackets after a
//! complete array path. This is not a Zend/SAPI registration API.

use super::{Error, Limits, decode_cookie_value, valid_header_value};
use std::io::{self, Write};

const MAX_ARRAY_DEPTH: usize = 128;
const DEFAULT_PHP_DEPTH: usize = 64;

#[derive(Debug, PartialEq, Eq)]
enum Key {
    Bytes(Vec<u8>),
    Integer(i64),
}

#[derive(Debug, PartialEq, Eq)]
struct Element {
    key: Key,
    value: Value,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Array {
    elements: Vec<Element>,
    next_index: Option<i64>,
}

impl Array {
    fn advance_next_index(&mut self, key: &Key) {
        // Preserve PHP 8.4's numeric append cursor even for repeated keys.
        if let Key::Integer(number) = key
            && self.next_index.is_none_or(|next| *number >= next)
        {
            self.next_index = Some(number.saturating_add(1));
        }
    }

    fn insert(
        &mut self,
        key: Key,
        value: Vec<u8>,
        nodes: &mut usize,
        max_nodes: usize,
    ) -> Result<(), Error> {
        self.advance_next_index(&key);
        if let Some(existing) = self.elements.iter_mut().find(|entry| entry.key == key) {
            existing.value = Value::Scalar(value);
        } else {
            charge_node(nodes, max_nodes)?;
            self.elements
                .try_reserve(1)
                .map_err(|_| Error::Allocation)?;
            self.elements.push(Element {
                key,
                value: Value::Scalar(value),
            });
        }
        Ok(())
    }

    fn append_index(&self) -> Result<i64, Error> {
        let index = self.next_index.unwrap_or(0);
        // An exhausted append must not overwrite a pre-existing i64::MAX.
        if self
            .elements
            .iter()
            .any(|entry| entry.key == Key::Integer(index))
        {
            return Err(Error::AppendExhausted);
        }
        Ok(index)
    }

    fn append(&mut self, value: Vec<u8>, nodes: &mut usize, max_nodes: usize) -> Result<(), Error> {
        let index = self.append_index()?;
        self.insert(Key::Integer(index), value, nodes, max_nodes)
    }

    fn nested_mut(
        &mut self,
        key: Key,
        nodes: &mut usize,
        max_nodes: usize,
    ) -> Result<&mut Array, Error> {
        self.advance_next_index(&key);
        let position =
            if let Some(position) = self.elements.iter().position(|entry| entry.key == key) {
                position
            } else {
                charge_node(nodes, max_nodes)?;
                self.elements
                    .try_reserve(1)
                    .map_err(|_| Error::Allocation)?;
                self.elements.push(Element {
                    key,
                    value: Value::Array(Array::default()),
                });
                self.elements.len() - 1
            };
        // PHP promotes a scalar child to an empty array when a later
        // bracket path descends into it.
        if matches!(self.elements[position].value, Value::Scalar(_)) {
            self.elements[position].value = Value::Array(Array::default());
        }
        let Value::Array(array) = &mut self.elements[position].value else {
            unreachable!();
        };
        Ok(array)
    }

    fn append_nested(&mut self, nodes: &mut usize, max_nodes: usize) -> Result<&mut Array, Error> {
        let index = self.append_index()?;
        self.nested_mut(Key::Integer(index), nodes, max_nodes)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Value {
    Scalar(Vec<u8>),
    Array(Array),
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    key: Key,
    value: Value,
}

/// Owns a bounded ordered projection of one validated Cookie header.
/// This slice models scalar roots and up to 128 configured bracket levels.
#[derive(Debug, PartialEq, Eq)]
pub struct CookieTree {
    entries: Vec<Entry>,
}

fn numeric_key(bytes: &[u8]) -> Option<i64> {
    let digits = bytes.strip_prefix(b"-").unwrap_or(bytes);
    if digits.is_empty()
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits[0] == b'0' && bytes.len() > 1)
    {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse::<i64>().ok()
}

fn key(bytes: &[u8]) -> Result<Key, Error> {
    if let Some(number) = numeric_key(bytes) {
        return Ok(Key::Integer(number));
    }
    let mut result = Vec::new();
    result
        .try_reserve_exact(bytes.len())
        .map_err(|_| Error::Allocation)?;
    result.extend_from_slice(bytes);
    Ok(Key::Bytes(result))
}

fn root_key(raw: &[u8], flatten_open_brackets: bool) -> Result<Key, Error> {
    let mut normalized = Vec::new();
    normalized
        .try_reserve_exact(raw.len())
        .map_err(|_| Error::Allocation)?;
    normalized.extend(raw.iter().map(|&byte| {
        if matches!(byte, b' ' | b'.') || (flatten_open_brackets && byte == b'[') {
            b'_'
        } else {
            byte
        }
    }));
    if let Some(number) = numeric_key(&normalized) {
        Ok(Key::Integer(number))
    } else {
        Ok(Key::Bytes(normalized))
    }
}

fn charge_node(nodes: &mut usize, max_nodes: usize) -> Result<(), Error> {
    *nodes = nodes
        .checked_add(1)
        .filter(|count| *count <= max_nodes)
        .ok_or(Error::Nodes)?;
    Ok(())
}

fn charge(retained: &mut usize, bytes: usize, limits: Limits) -> Result<(), Error> {
    *retained = retained
        .checked_add(bytes)
        .filter(|value| *value <= limits.retained_bytes)
        .ok_or(Error::RetainedBytes)?;
    Ok(())
}

/// Convert an already-extracted single Cookie header into a PHP-style tree.
/// Bracket paths and append keys are supported through the PHP 8.4 default
/// max_input_nesting_level of 64, independently of input-variable quotas.
/// Deeper paths remove their normalized root, matching PHP HTTP SAPI.
/// One ASCII whitespace byte also acts as an append key.
/// A trailing suffix after the final ']' is ignored, including an opening
/// bracket without a matching close. Hard header/pair/name/value limits remain.
pub fn parse_one_level_header(header: &[u8], limits: Limits) -> Result<CookieTree, Error> {
    parse_one_level_header_inner(header, limits, None, DEFAULT_PHP_DEPTH)
}

/// Apply PHP 8.4 HTTP SAPI's Cookie max_input_vars quota before registering
/// nonempty raw names, including duplicates and ignored bracket-root names.
pub fn parse_one_level_header_with_php_max_input_vars(
    header: &[u8],
    limits: Limits,
    max_input_vars: usize,
) -> Result<CookieTree, Error> {
    parse_one_level_header_inner(header, limits, Some(max_input_vars), DEFAULT_PHP_DEPTH)
}

/// Model both PHP 8.4 HTTP SAPI INI limits. A nesting value of zero
/// rejects every array path by discarding its existing normalized root,
/// while scalar cookies remain admissible. The independent hard cap of 128
/// levels is intentional: larger PHP configurations return NestingLevel
/// rather than silently truncating registration.
pub fn parse_one_level_header_with_php_ini_limits(
    header: &[u8],
    limits: Limits,
    max_input_vars: Option<usize>,
    max_input_nesting_level: usize,
) -> Result<CookieTree, Error> {
    parse_one_level_header_inner(header, limits, max_input_vars, max_input_nesting_level)
}

fn is_append_index(index: &[u8]) -> bool {
    index.is_empty()
        || (index.len() == 1
            && matches!(index[0], b' ' | b'\t' | b'\x0b' | b'\x0c' | b'\r' | b'\n'))
}

fn parse_one_level_header_inner(
    header: &[u8],
    limits: Limits,
    mut php_remaining: Option<usize>,
    max_input_nesting_level: usize,
) -> Result<CookieTree, Error> {
    if max_input_nesting_level > MAX_ARRAY_DEPTH {
        return Err(Error::NestingLevel);
    }
    if header.len() > limits.header_bytes {
        return Err(Error::HeaderBytes);
    }
    // Validate even when PHP's variable quota would skip every registration.
    if !valid_header_value(header) {
        return Err(Error::InvalidHeader);
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut pairs = 0usize;
    let mut retained = 0usize;
    // Count every structural slot ever inserted, including superseded ones.
    // This bounds allocation amplification independently of raw byte quotas.
    let mut nodes = 0usize;

    'segments: for segment in header.split(|&byte| byte == b';') {
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
        let (raw_name, raw_value) = match segment.iter().position(|&byte| byte == b'=') {
            Some(position) => (&segment[..position], &segment[position + 1..]),
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
        if raw_name.first() == Some(&b'[') {
            continue;
        }
        let bracket = raw_name
            .iter()
            .position(|&byte| byte == b'[')
            .and_then(|open| {
                raw_name[open + 1..]
                    .iter()
                    .position(|&byte| byte == b']')
                    .map(|offset| (open, open + 1 + offset))
            });
        if let Some((open, close)) = bracket {
            // Fixed stack storage avoids a separate allocation for bracket
            // paths while bounding parser work at the supported nesting cap.
            let mut indexes: [&[u8]; MAX_ARRAY_DEPTH] = [&[]; MAX_ARRAY_DEPTH];
            indexes[0] = &raw_name[open + 1..close];
            let mut depth = 1;
            let mut trailing = &raw_name[close + 1..];
            let mut too_deep = depth > max_input_nesting_level;
            while !too_deep {
                let Some(rest) = trailing.strip_prefix(b"[") else {
                    break;
                };
                // PHP counts a directly following opening bracket against
                // max_input_nesting_level even when its closing ']' is absent.
                // An over-depth name removes the already registered root.
                if depth >= max_input_nesting_level || depth == MAX_ARRAY_DEPTH {
                    too_deep = true;
                    break;
                }
                // If capacity remains, an unfinished suffix registers only
                // the preceding complete path (the PHP 8.4 behavior).
                let Some(end) = rest.iter().position(|&byte| byte == b']') else {
                    break;
                };
                indexes[depth] = &rest[..end];
                depth += 1;
                trailing = &rest[end + 1..];
                too_deep = depth > max_input_nesting_level;
            }
            if too_deep {
                // php_register_variable_ex removes an existing normalized
                // root on max_input_nesting_level overflow, then continues.
                // The ignored wire pair still counts toward max_input_vars.
                let root = root_key(&raw_name[..open], false)?;
                entries.retain(|entry| entry.key != root);
                continue;
            }
            if raw_value.len() > limits.value_bytes {
                return Err(Error::ValueBytes);
            }
            // Conservative cumulative accounting also charges overwritten
            // elements. The actual live keys/values are no larger than this
            // quota, but the quota does not represent process RSS.
            charge(
                &mut retained,
                raw_name
                    .len()
                    .checked_add(raw_value.len())
                    .ok_or(Error::RetainedBytes)?,
                limits,
            )?;
            let root = root_key(&raw_name[..open], false)?;
            let value = decode_cookie_value(raw_value)?;
            let root_position =
                if let Some(position) = entries.iter().position(|entry| entry.key == root) {
                    position
                } else {
                    charge_node(&mut nodes, limits.nodes)?;
                    entries.try_reserve(1).map_err(|_| Error::Allocation)?;
                    entries.push(Entry {
                        key: root,
                        value: Value::Array(Array::default()),
                    });
                    entries.len() - 1
                };
            if matches!(entries[root_position].value, Value::Scalar(_)) {
                entries[root_position].value = Value::Array(Array::default());
            }
            let Value::Array(array) = &mut entries[root_position].value else {
                unreachable!();
            };
            let mut current = array;
            for index in &indexes[..depth - 1] {
                current = if is_append_index(index) {
                    match current.append_nested(&mut nodes, limits.nodes) {
                        Ok(child) => child,
                        // PHP 8.4 ignores an overflowing [] assignment,
                        // including an intermediate append, and continues
                        // registering subsequent Cookie variables.
                        Err(Error::AppendExhausted) => continue 'segments,
                        Err(error) => return Err(error),
                    }
                } else {
                    current.nested_mut(key(index)?, &mut nodes, limits.nodes)?
                };
            }
            let leaf = indexes[depth - 1];
            if is_append_index(leaf) {
                match current.append(value, &mut nodes, limits.nodes) {
                    Ok(()) => {}
                    Err(Error::AppendExhausted) => continue 'segments,
                    Err(error) => return Err(error),
                }
            } else {
                current.insert(key(leaf)?, value, &mut nodes, limits.nodes)?;
            }
        } else {
            let root = root_key(raw_name, true)?;
            // HTTP Cookie scalar duplicates are first-wins, including after
            // normalization. A later scalar cannot replace an array root.
            if entries.iter().any(|entry| entry.key == root) {
                continue;
            }
            if raw_value.len() > limits.value_bytes {
                return Err(Error::ValueBytes);
            }
            charge(
                &mut retained,
                raw_name
                    .len()
                    .checked_add(raw_value.len())
                    .ok_or(Error::RetainedBytes)?,
                limits,
            )?;
            let value = decode_cookie_value(raw_value)?;
            charge_node(&mut nodes, limits.nodes)?;
            entries.try_reserve(1).map_err(|_| Error::Allocation)?;
            entries.push(Entry {
                key: root,
                value: Value::Scalar(value),
            });
        }
    }
    Ok(CookieTree { entries })
}

fn write_bytes(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    writer.write_all(b"[\"s\",\"")?;
    for &byte in bytes {
        writer.write_all(&[DIGITS[(byte >> 4) as usize], DIGITS[(byte & 15) as usize]])?;
    }
    writer.write_all(b"\"]")
}

fn write_key(writer: &mut impl Write, key: &Key) -> io::Result<()> {
    match key {
        Key::Bytes(bytes) => write_bytes(writer, bytes),
        Key::Integer(number) => write!(writer, "[\"i\",\"{number}\"]"),
    }
}

fn write_value(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    match value {
        Value::Scalar(bytes) => write_bytes(writer, bytes),
        Value::Array(array) => {
            writer.write_all(b"[\"a\",[")?;
            for (index, child) in array.elements.iter().enumerate() {
                if index != 0 {
                    writer.write_all(b",")?;
                }
                writer.write_all(b"[")?;
                write_key(writer, &child.key)?;
                writer.write_all(b",")?;
                write_value(writer, &child.value)?;
                writer.write_all(b"]")?;
            }
            writer.write_all(b"]]")
        }
    }
}

impl CookieTree {
    /// Ordered, typed, hex-binary-safe JSON matching the PHP SAPI oracle.
    /// Key types are preserved, including 64-bit integer array indices.
    pub fn write_json(&self, writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(b"[\"a\",[")?;
        for (position, entry) in self.entries.iter().enumerate() {
            if position != 0 {
                writer.write_all(b",")?;
            }
            writer.write_all(b"[")?;
            write_key(writer, &entry.key)?;
            writer.write_all(b",")?;
            write_value(writer, &entry.value)?;
            writer.write_all(b"]")?;
        }
        writer.write_all(b"]]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(header: &[u8]) -> CookieTree {
        parse_one_level_header(header, Limits::default()).unwrap()
    }

    #[test]
    fn cookie_array_field_value_rejects_controls_but_preserves_htab_and_obs_text() {
        for control in [0x00, 0x01, 0x1f, 0x7f, b'\r', b'\n'] {
            let header = [b'a', b'[', b'x', b']', b'=', control];
            for quota in [None, Some(0)] {
                assert_eq!(
                    parse_one_level_header_with_php_ini_limits(
                        &header,
                        Limits::default(),
                        quota,
                        64,
                    )
                    .unwrap_err(),
                    Error::InvalidHeader
                );
            }
        }
        let parsed = parse(b"a[\t]=tab; a[]=next; raw=\xff\x80");
        assert_eq!(parsed.entries.len(), 2);
        let Value::Array(array) = &parsed.entries[0].value else {
            panic!("array root expected");
        };
        assert_eq!(array.elements.len(), 2);
        assert_eq!(array.elements[0].key, Key::Integer(0));
        assert_eq!(array.elements[1].key, Key::Integer(1));
        assert_eq!(parsed.entries[1].value, Value::Scalar(b"\xff\x80".to_vec()));
    }

    #[test]
    fn php_cookie_ignores_incomplete_or_noncontiguous_trailing_array_brackets() {
        let keys: [&[u8]; 4] = [b"x", b"y", b"z", b"w"];
        for (header, depth, expected) in [
            (b"a[x][=one".as_slice(), 1, b"one".as_slice()),
            (b"a[x][y][=two", 2, b"two"),
            (b"a[x][y][z][=three", 3, b"three"),
            (b"a[x][y][z][w][v=four", 4, b"four"),
            (b"a[x]noise[y]=one", 1, b"one"),
            (b"a[x][y]noise[z]=two", 2, b"two"),
            (b"a[x][y][z]noise[w]=three", 3, b"three"),
        ] {
            let tree = parse(header);
            assert_eq!(tree.entries.len(), 1);
            assert_eq!(tree.entries[0].key, Key::Bytes(b"a".to_vec()));
            let mut value = &tree.entries[0].value;
            for index in &keys[..depth] {
                let Value::Array(array) = value else {
                    panic!("expected PHP array at depth");
                };
                assert_eq!(array.elements.len(), 1);
                assert_eq!(array.elements[0].key, Key::Bytes(index.to_vec()));
                value = &array.elements[0].value;
            }
            assert_eq!(value, &Value::Scalar(expected.to_vec()));
        }

        // An unmatched inner '[' before the closing bracket remains part
        // of the array key, rather than opening another level.
        let bracket = parse(b"a[x][y[z]=bracket");
        let Value::Array(root) = &bracket.entries[0].value else {
            panic!("expected array root");
        };
        let Value::Array(child) = &root.elements[0].value else {
            panic!("expected nested array");
        };
        assert_eq!(child.elements[0].key, Key::Bytes(b"y[z".to_vec()));
        assert_eq!(child.elements[0].value, Value::Scalar(b"bracket".to_vec()));

        // A later valid assignment replaces the earlier leaf. An ignored
        // suffix does not consume an additional array depth.
        let tree = parse(b"a[x]tail[y]=first; a[x]=second; b=ok");
        assert_eq!(tree.entries.len(), 2);
        let Value::Array(root) = &tree.entries[0].value else {
            panic!("expected array root");
        };
        assert_eq!(root.elements.len(), 1);
        assert_eq!(root.elements[0].value, Value::Scalar(b"second".to_vec()));
        assert_eq!(tree.entries[1].value, Value::Scalar(b"ok".to_vec()));

        let limited = parse_one_level_header_with_php_ini_limits(
            b"a[x][=one; b=excluded",
            Limits::default(),
            Some(1),
            2,
        )
        .unwrap();
        assert_eq!(limited.entries.len(), 1);
        let Value::Array(root) = &limited.entries[0].value else {
            panic!("expected array root");
        };
        assert_eq!(root.elements[0].value, Value::Scalar(b"one".to_vec()));
        let over_depth = parse_one_level_header_with_php_ini_limits(
            b"a=old; a[x][=ignored; b=ok",
            Limits::default(),
            None,
            1,
        )
        .unwrap();
        assert_eq!(over_depth.entries.len(), 1);
        assert_eq!(over_depth.entries[0].key, Key::Bytes(b"b".to_vec()));
    }

    #[test]
    fn array_indices_overwrite_while_scalar_roots_are_first_wins() {
        let tree = parse(b"a=first; a=second; a[x]=one; a[x]=two; a[y]=three; a=last");
        assert_eq!(tree.entries.len(), 1);
        let Value::Array(array) = &tree.entries[0].value else {
            panic!("array root expected");
        };
        assert_eq!(array.elements.len(), 2);
        assert_eq!(array.elements[0].key, Key::Bytes(b"x".to_vec()));
        assert_eq!(array.elements[0].value, Value::Scalar(b"two".to_vec()));
        assert_eq!(array.elements[1].value, Value::Scalar(b"three".to_vec()));
        let reverse = parse(b"a[x]=one; a=ignored; a[x]=two");
        let Value::Array(array) = &reverse.entries[0].value else {
            panic!("array root expected");
        };
        assert_eq!(array.elements[0].value, Value::Scalar(b"two".to_vec()));
    }

    #[test]
    fn append_after_numeric_and_negative_indices_matches_php_84() {
        let tree = parse(b"a[-2]=one; a[]=two; a[]=three; a[01]=four; a[0]=last");
        let Value::Array(array) = &tree.entries[0].value else {
            panic!("array root expected");
        };
        assert_eq!(array.elements[0].key, Key::Integer(-2));
        assert_eq!(array.elements[1].key, Key::Integer(-1));
        assert_eq!(array.elements[2].key, Key::Integer(0));
        assert_eq!(array.elements[2].value, Value::Scalar(b"last".to_vec()));
        assert_eq!(array.elements[3].key, Key::Bytes(b"01".to_vec()));
        assert_eq!(array.next_index, Some(1));
    }

    #[test]
    fn binary_json_preserves_numeric_root_and_child_types() {
        let tree = parse(b"0[1]=%00%FF+; 01[x]=value");
        let mut json = Vec::new();
        tree.write_json(&mut json).unwrap();
        assert_eq!(
            String::from_utf8(json).unwrap(),
            r#"["a",[[["i","0"],["a",[[["i","1"],["s","00ff2b"]]]]],[["s","3031"],["a",[[["s","78"],["s","76616c7565"]]]]]]]"#
        );
    }

    #[test]
    fn php_cookie_array_limits_and_invalid_headers_fail_closed() {
        let limited = Limits {
            retained_bytes: 9,
            ..Limits::default()
        };
        assert_eq!(
            parse_one_level_header(b"a[x]=1; a[x]=2", limited).unwrap_err(),
            Error::RetainedBytes
        );
        let limited = Limits {
            pairs: 1,
            ..Limits::default()
        };
        assert_eq!(
            parse_one_level_header(b"a[]=1; a[]=2", limited).unwrap_err(),
            Error::Pairs
        );
        assert_eq!(
            parse_one_level_header(b"a=1\r\n", Limits::default()).unwrap_err(),
            Error::InvalidHeader
        );
    }

    #[test]
    fn php_cookie_two_level_arrays_preserve_collisions_and_append_order() {
        let tree = parse(b"a[x]=old; a[x][y]=one; a[x][y]=two; a[x][z]=three; a[][v]=four");
        let Value::Array(root) = &tree.entries[0].value else {
            panic!("expected array root");
        };
        let Value::Array(nested) = &root.elements[0].value else {
            panic!("expected promoted nested array");
        };
        assert_eq!(nested.elements.len(), 2);
        assert_eq!(nested.elements[0].value, Value::Scalar(b"two".to_vec()));
        assert_eq!(nested.elements[1].value, Value::Scalar(b"three".to_vec()));
        assert_eq!(root.elements[1].key, Key::Integer(0));
        let Value::Array(appended) = &root.elements[1].value else {
            panic!("expected appended nested array");
        };
        assert_eq!(appended.elements[0].key, Key::Bytes(b"v".to_vec()));
        assert_eq!(appended.elements[0].value, Value::Scalar(b"four".to_vec()));

        let mut json = Vec::new();
        tree.write_json(&mut json).unwrap();
        assert!(
            String::from_utf8(json)
                .unwrap()
                .contains(r#"[["s","78"],["a",[[["s","79"],["s","74776f"]]"#)
        );
    }

    #[test]
    fn php_cookie_two_level_limits_fail_closed() {
        let limited = Limits {
            retained_bytes: 12,
            ..Limits::default()
        };
        assert_eq!(
            parse_one_level_header(b"a[x][y]=one; a[x][y]=two", limited).unwrap_err(),
            Error::RetainedBytes
        );
        let tree = parse(b"a[\x0b]=first; a[]=second");
        let Value::Array(array) = &tree.entries[0].value else {
            panic!("expected array");
        };
        assert_eq!(array.elements[0].key, Key::Integer(0));
        assert_eq!(array.elements[1].key, Key::Integer(1));
    }

    #[test]
    fn php_cookie_array_nodes_budget_is_cumulative_and_all_or_error() {
        // PHP 8.4 HTTP SAPI produces a[x][z]=second and b[0]=ok.
        // a[x][y] was removed when a[x] became a scalar, but its
        // structural slot still counts against the request's hard budget.
        let header = b"a[x][y]=first; a[x]=scalar; a[x][z]=second; b[]=ok";
        let enough = Limits {
            nodes: 6,
            ..Limits::default()
        };
        let tree = parse_one_level_header(header, enough).unwrap();
        assert_eq!(tree.entries.len(), 2);
        let Value::Array(a) = &tree.entries[0].value else {
            panic!("expected array a");
        };
        let Value::Array(x) = &a.elements[0].value else {
            panic!("expected promoted a[x] array");
        };
        assert_eq!(x.elements.len(), 1);
        assert_eq!(x.elements[0].key, Key::Bytes(b"z".to_vec()));
        assert_eq!(x.elements[0].value, Value::Scalar(b"second".to_vec()));
        let Value::Array(b) = &tree.entries[1].value else {
            panic!("expected array b");
        };
        assert_eq!(b.elements[0].key, Key::Integer(0));
        assert_eq!(b.elements[0].value, Value::Scalar(b"ok".to_vec()));

        assert_eq!(
            parse_one_level_header(
                header,
                Limits {
                    nodes: 5,
                    ..Limits::default()
                },
            )
            .unwrap_err(),
            Error::Nodes
        );
        // Repeated keys do not allocate another structural slot. PHP's
        // variable quota can skip a later pair without charging nodes.
        let repeated = parse_one_level_header_with_php_max_input_vars(
            b"a[x]=first; a[x]=second; b=skipped",
            Limits {
                nodes: 2,
                ..Limits::default()
            },
            2,
        )
        .unwrap();
        assert_eq!(repeated.entries.len(), 1);
        assert!(
            parse_one_level_header(
                b"",
                Limits {
                    nodes: 0,
                    ..Limits::default()
                },
            )
            .unwrap()
            .entries
            .is_empty()
        );
    }

    #[test]
    fn php_cookie_variable_quota_counts_repeated_array_indices() {
        let tree = parse_one_level_header_with_php_max_input_vars(
            b"a[x]=first; a[x]=second; a[y]=third; nested[k][z]=ignored",
            Limits::default(),
            2,
        )
        .unwrap();
        let Value::Array(array) = &tree.entries[0].value else {
            panic!("expected array");
        };
        assert_eq!(array.elements.len(), 1);
        assert_eq!(array.elements[0].key, Key::Bytes(b"x".to_vec()));
        assert_eq!(array.elements[0].value, Value::Scalar(b"second".to_vec()));
        assert!(
            parse_one_level_header_with_php_max_input_vars(
                b"[ignored]=x; a[x]=one; b=two",
                Limits::default(),
                1,
            )
            .unwrap()
            .entries
            .is_empty()
        );
        assert!(
            parse_one_level_header_with_php_max_input_vars(
                b"a[x]=one; b=two",
                Limits::default(),
                0,
            )
            .unwrap()
            .entries
            .is_empty()
        );
    }

    #[test]
    fn blank_cookie_array_components_consume_hard_work_budget() {
        let limits = Limits {
            pairs: 2,
            ..Limits::default()
        };
        assert_eq!(
            parse_one_level_header(b";;; a[x]=one", limits).unwrap_err(),
            Error::Pairs
        );
        assert_eq!(
            parse_one_level_header_with_php_max_input_vars(b";;; a[x]=one", limits, 0).unwrap_err(),
            Error::Pairs
        );
        let accepted =
            parse_one_level_header_with_php_max_input_vars(b"; a[x]=one", limits, 1).unwrap();
        assert_eq!(accepted.entries.len(), 1);
        let Value::Array(array) = &accepted.entries[0].value else {
            panic!("expected registered array");
        };
        assert_eq!(array.elements[0].value, Value::Scalar(b"one".to_vec()));
        assert!(
            parse_one_level_header(
                b"",
                Limits {
                    pairs: 0,
                    ..Limits::default()
                }
            )
            .unwrap()
            .entries
            .is_empty()
        );
    }

    #[test]
    fn php_cookie_array_variable_quota_cannot_bypass_hard_pair_budget() {
        let limits = Limits {
            pairs: 2,
            name_bytes: 4,
            ..Limits::default()
        };
        let accepted = parse_one_level_header_with_php_max_input_vars(
            b"a[x]=1; very_long_skipped_name[y]=2",
            limits,
            1,
        )
        .unwrap();
        assert_eq!(accepted.entries.len(), 1);

        // Quota-skipped names are not decoded or projected, yet they
        // remain chargeable against the hard count of wire segments.
        assert_eq!(
            parse_one_level_header_with_php_max_input_vars(
                b"a[x]=1; very_long_skipped_name[y]=2; b=3",
                limits,
                1,
            )
            .unwrap_err(),
            Error::Pairs
        );
        assert_eq!(
            parse_one_level_header_with_php_max_input_vars(b"a[x]=1; =ignored; b=3", limits, 1,)
                .unwrap_err(),
            Error::Pairs
        );
        assert_eq!(
            parse_one_level_header_with_php_max_input_vars(b"a[x]=1; b=2; c=3", limits, 0)
                .unwrap_err(),
            Error::Pairs
        );
    }

    #[test]
    fn exhausted_php_cookie_append_is_ignored_without_losing_following_values() {
        let tree =
            parse(b"a[9223372036854775807]=max; a[]=skipped; a[][x]=skipped; a[0]=zero; b=kept");
        assert_eq!(tree.entries.len(), 2);
        let Value::Array(root) = &tree.entries[0].value else {
            panic!("array root expected");
        };
        assert_eq!(root.elements.len(), 2);
        assert_eq!(root.elements[0].key, Key::Integer(i64::MAX));
        assert_eq!(root.elements[0].value, Value::Scalar(b"max".to_vec()));
        assert_eq!(root.elements[1].key, Key::Integer(0));
        assert_eq!(root.elements[1].value, Value::Scalar(b"zero".to_vec()));
        assert_eq!(tree.entries[1].value, Value::Scalar(b"kept".to_vec()));

        let nested =
            parse(b"a[x][9223372036854775807]=max; a[x][]=skipped; a[x][][y]=skipped; b=ok");
        assert_eq!(nested.entries.len(), 2);
        let Value::Array(root) = &nested.entries[0].value else {
            panic!("array root expected");
        };
        let Value::Array(child) = &root.elements[0].value else {
            panic!("nested array expected");
        };
        assert_eq!(child.elements.len(), 1);
        assert_eq!(child.elements[0].key, Key::Integer(i64::MAX));
        assert_eq!(child.elements[0].value, Value::Scalar(b"max".to_vec()));
        assert_eq!(nested.entries[1].value, Value::Scalar(b"ok".to_vec()));
    }

    #[test]
    fn php_cookie_three_level_paths_promote_overwrite_and_append() {
        let tree =
            parse(b"a[x][y]=old; a[x][y][z]=one; a[x][y][z]=two; a[x][y][]=tail; a[][v][w]=next");
        let Value::Array(root) = &tree.entries[0].value else {
            panic!("expected root array");
        };
        let Value::Array(x) = &root.elements[0].value else {
            panic!("expected first-level array");
        };
        let Value::Array(y) = &x.elements[0].value else {
            panic!("expected second-level array");
        };
        assert_eq!(y.elements[0].key, Key::Bytes(b"z".to_vec()));
        assert_eq!(y.elements[0].value, Value::Scalar(b"two".to_vec()));
        assert_eq!(y.elements[1].key, Key::Integer(0));
        assert_eq!(y.elements[1].value, Value::Scalar(b"tail".to_vec()));
        assert_eq!(root.elements[1].key, Key::Integer(0));
        let Value::Array(appended) = &root.elements[1].value else {
            panic!("expected appended array");
        };
        let Value::Array(v) = &appended.elements[0].value else {
            panic!("expected nested array");
        };
        assert_eq!(v.elements[0].value, Value::Scalar(b"next".to_vec()));
    }

    #[test]
    fn php_cookie_three_level_depth_and_quota_limits() {
        let limited = Limits {
            retained_bytes: 12,
            ..Limits::default()
        };
        assert_eq!(
            parse_one_level_header(b"a[x][y][z]=one", limited).unwrap_err(),
            Error::RetainedBytes
        );
        let quota = parse_one_level_header_with_php_max_input_vars(
            b"a[x][y][z]=first; a[x][y][z]=second",
            Limits::default(),
            1,
        )
        .unwrap();
        let Value::Array(root) = &quota.entries[0].value else {
            panic!("expected root array");
        };
        let Value::Array(x) = &root.elements[0].value else {
            panic!("expected first-level array");
        };
        let Value::Array(y) = &x.elements[0].value else {
            panic!("expected second-level array");
        };
        assert_eq!(y.elements[0].value, Value::Scalar(b"first".to_vec()));
    }

    #[test]
    fn php_cookie_four_level_paths_promote_overwrite_and_append() {
        let tree = parse(
            b"a[x][y][z]=old; a[x][y][z][w]=one; a[x][y][z][w]=two; a[x][y][z][]=tail; a[][v][w][k]=next",
        );
        let Value::Array(root) = &tree.entries[0].value else {
            panic!("expected root array");
        };
        let Value::Array(x) = &root.elements[0].value else {
            panic!("expected first-level array");
        };
        let Value::Array(y) = &x.elements[0].value else {
            panic!("expected second-level array");
        };
        let Value::Array(z) = &y.elements[0].value else {
            panic!("expected third-level array");
        };
        assert_eq!(z.elements[0].key, Key::Bytes(b"w".to_vec()));
        assert_eq!(z.elements[0].value, Value::Scalar(b"two".to_vec()));
        assert_eq!(z.elements[1].key, Key::Integer(0));
        assert_eq!(z.elements[1].value, Value::Scalar(b"tail".to_vec()));
        let Value::Array(appended) = &root.elements[1].value else {
            panic!("expected appended root");
        };
        let Value::Array(v) = &appended.elements[0].value else {
            panic!("expected nested v");
        };
        let Value::Array(w) = &v.elements[0].value else {
            panic!("expected nested w");
        };
        assert_eq!(w.elements[0].value, Value::Scalar(b"next".to_vec()));
    }

    #[test]
    fn php_cookie_four_level_depth_and_input_limits() {
        let limited = Limits {
            retained_bytes: 16,
            ..Limits::default()
        };
        assert_eq!(
            parse_one_level_header(b"a[x][y][z][w]=long", limited).unwrap_err(),
            Error::RetainedBytes
        );
        let quota = parse_one_level_header_with_php_max_input_vars(
            b"a[x][y][z][w]=first; a[x][y][z][w]=second",
            Limits::default(),
            1,
        )
        .unwrap();
        let mut json = Vec::new();
        quota.write_json(&mut json).unwrap();
        let json = String::from_utf8(json).unwrap();
        assert!(json.contains(r#"["s","6669727374"]"#));
        assert!(!json.contains(r#"["s","7365636f6e64"]"#));
    }

    #[test]
    fn php_cookie_default_64_level_boundary_matches_nested_scalar() {
        let header = format!("a{}=ok", "[x]".repeat(DEFAULT_PHP_DEPTH));
        let tree = parse(header.as_bytes());
        assert_eq!(tree.entries.len(), 1);
        let mut value = &tree.entries[0].value;
        for _ in 0..DEFAULT_PHP_DEPTH {
            let Value::Array(array) = value else {
                panic!("expected nested array through 64 levels");
            };
            assert_eq!(array.elements.len(), 1);
            value = &array.elements[0].value;
        }
        assert_eq!(value, &Value::Scalar(b"ok".to_vec()));
    }

    #[test]
    fn php_cookie_overdepth_discards_prior_root_and_later_registration_recovers() {
        let header = format!(
            "a=old; b=keep; a{}=oversize; a[y]=reborn",
            "[x]".repeat(DEFAULT_PHP_DEPTH + 1)
        );
        let tree = parse(header.as_bytes());
        assert_eq!(tree.entries.len(), 2);
        assert_eq!(tree.entries[0].key, Key::Bytes(b"b".to_vec()));
        assert_eq!(tree.entries[1].key, Key::Bytes(b"a".to_vec()));
        let Value::Array(reborn) = &tree.entries[1].value else {
            panic!("expected a freshly registered array after overflow");
        };
        assert_eq!(reborn.elements[0].key, Key::Bytes(b"y".to_vec()));
        assert_eq!(reborn.elements[0].value, Value::Scalar(b"reborn".to_vec()));

        // Even a discarded over-depth pair consumes a PHP input-variable slot.
        let header = format!(
            "a=old; a{}=ignored; b=excluded",
            "[x]".repeat(DEFAULT_PHP_DEPTH + 1)
        );
        let limited =
            parse_one_level_header_with_php_max_input_vars(header.as_bytes(), Limits::default(), 1)
                .unwrap();
        assert_eq!(limited.entries[0].value, Value::Scalar(b"old".to_vec()));
        let limited =
            parse_one_level_header_with_php_max_input_vars(header.as_bytes(), Limits::default(), 2)
                .unwrap();
        assert!(limited.entries.is_empty());

        // The name-byte hard cap is checked before the PHP compatibility policy.
        let restricted = Limits {
            name_bytes: 32,
            ..Limits::default()
        };
        assert_eq!(
            parse_one_level_header(header.as_bytes(), restricted).unwrap_err(),
            Error::NameBytes
        );
    }

    #[test]
    fn php_cookie_configured_zero_nesting_discards_roots_and_preserves_scalars() {
        let tree = parse_one_level_header_with_php_ini_limits(
            b"a=old; a[x]=ignored; b=ok; c[x]=discarded",
            Limits::default(),
            None,
            0,
        )
        .unwrap();
        assert_eq!(tree.entries.len(), 1);
        assert_eq!(tree.entries[0].key, Key::Bytes(b"b".to_vec()));
        assert_eq!(tree.entries[0].value, Value::Scalar(b"ok".to_vec()));
    }

    #[test]
    fn php_cookie_configured_nesting_boundary_and_quota_are_independent() {
        let header = b"a[x]=one; a[x][y]=two; b=tail";
        let level_one =
            parse_one_level_header_with_php_ini_limits(header, Limits::default(), None, 1).unwrap();
        assert_eq!(level_one.entries.len(), 1);
        assert_eq!(level_one.entries[0].key, Key::Bytes(b"b".to_vec()));

        let level_two =
            parse_one_level_header_with_php_ini_limits(header, Limits::default(), None, 2).unwrap();
        assert_eq!(level_two.entries.len(), 2);
        let Value::Array(a) = &level_two.entries[0].value else {
            panic!("expected nested root at the configured boundary");
        };
        let Value::Array(x) = &a.elements[0].value else {
            panic!("expected nested child");
        };
        assert_eq!(x.elements[0].value, Value::Scalar(b"two".to_vec()));

        let quota =
            parse_one_level_header_with_php_ini_limits(header, Limits::default(), Some(2), 1)
                .unwrap();
        assert!(quota.entries.is_empty());

        let depth_64 = format!("a{}=accepted", "[x]".repeat(64));
        let accepted = parse_one_level_header_with_php_ini_limits(
            depth_64.as_bytes(),
            Limits::default(),
            None,
            64,
        )
        .unwrap();
        assert_eq!(accepted.entries.len(), 1);
        assert_eq!(
            parse_one_level_header_with_php_ini_limits(b"a=ok", Limits::default(), None, 129,)
                .unwrap_err(),
            Error::NestingLevel
        );
    }

    #[test]
    fn php_cookie_configured_128_level_boundary_and_overflow_reset() {
        let accepted = format!("a{}=at_limit; b=keep", "[x]".repeat(128));
        let tree = parse_one_level_header_with_php_ini_limits(
            accepted.as_bytes(),
            Limits::default(),
            None,
            128,
        )
        .unwrap();
        assert_eq!(tree.entries.len(), 2);
        let mut value = &tree.entries[0].value;
        for _ in 0..128 {
            let Value::Array(array) = value else {
                panic!("expected a nested array through configured 128 levels");
            };
            assert_eq!(array.elements.len(), 1);
            value = &array.elements[0].value;
        }
        assert_eq!(value, &Value::Scalar(b"at_limit".to_vec()));

        let overflow = format!("a=old; a{}=discarded; b=after", "[x]".repeat(129));
        let reset = parse_one_level_header_with_php_ini_limits(
            overflow.as_bytes(),
            Limits::default(),
            None,
            128,
        )
        .unwrap();
        assert_eq!(reset.entries.len(), 1);
        assert_eq!(reset.entries[0].key, Key::Bytes(b"b".to_vec()));

        let at_lower_limit = format!("a=old; a{}=discarded; b=after", "[x]".repeat(128));
        let reset = parse_one_level_header_with_php_ini_limits(
            at_lower_limit.as_bytes(),
            Limits::default(),
            None,
            127,
        )
        .unwrap();
        assert_eq!(reset.entries.len(), 1);
        assert_eq!(reset.entries[0].key, Key::Bytes(b"b".to_vec()));

        // The ignored 129-level pair consumes the second input-variable slot.
        let quota = parse_one_level_header_with_php_ini_limits(
            overflow.as_bytes(),
            Limits::default(),
            Some(2),
            128,
        )
        .unwrap();
        assert!(quota.entries.is_empty());
        assert_eq!(
            parse_one_level_header_with_php_ini_limits(b"a=ok", Limits::default(), None, 129,)
                .unwrap_err(),
            Error::NestingLevel
        );
    }
}
