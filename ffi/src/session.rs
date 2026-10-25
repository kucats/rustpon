//! Process-local, bounded multipart admission sessions for a trusted C shim.
//!
//! Opaque 64-bit IDs, never raw Rust pointers. Every successful open owns an
//! AdmittedRequest, including its anonymous upload FDs, until explicit close.
//! The registry serializes operations and permits at most eight live sessions.

use super::{INVALID_ARGUMENT, LimitedOutput, MAX_INPUT, OUTPUT_LIMIT, PANIC, PARSE_REJECTED, POST_REJECTED, publish};
use rustpon::admission::{self, AdmittedRequest, Policy};
use std::path::Path;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Mutex, OnceLock};

const MAX_SESSIONS: usize = 8;
const MAX_BOUNDARY: usize = 70;
const MAX_SPOOL_PATH: usize = 4096;
const INVALID_HANDLE: i32 = 8;
const SESSIONS_FULL: i32 = 9;
const NOT_ADMITTED: i32 = 7;

struct Registry {
    next: u64,
    active: Vec<(u64, AdmittedRequest)>,
}

impl Registry {
    fn new() -> Self {
        Self { next: 1, active: Vec::new() }
    }
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry::new()))
}

fn parse_admitted(boundary: &[u8], body: &[u8], spool: &Path) -> Result<AdmittedRequest, i32> {
    let limits = rustpon::Limits {
        body_bytes: MAX_INPUT as u64,
        ..rustpon::Limits::default()
    };
    let mut parser = rustpon::Parser::new(boundary, limits, spool).map_err(|_| PARSE_REJECTED)?;
    parser.feed(body).map_err(|_| PARSE_REJECTED)?;
    let raw = parser.finish().map_err(|_| PARSE_REJECTED)?;
    admission::admit(
        raw,
        Policy { max_body_bytes: MAX_INPUT as u64, ..Policy::default() },
    ).map_err(|_| POST_REJECTED)
}

/// Open a bounded, admitted multipart session.
///
/// # Safety
///
/// All non-NULL pointers must refer to live, readable input buffers for their
/// declared lengths; out_handle is a live, aligned, disjoint writable u64.
/// Spool path must be provided by trusted server configuration, not HTTP data.
/// A returned integer is a process-local index, *not a secret capability*.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustpon_v1_multipart_session_open(
    boundary: *const u8,
    boundary_len: usize,
    body: *const u8,
    body_len: usize,
    spool_dir: *const u8,
    spool_len: usize,
    out_handle: *mut u64,
) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| {
        if out_handle.is_null() { return INVALID_ARGUMENT; }
        // SAFETY: caller guarantees out_handle is aligned and writable.
        unsafe { ptr::write(out_handle, 0) };
        if boundary.is_null() || boundary_len == 0 || boundary_len > MAX_BOUNDARY
            || (body.is_null() && body_len != 0) || body_len > MAX_INPUT
            || spool_dir.is_null() || spool_len == 0 || spool_len > MAX_SPOOL_PATH
        {
            return INVALID_ARGUMENT;
        }
        // SAFETY: all input spans are guaranteed live, disjoint from out_handle.
        let boundary = unsafe { std::slice::from_raw_parts(boundary, boundary_len) };
        let body = if body_len == 0 { &[][..] } else {
            // SAFETY: caller provides body_len valid bytes.
            unsafe { std::slice::from_raw_parts(body, body_len) }
        };
        // SAFETY: caller provides spool_len valid bytes.
        let spool = unsafe { std::slice::from_raw_parts(spool_dir, spool_len) };
        let spool = match std::str::from_utf8(spool) {
            Ok(path) if !path.contains('\0') => Path::new(path),
            _ => return INVALID_ARGUMENT,
        };
        let mut guard = match registry().lock() {
            Ok(guard) => guard,
            Err(_) => return PANIC,
        };
        if guard.active.len() >= MAX_SESSIONS {
            return SESSIONS_FULL;
        }
        if guard.next == u64::MAX {
            return SESSIONS_FULL;
        }
        // Bound simultaneous parse work by holding the single registry lock.
        // No user code callbacks can occur with the lock held.
        let admitted = match parse_admitted(boundary, body, spool) {
            Ok(admitted) => admitted,
            Err(status) => return status,
        };
        if guard.active.try_reserve(1).is_err() { return POST_REJECTED; }
        let handle = guard.next;
        guard.next += 1;
        guard.active.push((handle, admitted));
        // SAFETY: pointer is a disjoint, writable u64.
        unsafe { ptr::write(out_handle, handle) };
        0
    })) {
        Ok(status) => status,
        Err(_) => PANIC,
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Metadata,
    Upload(usize),
}

