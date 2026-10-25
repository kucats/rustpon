//! Tiny, audited pointer boundary for an experimental C caller.
//!
//! Core rustpon forbids unsafe code. Only this separate crate dereferences C
//! pointers; no C callback, Rust-owned allocation or Zend pointer crosses the
//! ABI. No function here calls PHP or preserves input across calls.

use rustpon::post;
use rustpon::urlencoded::FormParser;
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

const ABI_VERSION: u32 = 0x0001_0000;
const MAX_INPUT: usize = 256 * 1024;
const MAX_OUTPUT: usize = 1024 * 1024;

const OK: i32 = 0;
const INVALID_ARGUMENT: i32 = 1;
const PARSE_REJECTED: i32 = 2;
const POST_REJECTED: i32 = 3;
const BUFFER_TOO_SMALL: i32 = 4;
const PANIC: i32 = 5;
const OUTPUT_LIMIT: i32 = 6;

struct LimitedOutput {
    bytes: Vec<u8>,
}

impl LimitedOutput {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }
}

impl Write for LimitedOutput {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.len() > MAX_OUTPUT.saturating_sub(self.bytes.len()) {
            return Err(io::Error::from(io::ErrorKind::FileTooLarge));
        }
        self.bytes
            .try_reserve(input.len())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn parse(bytes: &[u8]) -> Result<Vec<u8>, i32> {
    let mut parser = FormParser::new(rustpon::Limits::default()).map_err(|_| PARSE_REJECTED)?;
    parser.feed(bytes).map_err(|_| PARSE_REJECTED)?;
    let request = parser.finish().map_err(|_| PARSE_REJECTED)?;
    let post = request
        .into_post(post::Limits::default())
        .map_err(|_| POST_REJECTED)?;
    let mut writer = LimitedOutput::new();
    post.write_json(&mut writer).map_err(|_| OUTPUT_LIMIT)?;
    Ok(writer.bytes)
}

#[unsafe(no_mangle)]
pub extern "C" fn rustpon_ffi_abi_version() -> u32 {
    ABI_VERSION
}

/// # Safety
///
/// Pointers must be live, aligned (for required_len), accessible and
/// non-overlapping for their declared lengths. No alias may be mutated by
/// another thread during this call. In particular required_len must point to
/// writable storage disjoint from both byte arrays, and output and input
/// must not overlap. NULL input is accepted only with input_len == 0.
/// NULL output is accepted only with capacity == 0.
/// Bad pointers supplied by C violate this contract and cannot be validated
/// by a Rust function. Do not invoke PHP callbacks or Zend bailout here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustpon_v1_form_post_json(
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    output_capacity: usize,
    required_len: *mut usize,
) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| {
        if required_len.is_null() {
            return INVALID_ARGUMENT;
        }
        // SAFETY: Caller promises a writable aligned size_t and all
        // required pointer ranges are disjoint.
        unsafe { ptr::write(required_len, 0) };
        if (input.is_null() && input_len != 0)
            || (output.is_null() && output_capacity != 0)
            || input_len > MAX_INPUT
        {
            return INVALID_ARGUMENT;
        }
        let bytes = if input_len == 0 {
            &[][..]
        } else {
            // SAFETY: Caller guarantees a live readable buffer of this size.
            unsafe { std::slice::from_raw_parts(input, input_len) }
        };
        let encoded = match parse(bytes) {
            Ok(encoded) => encoded,
            Err(error) => return error,
        };
        // SAFETY: Caller guarantees the required_len pointer is writable.
        unsafe { ptr::write(required_len, encoded.len()) };
        if output_capacity < encoded.len() {
            return BUFFER_TOO_SMALL;
        }
        if output.is_null() {
            return INVALID_ARGUMENT;
        }
        // SAFETY: Caller provides output_capacity writable bytes disjoint
        // from the local Vec and input. We validated the written length.
        unsafe { ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len()) };
        OK
    })) {
        Ok(code) => code,
        Err(_) => PANIC,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(input: &[u8]) -> (i32, Vec<u8>) {
        let mut needed = usize::MAX;
        // SAFETY: All pointers in these tests are live, aligned and disjoint.
        let probe = unsafe {
            rustpon_v1_form_post_json(input.as_ptr(), input.len(), ptr::null_mut(), 0, &mut needed)
        };
        if probe != BUFFER_TOO_SMALL {
            return (probe, Vec::new());
        }
        let mut output = vec![0u8; needed];
        let mut copied = 0;
        // SAFETY: Output has needed writable bytes, is disjoint from input.
        let code = unsafe {
            rustpon_v1_form_post_json(
                input.as_ptr(),
                input.len(),
                output.as_mut_ptr(),
                output.len(),
                &mut copied,
            )
        };
        assert_eq!(copied, needed);
        (code, output)
    }

    #[test]
    fn query_copy_and_invalid_inputs() {
        assert_eq!(rustpon_ffi_abi_version(), ABI_VERSION);
        let (code, output) = call(b"a=1&a=2&x=%00%ff");
        assert_eq!(code, OK);
        let json = String::from_utf8(output).unwrap();
        assert!(json.contains(r#"["s","00ff"]"#));
        assert!(json.contains(r#"["s","32"]"#));
        let (code, _) = call(b"a%00b=x");
        assert_eq!(code, PARSE_REJECTED);
        let mut needed = 999;
        // SAFETY: required_len is valid, NULL is paired with length zero.
        assert_eq!(
            unsafe { rustpon_v1_form_post_json(ptr::null(), 0, ptr::null_mut(), 0, &mut needed) },
            BUFFER_TOO_SMALL
        );
        assert!(needed > 0);
        // SAFETY: no buffer dereference occurs on rejected pointer/size.
        assert_eq!(
            unsafe { rustpon_v1_form_post_json(ptr::null(), 1, ptr::null_mut(), 0, &mut needed) },
            INVALID_ARGUMENT
        );
    }

    #[test]
    fn short_output_is_untouched_and_required_is_exact() {
        let bytes = b"z=last&a[]=first";
        let (code, json) = call(bytes);
        assert_eq!(code, OK);
        let mut short = vec![0xa5u8; json.len() - 1];
        let mut needed = 0;
        // SAFETY: Buffers are disjoint, output is valid and intentionally short.
        let code = unsafe {
            rustpon_v1_form_post_json(
                bytes.as_ptr(),
                bytes.len(),
                short.as_mut_ptr(),
                short.len(),
                &mut needed,
            )
        };
        assert_eq!(code, BUFFER_TOO_SMALL);
        assert_eq!(needed, json.len());
        assert!(short.iter().all(|byte| *byte == 0xa5));
        for _ in 0..128 {
            assert_eq!(call(bytes).1, json);
        }
    }

    #[test]
    fn request_size_cap_rejects_before_dereference() {
        let mut needed = 123;
        let oversized = vec![0u8; MAX_INPUT + 1];
        // SAFETY: The input buffer is valid and disjoint from required_len.
        let status = unsafe {
            rustpon_v1_form_post_json(
                oversized.as_ptr(),
                oversized.len(),
                ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        assert_eq!(status, INVALID_ARGUMENT);
    }
}
