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
    let file_count = request.parts().filter(|p| p.filename.is_some()).count();
    let parts_count = file_count
        .checked_mul(ATTRIBUTES.len())
        .ok_or(Error::InvalidLimits)?;
    let mut needed = 0usize;
    for part in request.parts().filter(|p| p.filename.is_some()) {
        if !post::valid_upload_name(part.name) {
            return Err(Error::MalformedUploadName);
        }
        for &attr in &ATTRIBUTES {
            let value = value_for(&part, attr);
            let prefix_len = part
                .name
                .iter()
                .position(|c| *c == b'[')
                .unwrap_or(part.name.len());
            // root + "[" + attr + "]" + original bracket suffix + value
            let entry_len = part
                .name
                .len()
                .checked_add(attr.len() + 2)
                .and_then(|n| n.checked_add(value.len()))
                .ok_or(Error::InvalidLimits)?;
            if prefix_len == 0 {
                return Err(Error::MalformedUploadName);
            }
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

    for part in request.parts().filter(|p| p.filename.is_some()) {
        let root_len = part
            .name
            .iter()
            .position(|c| *c == b'[')
            .unwrap_or(part.name.len());
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
            let value = value_for(&part, attr);
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
    .into_post(limits.post)
    .map_err(Error::Post)
}

fn map_wire_error(error: crate::Error) -> Error {
    match error {
        crate::Error::Limit(WireLimit::RetainedBytes) => Error::MetadataBytes,
        _ => Error::InvalidLimits,
    }
}

fn value_for(part: &crate::PartView<'_>, attribute: &[u8]) -> Vec<u8> {
    let filename = part.filename.expect("file parts only");
    let valid = !filename.is_empty();
    match attribute {
        b"name" => filename
            .rsplit(|byte| *byte == b'/' || *byte == b'\\')
            .next()
            .unwrap_or(filename)
            .to_vec(),
        b"full_path" => filename.to_vec(),
        b"type" if valid => part.content_type.unwrap_or(b"").to_vec(),
        b"type" => Vec::new(),
        b"tmp_name" if valid => STAGED_SENTINEL.to_vec(),
        b"tmp_name" => Vec::new(),
        b"error" if valid => b"0".to_vec(),
        b"error" => b"4".to_vec(),
        b"size" if valid => part.file_bytes.unwrap_or(0).to_string().into_bytes(),
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
        assert_eq!(value_for(&view, b"name").as_slice(), b"leaf.bin");
        assert_eq!(value_for(&view, b"full_path").as_slice(), b"folder/leaf.bin");
        let windows = crate::PartView {
            filename: Some(b"C:\\fakepath\\leaf.bin"),
            ..view
        };
        assert_eq!(value_for(&windows, b"name").as_slice(), b"leaf.bin");
        assert_eq!(
            value_for(&windows, b"full_path").as_slice(),
            b"C:\\fakepath\\leaf.bin"
        );
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
}
