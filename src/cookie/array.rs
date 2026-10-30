//! Experimental bounded PHP 8.4 Cookie registration with one array level.
//!
//! Scalar roots are first-wins, while an array path replaces an earlier
//! scalar root and repeated array indices are last-wins. Deeper bracket
//! paths are deliberately rejected; this is not a Zend/SAPI registration API.

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
    value: Vec<u8>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Array {
    elements: Vec<Element>,
    next_index: Option<i64>,
}

impl Array {
    fn insert(&mut self, key: Key, value: Vec<u8>) -> Result<(), Error> {
        // Match 64-bit PHP array-index advancement even when an existing
        // explicit index is replaced by a later Cookie header segment.
        if let Key::Integer(number) = &key
            && self.next_index.is_none_or(|next| *number >= next)
        {
            self.next_index = Some(number.saturating_add(1));
        }
        if let Some(existing) = self.elements.iter_mut().find(|entry| entry.key == key) {
            existing.value = value;
        } else {
            self.elements
                .try_reserve(1)
                .map_err(|_| Error::Allocation)?;
            self.elements.push(Element { key, value });
        }
        Ok(())
    }

    fn append(&mut self, value: Vec<u8>) -> Result<(), Error> {
        let index = self.next_index.unwrap_or(0);
        // PHP may silently ignore an append after i64::MAX. Reject rather
        // than overwrite an existing element or produce a false projection.
        if self
            .elements
            .iter()
            .any(|entry| entry.key == Key::Integer(index))
        {
            return Err(Error::AppendExhausted);
        }
        self.insert(Key::Integer(index), value)
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
/// This slice models scalar roots and one bracket level only.
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
        if matches!(byte, b' ' | b'.')
            || (flatten_open_brackets && byte == b'[')
        {
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
/// Accepted array forms: root[key], root[], and root[one-whitespace-byte].
/// A trailing suffix after the first completed ']' is ignored unless it
/// opens another bracket. Nested paths are rejected without partial output.
/// Header, pair, field and retained-byte limits remain independent.
pub fn parse_one_level_header(header: &[u8], limits: Limits) -> Result<CookieTree, Error> {
    if header.len() > limits.header_bytes {
        return Err(Error::HeaderBytes);
    }
    if header.iter().any(|&byte| matches!(byte, b'\0' | b'\r' | b'\n')) {
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
        pairs = pairs.checked_add(1).ok_or(Error::Pairs)?;
        if pairs > limits.pairs {
            return Err(Error::Pairs);
        }
        let (raw_name, raw_value) = match segment.iter().position(|&byte| byte == b'=') {
            Some(position) => (&segment[..position], &segment[position + 1..]),
            None => (segment, b"".as_slice()),
        };
        if raw_name.is_empty() {
            continue;
        }
        if raw_name.len() > limits.name_bytes {
            return Err(Error::NameBytes);
        }
        if raw_name.first() == Some(&b'[') {
            continue;
        }
        let bracket = raw_name.iter().position(|&byte| byte == b'[').and_then(|open| {
            raw_name[open + 1..]
                .iter()
                .position(|&byte| byte == b']')
                .map(|offset| (open, open + 1 + offset))
        });
        if let Some((open, close)) = bracket {
            if raw_name[close + 1..].contains(&b'[') {
                return Err(Error::UnsupportedArrayName);
            }
            if raw_value.len() > limits.value_bytes {
                return Err(Error::ValueBytes);
            }
            // Conservative cumulative accounting also charges overwritten
            // elements. The actual live keys/values are no larger than this
            // quota, but the quota does not represent process RSS.
            charge(
                &mut retained,
                raw_name.len().checked_add(raw_value.len()).ok_or(Error::RetainedBytes)?,
                limits,
            )?;
            let root = root_key(&raw_name[..open], false)?;
            let index_bytes = &raw_name[open + 1..close];
            let append = index_bytes.is_empty()
                || (index_bytes.len() == 1 && index_bytes[0].is_ascii_whitespace());
            let child_key = if append { None } else { Some(key(index_bytes)?) };
            let value = decode_cookie_value(raw_value)?;
            let root_position = if let Some(position) =
                entries.iter().position(|entry| entry.key == root)
            {
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
            match child_key {
                Some(index) => array.insert(index, value)?,
                None => array.append(value)?,
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
                raw_name.len().checked_add(raw_value.len()).ok_or(Error::RetainedBytes)?,
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
            match &entry.value {
                Value::Scalar(bytes) => write_bytes(writer, bytes)?,
                Value::Array(array) => {
                    writer.write_all(b"[\"a\",[")?;
                    for (index, child) in array.elements.iter().enumerate() {
                        if index != 0 {
                            writer.write_all(b",")?;
                        }
                        writer.write_all(b"[")?;
                        write_key(writer, &child.key)?;
                        writer.write_all(b",")?;
                        write_bytes(writer, &child.value)?;
                        writer.write_all(b"]")?;
                    }
                    writer.write_all(b"]]")?;
                }
            }
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
        assert_eq!(array.elements[0].value.as_slice(), b"two");
        assert_eq!(array.elements[1].value.as_slice(), b"three");
        let reverse = parse(b"a[x]=one; a=ignored; a[x]=two");
        let Value::Array(array) = &reverse.entries[0].value else {
            panic!("array root expected");
        };
        assert_eq!(array.elements[0].value.as_slice(), b"two");
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
        assert_eq!(array.elements[2].value.as_slice(), b"last");
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
            parse_one_level_header(b"a[x][y]=1", Limits::default()).unwrap_err(),
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
    fn exhausted_append_is_an_error_instead_of_overwriting() {
        assert_eq!(
            parse_one_level_header(
                b"a[9223372036854775807]=x; a[]=y",
                Limits::default()
            )
            .unwrap_err(),
            Error::AppendExhausted
        );
    }
}
