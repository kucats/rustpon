//! Bounded, provisional PHP-style $_FILES metadata tree.
//!
//! Only metadata is projected. Upload descriptors remain owned by the original
//! wire Request; the temporary PHP path is deliberately represented as a
//! stable sentinel, never as a disclosed host filesystem path.
//! Numeric PHP metadata values are encoded as ASCII string leaves in this
//! testing projection. A future Zend adapter must preserve native zval types.

use crate::post;
use crate::{Limit as WireLimit, Part, Payload, Request, Stats, store};
use std::ops::Range;

// PHP upload error codes used by the standalone metadata projection.
const UPLOAD_ERR_OK: u8 = 0;
const UPLOAD_ERR_FORM_SIZE: u8 = 2;
const UPLOAD_ERR_NO_FILE: u8 = 4;

const ATTRIBUTES: [&[u8]; 6] = [
    b"name",
    b"full_path",
    b"type",
    b"tmp_name",
    b"error",
    b"size",
];
/// Never treated as a real PHP temporary upload path.
pub const STAGED_SENTINEL: &[u8] = b"__RUSTPON_STAGED__";

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Bytes used to assemble the synthetic metadata request.
    pub metadata_bytes: usize,
    /// Capacity of its arena and part table, including allocator slack.
    pub memory_capacity: usize,
    /// Applies the same bounded PHP array-key rules as the POST adapter.
    pub post: post::Limits,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            metadata_bytes: 256 * 1024,
            memory_capacity: 512 * 1024,
            post: post::Limits::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidLimits,
    Allocation,
    MetadataBytes,
    MemoryCapacity,
    MalformedUploadName,
    Post(post::Error),
}

impl From<post::Error> for Error {
    fn from(error: post::Error) -> Self {
        Self::Post(error)
    }
}

/// Produce a PHP-shaped metadata tree from a validated wire request.
///
/// The caller must retain the original Request while upload bytes are used.
/// This projection contains no upload file descriptors or server temp paths.
/// Metadata (including nested indices and duplicate names) is staged fully
/// before the returned tree is visible to the caller.
pub fn project(request: &Request, limits: Limits) -> Result<post::PostRequest, Error> {
    project_impl(request, limits, false)
}

/// Internal projection with distinct per-upload tmp_name tokens. The tokens
/// are used only to identify which physical upload survived PHP-style array
/// replacement; no token is returned to applications.
pub(crate) fn project_with_ids(
    request: &Request,
    limits: Limits,
) -> Result<post::PostRequest, Error> {
    project_impl(request, limits, true)
}