unsafe fn get(
    handle: u64,
    output: *mut u8,
    output_capacity: usize,
    required_len: *mut usize,
    mode: Mode,
) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| {
        if required_len.is_null() { return INVALID_ARGUMENT; }
        // SAFETY: caller guarantees disjoint, writable size_t.
        unsafe { ptr::write(required_len, 0) };
        if handle == 0 || (output.is_null() && output_capacity != 0) {
            return INVALID_ARGUMENT;
        }
        let mut guard = match registry().lock() {
            Ok(guard) => guard,
            Err(_) => return PANIC,
        };
        let Some((_, admitted)) = guard.active.iter_mut().find(|(id, _)| *id == handle) else {
            return INVALID_HANDLE;
        };
        let mut writer = LimitedOutput::new();
        match mode {
            Mode::Metadata => {
                if admitted.metadata().write_php_files_json(&mut writer).is_err() {
                    return OUTPUT_LIMIT;
                }
            }
            Mode::Upload(index) => {
                if admitted.accepted_file(index).is_none() { return NOT_ADMITTED; }
                match admitted.copy_accepted(index, &mut writer) {
                    Ok(Some(_)) => (),
                    Ok(None) => return NOT_ADMITTED,
                    Err(_) => return OUTPUT_LIMIT,
                }
            }
        }
        // SAFETY: C output pointer and result pointer are disjoint, live and
        // writable for the caller-provided capacity.
        unsafe { publish(&writer.bytes, output, output_capacity, required_len) }
    })) {
        Ok(status) => status,
        Err(_) => PANIC,
    }
}

/// Serialized admitted PHP FILES metadata, retaining original request ownership.
///
/// # Safety
///
/// Caller supplies valid, disjoint writable buffers and a live size_t.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustpon_v1_multipart_session_files_json(
    handle: u64,
    output: *mut u8,
    output_capacity: usize,
    required_len: *mut usize,
) -> i32 {
    // SAFETY: forwarded unchanged under the external pointer contract.
    unsafe { get(handle, output, output_capacity, required_len, Mode::Metadata) }
}

/// Copy a previously admitted file by its original wire part index.
///
/// # Safety
///
/// Caller supplies valid, disjoint writable buffers and a live size_t.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustpon_v1_multipart_session_upload_copy(
    handle: u64,
    wire_part_index: usize,
    output: *mut u8,
    output_capacity: usize,
    required_len: *mut usize,
) -> i32 {
    // SAFETY: forwarded unchanged under the external pointer contract.
    unsafe { get(handle, output, output_capacity, required_len, Mode::Upload(wire_part_index)) }
}

/// Close exactly once; a second or stale close returns INVALID_HANDLE.
/// Dropping AdmittedRequest closes all temporary upload file descriptors.
#[unsafe(no_mangle)]
pub extern "C" fn rustpon_v1_multipart_session_close(handle: u64) -> i32 {
    match catch_unwind(|| {
        if handle == 0 { return INVALID_HANDLE; }
        let mut guard = match registry().lock() {
            Ok(guard) => guard,
            Err(_) => return PANIC,
        };
        if let Some(index) = guard.active.iter().position(|(id, _)| *id == handle) {
            guard.active.swap_remove(index);
            0
        } else {
            INVALID_HANDLE
        }
    }) {
        Ok(status) => status,
        Err(_) => PANIC,
    }
}
