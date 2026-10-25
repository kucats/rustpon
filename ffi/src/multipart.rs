//! One-shot multipart FFI, with no persistent Rust-owned request handles.
//!
//! Both operations consume the fully-buffered request, enforce the Rust
//! admission gate, and drop all staging descriptors before returning to C.
//! The caller must use a trusted pre-existing private spool directory; this
//! is not a client-configurable path or a PHP upload registry.

use super::{
    BUFFER_TOO_SMALL, INVALID_ARGUMENT, LimitedOutput, MAX_INPUT, OK, OUTPUT_LIMIT,
    PANIC, PARSE_REJECTED, POST_REJECTED, publish,
};
use rustpon::admission::{self, Policy};
use std::path::Path;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

const MAX_BOUNDARY: usize = 70;
const MAX_SPOOL_PATH: usize = 4096;
const NOT_ADMITTED: i32 = 7;

enum Mode {
    Metadata,
    Upload(usize),
}

fn parse(
    boundary: &[u8],
    body: &[u8],
    spool: &Path,
    mode: Mode,
) -> Result<Vec<u8>, i32> {
    let limits = rustpon::Limits {
        body_bytes: MAX_INPUT as u64,
        ..rustpon::Limits::default()
    };
    let mut parser = rustpon::Parser::new(boundary, limits, spool)
        .map_err(|_| PARSE_REJECTED)?;
    parser.feed(body).map_err(|_| PARSE_REJECTED)?;
    let request = parser.finish().map_err(|_| PARSE_REJECTED)?;
    let policy = Policy {
        max_body_bytes: MAX_INPUT as u64,
        ..Policy::default()
    };
    let mut admitted = admission::admit(request, policy)
        .map_err(|_| POST_REJECTED)?;
    let mut writer = LimitedOutput::new();
    match mode {
        Mode::Metadata => admitted.metadata().write_php_files_json(&mut writer)
            .map_err(|_| OUTPUT_LIMIT)?,
        Mode::Upload(index) => {
            if admitted.accepted_file(index).is_none() {
                return Err(NOT_ADMITTED);
            }
            let copied = admitted.copy_accepted(index, &mut writer)
                .map_err(|_| OUTPUT_LIMIT)?;
            if copied.is_none() {
                return Err(NOT_ADMITTED);
            }
        }
    }
    Ok(writer.bytes)
}

/// # Safety
///
/// All pointers must be live, properly aligned and pairwise non-overlapping
/// over the entire declared range throughout the call. No concurrent mutation.
/// The spool path is supplied by a trusted C caller, not an HTTP client.
unsafe fn run(
    boundary: *const u8,
    boundary_len: usize,
    body: *const u8,
    body_len: usize,
    spool_dir: *const u8,
    spool_len: usize,
    output: *mut u8,
    output_capacity: usize,
    required_len: *mut usize,
    mode: Mode,
) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| {
        if required_len.is_null() {
            return INVALID_ARGUMENT;
        }
        // SAFETY: The C caller promises a disjoint, writable size_t.
        unsafe { ptr::write(required_len, 0) };
        if boundary.is_null() || boundary_len == 0 || boundary_len > MAX_BOUNDARY
            || (body.is_null() && body_len != 0)
            || spool_dir.is_null() || spool_len == 0 || spool_len > MAX_SPOOL_PATH
            || (output.is_null() && output_capacity != 0)
            || body_len > MAX_INPUT
        {
            return INVALID_ARGUMENT;
        }
        // SAFETY: C caller guarantees the input buffers are live for their
        // declared lengths and no bytes overlap with writable output.
        let boundary = unsafe { std::slice::from_raw_parts(boundary, boundary_len) };
        let body = if body_len == 0 {
            &[][..]
        } else {
            // SAFETY: C caller's readable buffer contract.
            unsafe { std::slice::from_raw_parts(body, body_len) }
        };
        // SAFETY: C caller guarantees spool_dir is readable for spool_len.
        let path_bytes = unsafe { std::slice::from_raw_parts(spool_dir, spool_len) };
        let path = match std::str::from_utf8(path_bytes) {
            Ok(path) if !path.contains('\0') => Path::new(path),
            _ => return INVALID_ARGUMENT,
        };
        let encoded = match parse(boundary, body, path, mode) {
            Ok(bytes) => bytes,
            Err(status) => return status,
        };
        // SAFETY: Length pointer is writable and disjoint; output is writable
        // for capacity bytes if not NULL. Errors preserve caller output.
        unsafe { publish(&encoded, output, output_capacity, required_len) }
    })) {
        Ok(status) => status,
        Err(_) => PANIC,
    }
}