fn project_impl(
    request: &Request,
    limits: Limits,
    tag_uploads: bool,
) -> Result<post::PostRequest, Error> {
    // PHP stops registering uploads at the first malformed file name.
    // Count only metadata that can actually be published; otherwise a
    // rejected suffix could exhaust the synthetic projection budget.
    let mut parts_count = 0usize;
    let mut needed = 0usize;
    let mut form_max = 0i64;
    let mut uploads_stopped = false;
    for (index, part) in request.parts().enumerate() {
        if part.filename.is_none() {
            if part.name.eq_ignore_ascii_case(b"MAX_FILE_SIZE") {
                form_max = parse_form_max(part.value.unwrap_or_default());
            }
            continue;
        }
        if uploads_stopped {
            continue;
        }
        if !post::valid_upload_name(part.name) {
            uploads_stopped = true;
            continue;
        }
        let prefix_len = part
            .name
            .iter()
            .position(|c| *c == b'[')
            .unwrap_or(part.name.len());
        // Empty or bracket-only roots are ignored without stopping later files.
        if prefix_len == 0 {
            continue;
        }
        parts_count = parts_count
            .checked_add(ATTRIBUTES.len())
            .ok_or(Error::InvalidLimits)?;
        let upload_error = upload_error(&part, form_max);
        for &attr in &ATTRIBUTES {
            let value = projected_value(&part, attr, upload_error, index, tag_uploads);
            // root + "[" + attr + "]" + original bracket suffix + value
            let entry_len = part
                .name
                .len()
                .checked_add(attr.len() + 2)
                .and_then(|n| n.checked_add(value.len()))
                .ok_or(Error::InvalidLimits)?;
            needed = needed.checked_add(entry_len).ok_or(Error::InvalidLimits)?;
            if needed > limits.metadata_bytes {
                return Err(Error::MetadataBytes);
            }
        }
    }
    let requested = needed
        .checked_add(
            parts_count
                .checked_mul(size_of::<Part>())
                .ok_or(Error::InvalidLimits)?,
        )
        .and_then(|n| n.checked_add(size_of::<Request>()))
        .ok_or(Error::InvalidLimits)?;
    if requested > limits.memory_capacity {
        return Err(Error::MemoryCapacity);
    }
    let mut arena = Vec::<u8>::new();
    arena
        .try_reserve_exact(needed)
        .map_err(|_| Error::Allocation)?;
    let mut parts = Vec::<Part>::new();
    parts
        .try_reserve_exact(parts_count)
        .map_err(|_| Error::Allocation)?;
    let actual = arena
        .capacity()
        .checked_add(
            parts
                .capacity()
                .checked_mul(size_of::<Part>())
                .ok_or(Error::InvalidLimits)?,
        )
        .and_then(|n| n.checked_add(size_of::<Request>()))
        .ok_or(Error::InvalidLimits)?;
    if actual > limits.memory_capacity {
        return Err(Error::MemoryCapacity);
    }

    form_max = 0;
    uploads_stopped = false;
    for (index, part) in request.parts().enumerate() {
        if part.filename.is_none() {
            if part.name.eq_ignore_ascii_case(b"MAX_FILE_SIZE") {
                form_max = parse_form_max(part.value.unwrap_or_default());
            }
            continue;
        }
        if uploads_stopped {
            continue;
        }
        if !post::valid_upload_name(part.name) {
            uploads_stopped = true;
            continue;
        }
        let root_len = part
            .name
            .iter()
            .position(|c| *c == b'[')
            .unwrap_or(part.name.len());
        if root_len == 0 {
            continue;
        }
        let upload_error = upload_error(&part, form_max);
        let tail = &part.name[root_len..];
        for &attr in &ATTRIBUTES {
            let start = arena.len();
            store(&mut arena, &part.name[..root_len], limits.metadata_bytes)
                .map_err(map_wire_error)?;
            store(&mut arena, b"[", limits.metadata_bytes).map_err(map_wire_error)?;
            store(&mut arena, attr, limits.metadata_bytes).map_err(map_wire_error)?;
            store(&mut arena, b"]", limits.metadata_bytes).map_err(map_wire_error)?;
            store(&mut arena, tail, limits.metadata_bytes).map_err(map_wire_error)?;
            let name_end = arena.len();
            let value = projected_value(&part, attr, upload_error, index, tag_uploads);
            store(&mut arena, &value, limits.metadata_bytes).map_err(map_wire_error)?;
            let value_end = arena.len();
            parts.push(Part {
                name: Range {
                    start,
                    end: name_end,
                },
                filename: None,
                content_type: None,
                payload: Payload::Field(name_end..value_end),
            });
        }
    }
    Request {
        arena,
        parts,
        stats: Stats {
            parts: parts_count,
            fields: parts_count,
            memory_capacity: actual,
            ..Stats::default()
        },
    }
    .into_files_post(limits.post)
    .map_err(Error::Post)
}

fn projected_value(
    part: &crate::PartView<'_>,
    attribute: &[u8],
    error: u8,
    index: usize,
    tag_uploads: bool,
) -> Vec<u8> {
    if tag_uploads && attribute == b"tmp_name" && error == UPLOAD_ERR_OK {
        return format!("__RUSTPON_UPLOAD_ID_{index}__").into_bytes();
    }
    value_for(part, attribute, error)
}

fn map_wire_error(error: crate::Error) -> Error {
    match error {
        crate::Error::Limit(WireLimit::RetainedBytes) => Error::MetadataBytes,
        _ => Error::InvalidLimits,
    }
}

