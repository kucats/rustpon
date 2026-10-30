//! Experimental bounded PHP 8.4 Cookie registration with up to two array levels.
//!
//! Scalar roots are first-wins, while an array path replaces an earlier
//! scalar root and repeated array indices are last-wins. Three or more bracket
//! levels fail closed; this is not a Zend/SAPI registration API.

use super::{Error, Limits, decode_cookie_value};
use std::io::{self, Write};

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

    fn insert(&mut self, key: Key, value: Vec<u8>) -> Result<(), Error> {
        self.advance_next_index(&key);
        if let Some(existing) = self.elements.iter_mut().find(|entry| entry.key == key) {
            existing.value = Value::Scalar(value);
        } else {
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

    fn append(&mut self, value: Vec<u8>) -> Result<(), Error> {
        let index = self.append_index()?;
        self.insert(Key::Integer(index), value)
    }

    fn nested_mut(&mut self, key: Key) -> Result<&mut Array, Error> {
        self.advance_next_index(&key);
        let position =
            if let Some(position) = self.elements.iter().position(|entry| entry.key == key) {
                position
            } else {
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

    fn append_nested(&mut self) -> Result<&mut Array, Error> {
        let index = self.append_index()?;
        self.nested_mut(Key::Integer(index))
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
/// This slice models scalar roots and up to two bracket levels.
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

fn charge(retained: &mut usize, bytes: usize, limits: Limits) -> Result<(), Error> {
    *retained = retained
        .checked_add(bytes)
        .filter(|value| *value <= limits.retained_bytes)
        .ok_or(Error::RetainedBytes)?;
    Ok(())
}

/// Convert an already-extracted single Cookie header into a PHP-style tree.
/// Accepted array forms: root[key], root[], root[key][child], root[][child],
/// and root[key][]. One ASCII whitespace byte also acts as an append key.
/// A trailing suffix after the final ']' is ignored; malformed or deeper
/// bracket paths are rejected without partial output.
/// Header, pair, field and retained-byte limits remain independent.
pub fn parse_one_level_header(header: &[u8], limits: Limits) -> Result<CookieTree, Error> {
    parse_one_level_header_inner(header, limits, None)
}

/// Apply PHP 8.4 HTTP SAPI's Cookie max_input_vars quota before registering
/// nonempty raw names, including duplicates and ignored bracket-root names.
pub fn parse_one_level_header_with_php_max_input_vars(
    header: &[u8],
    limits: Limits,
    max_input_vars: usize,
) -> Result<CookieTree, Error> {
    parse_one_level_header_inner(header, limits, Some(max_input_vars))
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
) -> Result<CookieTree, Error> {
    if header.len() > limits.header_bytes {
        return Err(Error::HeaderBytes);
    }
    if header
        .iter()
        .any(|&byte| matches!(byte, b'\0' | b'\r' | b'\n'))
    {
        return Err(Error::InvalidHeader);
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut pairs = 0usize;
    let mut retained = 0usize;

    for segment in header.split(|&byte| byte == b';') {
        let segment = segment.trim_ascii_start();
        if segment.is_empty() {
            continue;
        }
        let (raw_name, raw_value) = match segment.iter().position(|&byte| byte == b'=') {
            Some(position) => (&segment[..position], &segment[position + 1..]),
            None => (segment, b"".as_slice()),
        };
        if !raw_name.is_empty()
            && let Some(remaining) = php_remaining.as_mut()
        {
            if *remaining == 0 {
                break;
            }
            *remaining -= 1;
        }
        pairs = pairs.checked_add(1).ok_or(Error::Pairs)?;
        if pairs > limits.pairs {
            return Err(Error::Pairs);
        }
        if raw_name.is_empty() {
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
            let suffix = &raw_name[close + 1..];
            // PHP interprets an immediately following bracket as a second
            // array dimension. An arbitrary suffix is ignored only if it
            // cannot introduce another bracket path.
            let second_index = if let Some(rest) = suffix.strip_prefix(b"[") {
                let end = rest
                    .iter()
                    .position(|&byte| byte == b']')
                    .ok_or(Error::UnsupportedArrayName)?;
                if rest[end + 1..].contains(&b'[') {
                    return Err(Error::UnsupportedArrayName);
                }
                Some(&rest[..end])
            } else {
                if suffix.contains(&b'[') {
                    return Err(Error::UnsupportedArrayName);
                }
                None
            };
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
            let index_bytes = &raw_name[open + 1..close];
            let append = is_append_index(index_bytes);
            let child_key = if append {
                None
            } else {
                Some(key(index_bytes)?)
            };
            let value = decode_cookie_value(raw_value)?;
            let root_position =
                if let Some(position) = entries.iter().position(|entry| entry.key == root) {
                    position
                } else {
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
            if let Some(second) = second_index {
                let nested = match child_key {
                    Some(index) => array.nested_mut(index)?,
                    None => array.append_nested()?,
                };
                if is_append_index(second) {
                    nested.append(value)?;
                } else {
                    nested.insert(key(second)?, value)?;
                }
            } else {
                match child_key {
                    Some(index) => array.insert(index, value)?,
                    None => array.append(value)?,
                }
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
    fn nested_paths_fail_closed_and_limits_cover_overwrites() {
        assert_eq!(
            parse_one_level_header(b"a[x][y][z]=1", Limits::default()).unwrap_err(),
            Error::UnsupportedArrayName
        );
        assert_eq!(
            parse_one_level_header(b"a[x][=1", Limits::default()).unwrap_err(),
            Error::UnsupportedArrayName
        );
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
    fn php_cookie_two_level_limits_and_malformed_paths_fail_closed() {
        for invalid in [
            b"a[x][y][z]=one".as_slice(),
            b"a[x][y][=two",
            b"a[x]noise[y]=three",
        ] {
            assert_eq!(
                parse_one_level_header(invalid, Limits::default()).unwrap_err(),
                Error::UnsupportedArrayName
            );
        }
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
    fn exhausted_append_is_an_error_instead_of_overwriting() {
        assert_eq!(
            parse_one_level_header(b"a[9223372036854775807]=x; a[]=y", Limits::default())
                .unwrap_err(),
            Error::AppendExhausted
        );
    }
}