/// Produces the *typed tagged JSON* projection of the admitted $_FILES tree.
/// This has no file payloads and no server temp paths. The source body must
/// be a complete, dechunked multipart entity with explicit boundary.
///
/// # Safety
///
/// Caller supplies valid non-overlapping buffers and a trusted spool path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustpon_v1_multipart_files_json(
    boundary: *const u8,
    boundary_len: usize,
    body: *const u8,
    body_len: usize,
    spool_dir: *const u8,
    spool_len: usize,
    output: *mut u8,
    output_capacity: usize,
    required_len: *mut usize,
) -> i32 {
    // SAFETY: Contract is identical to run; no pointers are retained.
    unsafe {
        run(
            boundary, boundary_len, body, body_len, spool_dir, spool_len,
            output, output_capacity, required_len, Mode::Metadata,
        )
    }
}

/// Copies the payload of a *currently admitted* upload by original multipart
/// wire part index, after MAX_FILE_SIZE and PHP FILES overwrite resolution.
/// Invalid, rejected, empty-name, and overwritten uploads return code 7.
///
/// A NULL output/capacity-zero query parses and stages the request, then
/// drops the descriptors before returning the required payload size. A
/// second call reparses it; do not mutate the request between those calls.
///
/// # Safety
///
/// Caller supplies valid non-overlapping buffers and a trusted spool path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustpon_v1_multipart_upload_copy(
    boundary: *const u8,
    boundary_len: usize,
    body: *const u8,
    body_len: usize,
    spool_dir: *const u8,
    spool_len: usize,
    wire_part_index: usize,
    output: *mut u8,
    output_capacity: usize,
    required_len: *mut usize,
) -> i32 {
    // SAFETY: Contract is identical to run; no pointers are retained.
    unsafe {
        run(
            boundary, boundary_len, body, body_len, spool_dir, spool_len,
            output, output_capacity, required_len, Mode::Upload(wire_part_index),
        )
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct PrivateSpool(std::path::PathBuf);

    impl PrivateSpool {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rustpon-ffi-multipart-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &[u8] {
            self.0.to_str().unwrap().as_bytes()
        }

        fn assert_empty(&self) {
            assert_eq!(fs::read_dir(&self.0).unwrap().count(), 0);
        }
    }

    impl Drop for PrivateSpool {
        fn drop(&mut self) {
            self.assert_empty();
            fs::remove_dir(&self.0).unwrap();
        }
    }

    fn body(parts: &[(&[u8], &[u8], &[u8])]) -> Vec<u8> {
        let mut input = Vec::new();
        for (name, filename, content) in parts {
            input.extend_from_slice(b"--b\r\nContent-Disposition: form-data; name=\"");
            input.extend_from_slice(name);
            input.extend_from_slice(b"\"; filename=\"");
            input.extend_from_slice(filename);
            input.extend_from_slice(b"\"\r\n\r\n");
            input.extend_from_slice(content);
            input.extend_from_slice(b"\r\n");
        }
        input.extend_from_slice(b"--b--\r\n");
        input
    }

    fn call(spool: &PrivateSpool, input: &[u8], index: Option<usize>) -> (i32, Vec<u8>) {
        let mut needed = usize::MAX;
        let path = spool.path();
        let invoke = |output: *mut u8, capacity: usize, required: &mut usize| {
            if let Some(index) = index {
                // SAFETY: All spans are live and non-overlapping for this call.
                unsafe {
                    rustpon_v1_multipart_upload_copy(
                        b"b".as_ptr(), 1, input.as_ptr(), input.len(),
                        path.as_ptr(), path.len(), index,
                        output, capacity, required,
                    )
                }
            } else {
                // SAFETY: All spans are live and non-overlapping for this call.
                unsafe {
                    rustpon_v1_multipart_files_json(
                        b"b".as_ptr(), 1, input.as_ptr(), input.len(),
                        path.as_ptr(), path.len(),
                        output, capacity, required,
                    )
                }
            }
        };
        let status = invoke(ptr::null_mut(), 0, &mut needed);
        spool.assert_empty();
        if status != BUFFER_TOO_SMALL {
            return (status, Vec::new());
        }
        let mut output = vec![0u8; needed];
        let mut written = 0;
        let status = invoke(output.as_mut_ptr(), output.len(), &mut written);
        assert_eq!(written, needed);
        spool.assert_empty();
        (status, output)
    }

    #[test]
    fn accepted_upload_copy_rejects_overwritten_and_no_file_indices() {
        let spool = PrivateSpool::new();
        let input = body(&[
            (b"f", b"a.bin", b"first"),
            (b"f", b"b.bin", b"second"),
            (b"f[]", b"third.bin", b"third"),
            (b"empty", b"", b""),
            (b"zero", b"zero.bin", b""),
        ]);
        // The final f[] assignment overwrites f's previous scalar file.
        let (status, metadata) = call(&spool, &input, None);
        assert_eq!(status, OK);
        let json = String::from_utf8(metadata).unwrap();
        assert!(json.contains(r#"["i","0"]"#)); // PHP upload error code
        assert!(json.contains(r#"["i","5"]"#)); // third.bin size
        assert!(json.contains(r#"["i","4"]"#)); // empty-name upload error
        assert_eq!(call(&spool, &input, Some(0)).0, NOT_ADMITTED);
        assert_eq!(call(&spool, &input, Some(1)).0, NOT_ADMITTED);
        assert_eq!(call(&spool, &input, Some(2)), (OK, b"third".to_vec()));
        assert_eq!(call(&spool, &input, Some(3)).0, NOT_ADMITTED);
        assert_eq!(call(&spool, &input, Some(4)), (OK, Vec::new()));
        assert_eq!(call(&spool, &input, Some(999)).0, NOT_ADMITTED);
        for _ in 0..24 {
            assert_eq!(call(&spool, &input, Some(2)), (OK, b"third".to_vec()));
        }
    }

    #[test]
    fn fail_closed_on_invalid_boundary_truncation_and_spool() {
        let spool = PrivateSpool::new();
        let input = body(&[(b"upload", b"x.txt", b"abc")]);
        assert_eq!(call(&spool, &input[..input.len() - 2], None).0, PARSE_REJECTED);
        assert_eq!(call(&spool, b"garbage", None).0, PARSE_REJECTED);
        let mut required = 23;
        // SAFETY: Readable body/boundary/spool and disjoint writable size.
        let invalid = unsafe {
            rustpon_v1_multipart_files_json(
                b"b".as_ptr(), 1, input.as_ptr(), input.len(),
                b"/nonexistent/spool".as_ptr(), b"/nonexistent/spool".len(),
                ptr::null_mut(), 0, &mut required,
            )
        };
        assert_eq!(invalid, PARSE_REJECTED);
        assert_eq!(required, 0);
        // SAFETY: Invalid null boundary is checked before any dereference.
        let null_boundary = unsafe {
            rustpon_v1_multipart_files_json(
                ptr::null(), 1, input.as_ptr(), input.len(),
                spool.path().as_ptr(), spool.path().len(),
                ptr::null_mut(), 0, &mut required,
            )
        };
        assert_eq!(null_boundary, INVALID_ARGUMENT);
        assert_eq!(required, 0);
        spool.assert_empty();
    }
}