/// C strtoll-like conversion of the form field: first signed decimal
/// prefix wins, leading ASCII whitespace allowed, overflow saturates.
/// A zero value disables the form advisory limit, just as PHP does.
pub(crate) fn parse_form_max(bytes: &[u8]) -> i64 {
    let mut tail = bytes.trim_ascii_start();
    let negative = tail.starts_with(b"-");
    if negative || tail.starts_with(b"+") {
        tail = &tail[1..];
    }
    let cap = (i64::MAX as u64) + u64::from(negative);
    let mut value = 0u64;
    let mut found = false;
    for &byte in tail {
        if !byte.is_ascii_digit() {
            break;
        }
        found = true;
        value = value
            .saturating_mul(10)
            .saturating_add(u64::from(byte - b'0'))
            .min(cap);
    }
    if !found {
        return 0;
    }
    if negative {
        if value == cap {
            i64::MIN
        } else {
            -(value as i64)
        }
    } else {
        value as i64
    }
}

fn upload_error(part: &crate::PartView<'_>, form_max: i64) -> u8 {
    if part.filename.expect("file parts only").is_empty() {
        return UPLOAD_ERR_NO_FILE;
    }
    if form_max != 0 && (form_max < 0 || part.file_bytes.unwrap_or(0) > form_max as u64) {
        return UPLOAD_ERR_FORM_SIZE;
    }
    UPLOAD_ERR_OK
}

fn value_for(part: &crate::PartView<'_>, attribute: &[u8], error: u8) -> Vec<u8> {
    let filename = part.filename.expect("file parts only");
    match attribute {
        b"name" => filename
            .rsplit(|byte| *byte == b'/' || *byte == b'\\')
            .next()
            .unwrap_or(filename)
            .to_vec(),
        b"full_path" => filename.to_vec(),
        b"type" if error == UPLOAD_ERR_OK => part.content_type.unwrap_or(b"").to_vec(),
        b"type" => Vec::new(),
        b"tmp_name" if error == UPLOAD_ERR_OK => STAGED_SENTINEL.to_vec(),
        b"tmp_name" => Vec::new(),
        b"error" => error.to_string().into_bytes(),
        b"size" if error == UPLOAD_ERR_OK => part.file_bytes.unwrap_or(0).to_string().into_bytes(),
        b"size" => b"0".to_vec(),
        _ => unreachable!("metadata attributes are fixed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Limits as WireLimits, Parser};
    use std::fs;
    use std::os::unix::fs::DirBuilderExt;

    #[test]
    fn nested_upload_shape_and_empty_filename() {
        let path =
            std::env::temp_dir().join(format!("rustpon-files-projection-{}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let body = b"--b\r\nContent-Disposition: form-data; name=\"f[x][]\"; filename=\"one.bin\"\r\nContent-Type: application/octet-stream\r\n\r\nhello\r\n--b\r\nContent-Disposition: form-data; name=\"f[x][]\"; filename=\"two.bin\"\r\nContent-Type: application/octet-stream\r\n\r\nworld\r\n--b--\r\n";
        let mut parser = Parser::new(b"b", WireLimits::default(), &path).unwrap();
        parser.feed(body).unwrap();
        let request = parser.finish().unwrap();
        let projection = project(&request, Limits::default()).unwrap();
        let mut output = Vec::new();
        projection.write_json(&mut output).unwrap();
        let json = String::from_utf8(output).unwrap();
        assert!(json.contains("6f6e652e62696e")); // one.bin
        assert!(json.contains("74776f2e62696e")); // two.bin
        assert!(json.contains("5f5f52555354504f4e5f5354414745445f5f")); // sentinel
        assert!(json.contains("66756c6c5f70617468")); // full_path
        drop(projection);
        drop(request);
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn filename_basename_does_not_change_full_path() {
        let view = crate::PartView {
            name: b"upload",
            filename: Some(b"folder/leaf.bin"),
            content_type: Some(b"application/octet-stream"),
            value: None,
            file_bytes: Some(123),
        };
        assert_eq!(value_for(&view, b"name", 0).as_slice(), b"leaf.bin");
        assert_eq!(
            value_for(&view, b"full_path", 0).as_slice(),
            b"folder/leaf.bin"
        );
        let windows = crate::PartView {
            filename: Some(b"C:\\fakepath\\leaf.bin"),
            ..view
        };
        assert_eq!(value_for(&windows, b"name", 0).as_slice(), b"leaf.bin");
        assert_eq!(
            value_for(&windows, b"full_path", 0).as_slice(),
            b"C:\\fakepath\\leaf.bin"
        );
    }

    #[test]
    fn form_limit_parsing_matches_signed_decimal_prefixes() {
        assert_eq!(parse_form_max(b""), 0);
        assert_eq!(parse_form_max(b"invalid"), 0);
        assert_eq!(parse_form_max(b" +0002suffix"), 2);
        assert_eq!(parse_form_max(b"\t-1junk"), -1);
        assert_eq!(parse_form_max(b"0"), 0);
        assert_eq!(parse_form_max(b"9223372036854775808"), i64::MAX);
        assert_eq!(parse_form_max(b"-9223372036854775809"), i64::MIN);
    }

    #[test]
    fn max_file_size_is_ordered_and_does_not_take_upload_ownership() {
        let path = std::env::temp_dir().join(format!("rustpon-max-form-{}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let body = b"--b\r\nContent-Disposition: form-data; name=MAX_FILE_SIZE\r\n\r\n2\r\n--b\r\nContent-Disposition: form-data; name=f; filename=x.txt\r\nContent-Type: application/octet-stream\r\n\r\nabc\r\n--b\r\nContent-Disposition: form-data; name=MAX_FILE_SIZE\r\n\r\n3\r\n--b\r\nContent-Disposition: form-data; name=g; filename=y.txt\r\nContent-Type: application/octet-stream\r\n\r\nabc\r\n--b--\r\n";
        let mut parser = Parser::new(b"b", WireLimits::default(), &path).unwrap();
        parser.feed(body).unwrap();
        let mut request = parser.finish().unwrap();
        let projection = project(&request, Limits::default()).unwrap();
        let pairs: Vec<_> = projection
            .wire()
            .parts()
            .map(|part| (part.name.to_vec(), part.value.unwrap().to_vec()))
            .collect();
        for (name, value) in [
            (b"f[error]".as_slice(), b"2".as_slice()),
            (b"f[size]", b"0"),
            (b"f[type]", b""),
            (b"f[tmp_name]", b""),
            (b"f[name]", b"x.txt"),
            (b"g[error]", b"0"),
            (b"g[size]", b"3"),
            (b"g[type]", b"application/octet-stream"),
            (b"g[tmp_name]", STAGED_SENTINEL),
        ] {
            assert!(
                pairs.iter().any(|(key, data)| key == name && data == value),
                "missing metadata key {name:?}"
            );
        }
        // The wire request still owns both upload FDs. Policy enforcement at
        // a PHP/SAPI handoff must not mistake staged bytes for accepted files.
        let mut staged = Vec::new();
        request.upload_mut(1).unwrap().copy_to(&mut staged).unwrap();
        assert_eq!(staged, b"abc");
        drop(projection);
        drop(request);
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn typed_files_json_preserves_nested_name_keys_and_integer_leaves() {
        let path = std::env::temp_dir().join(format!("rustpon-files-typed-{}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let body = b"--b\r\nContent-Disposition: form-data; name=\"f[tmp_name][]\"; filename=\"leaf.bin\"\r\nContent-Type: application/octet-stream\r\n\r\nabc\r\n--b--\r\n";
        let mut parser = Parser::new(b"b", WireLimits::default(), &path).unwrap();
        parser.feed(body).unwrap();
        let request = parser.finish().unwrap();
        let projection = project(&request, Limits::default()).unwrap();
        let mut legacy = Vec::new();
        projection.write_json(&mut legacy).unwrap();
        let mut typed = Vec::new();
        projection.write_php_files_json(&mut typed).unwrap();
        let legacy = String::from_utf8(legacy).unwrap();
        let typed = String::from_utf8(typed).unwrap();
        assert!(legacy.contains("[\"s\",\"33\"]")); // size 3 as legacy hex
        assert!(legacy.contains("[\"s\",\"30\"]")); // error 0 as legacy hex
        assert!(typed.contains("[\"i\",\"3\"]")); // PHP size 3
        assert!(typed.contains("[\"i\",\"0\"]")); // PHP error 0
        assert!(!typed.contains("[\"s\",\"33\"]"));
        assert!(typed.contains("6c6561662e62696e")); // filename remains bytes
        assert!(typed.contains("746d705f6e616d65")); // nested key tmp_name
        drop(projection);
        drop(request);
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn metadata_and_projection_budgets_are_exact() {
        let path =
            std::env::temp_dir().join(format!("rustpon-files-budget-{}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let body = b"--b\r\nContent-Disposition: form-data; name=\"f[x]\"; filename=\"a.bin\"\r\nContent-Type: application/octet-stream\r\n\r\npayload\r\n--b--\r\n";
        let mut parser = Parser::new(b"b", WireLimits::default(), &path).unwrap();
        parser.feed(body).unwrap();
        let request = parser.finish().unwrap();
        let baseline = project(&request, Limits::default()).unwrap();
        let bytes: usize = baseline
            .wire()
            .parts()
            .map(|p| p.name.len() + p.value.unwrap().len())
            .sum();
        let memory = baseline.wire().stats().memory_capacity;
        assert!(bytes > 0);
        drop(baseline);
        let exact = Limits {
            metadata_bytes: bytes,
            memory_capacity: memory,
            ..Limits::default()
        };
        assert!(project(&request, exact).is_ok());
        assert!(matches!(
            project(
                &request,
                Limits {
                    metadata_bytes: bytes - 1,
                    ..exact
                }
            ),
            Err(Error::MetadataBytes)
        ));
        assert!(matches!(
            project(
                &request,
                Limits {
                    memory_capacity: memory - 1,
                    ..exact
                }
            ),
            Err(Error::MemoryCapacity)
        ));
        assert!(matches!(
            project(
                &request,
                Limits {
                    post: post::Limits {
                        nodes: 1,
                        ..post::Limits::default()
                    },
                    ..Limits::default()
                }
            ),
            Err(Error::Post(post::Error::Nodes))
        ));
        // Projection errors must not steal or close the wire upload descriptor.
        let mut original = request;
        let mut content = Vec::new();
        assert_eq!(
            original
                .upload_mut(0)
                .unwrap()
                .copy_to(&mut content)
                .unwrap(),
            7
        );
        assert_eq!(content, b"payload");
        drop(original);
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn malformed_upload_name_stops_later_files_without_losing_fields() {
        let path =
            std::env::temp_dir().join(format!("rustpon-invalid-upload-{}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let cases: &[(&[&[u8]], &[usize])] = &[
            (&[b"bad[x", b"later"], &[]),
            (&[b"good", b"bad[x", b"later"], &[0]),
            (&[b"bad]", b"later"], &[]),
            (&[b"bad[x][", b"later"], &[]),
            (&[b"[ignored]", b"later"], &[1]),
            (&[b"", b"later"], &[1]),
        ];
        for &(names, expected) in cases {
            let mut body = Vec::new();
            for &name in names {
                body.extend_from_slice(b"--b\r\nContent-Disposition: form-data; name=\"");
                body.extend_from_slice(name);
                body.extend_from_slice(b"\"; filename=\"upload.bin\"\r\n\r\nxyz\r\n");
            }
            body.extend_from_slice(
                b"--b\r\nContent-Disposition: form-data; name=\"plain\"\r\n\r\nstill\r\n--b--\r\n",
            );
            let parse = || {
                let mut parser = Parser::new(b"b", WireLimits::default(), &path).unwrap();
                parser.feed(&body).unwrap();
                parser.finish().unwrap()
            };
            let request = parse();
            let metadata = project(&request, Limits::default()).unwrap();
            let mut tree = Vec::new();
            metadata.write_php_files_json(&mut tree).unwrap();
            assert!(!tree.is_empty(), "names={names:?}");
            drop(metadata);

            let mut admitted =
                crate::admission::admit(request, crate::admission::Policy::default()).unwrap();
            assert_eq!(
                admitted.accepted_indices().collect::<Vec<_>>().as_slice(),
                expected,
                "names={names:?}"
            );
            for index in 0..names.len() {
                let mut bytes = Vec::new();
                let copied = admitted.copy_accepted(index, &mut bytes).unwrap();
                assert_eq!(copied.is_some(), expected.contains(&index), "index={index}");
                if copied.is_some() {
                    assert_eq!(bytes, b"xyz");
                }
            }
            drop(admitted);

            let post = parse().into_post(post::Limits::default()).unwrap();
            let mut post_json = Vec::new();
            post.write_json(&mut post_json).unwrap();
            assert!(
                String::from_utf8(post_json).unwrap().contains("7374696c6c"),
                "ordinary field after invalid upload must survive"
            );
            drop(post);
            assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        }
        fs::remove_dir(&path).unwrap();
    }
}
