# Experimental C ABI: URL-encoded POST projection

The `ffi/` crate is separate from the core Rust library. The core keeps
`#![forbid(unsafe_code)]`; all raw-pointer operations are confined to
the isolated FFI crate. **The ABI alone is not a PHP extension or a
security fix.** An optional PHP 8.4 RINIT/RSHUTDOWN lifecycle probe is
now available in [`php_probe/`](php_probe/README.md). This probe does
not replace SAPI parsing or register PHP uploads.

## ABI v1

`include/rustpon_ffi.h` defines:

```c
uint32_t rustpon_ffi_abi_version(void);
int32_t rustpon_v1_form_post_json(
    const uint8_t *input, size_t input_len,
    uint8_t *output, size_t output_capacity, size_t *required_len
);
```

Pass the complete dechunked `application/x-www-form-urlencoded` request body.
The function creates and discards a fresh Rust parser and POST tree on **every
call**. A NULL `output` with zero capacity returns `BUFFER_TOO_SMALL` and
writes the required byte length (without a terminator). Allocate that many
bytes in the caller and repeat the call with identical input, or provide a
sufficiently large output buffer immediately.

The output is the core's binary-safe, tagged JSON POST tree. It is not NUL
terminated. All byte strings are hex-encoded in JSON, with explicit typed
array keys. Input is capped at 256 KiB, serialized output at 1 MiB; the core's
additional parser work and POST tree quotas apply unchanged.

```sh
cargo test --manifest-path ffi/Cargo.toml --offline
cargo clippy --manifest-path ffi/Cargo.toml --offline --all-targets -- -D warnings
cargo build --manifest-path ffi/Cargo.toml --offline
cc -std=c11 -Wall -Wextra -Werror -Iffi/include ffi/tests/abi_smoke.c \
    -Lffi/target/debug -lrustpon_ffi \
    -Wl,-rpath,"$PWD/ffi/target/debug" -o ffi/target/abi-smoke
ffi/target/abi-smoke
python3 tools/differential_ffi.py \
    --library ffi/target/debug/librustpon_ffi.so
```

The example shared library filename is Linux-specific (`.dylib` on macOS).
The C smoke and PHP oracle run on Linux CI; pure Rust FFI tests, rustfmt and
Clippy also run on macOS.

## Safety contract at the Zend/C boundary

1. **No PHP or Zend callbacks inside Rust:** do not call `zend_bailout`,
   `zend_throw_exception`, allocation helpers or other Zend APIs from the
   Rust side. Zend's `longjmp` must never cross a live Rust stack frame.
2. **C owns the buffers:** input, output, and the `size_t` result are live,
   correctly aligned, non-overlapping and unchanged by other threads during
   the call. Invalid C pointers are *undefined behavior* even if Rust checks
   for NULL and size bounds. Handle input pointers as untrusted in the
   *caller*, validating lengths against actual allocated storage.
3. **No retained pointers or callbacks:** the ABI has no `void*` handles,
   Rust allocations exposed to C, global per-request state, callbacks, or
   server-supplied file paths. The Rust-owned parser and JSON buffer are
   destroyed before returning to the caller.
4. **Return status, then touch PHP:** on return, the C shim can validate
   `RUSTPON_OK` and output bytes, and *then* invoke Zend APIs to construct
   actual `zval` objects or report errors. Convert status codes outside the
   FFI call. Never hold a Rust borrow across a PHP call.
5. **Fail without partial publication:** the bridge builds the full JSON
   internally. Too-small C output buffers remain completely untouched.
   Parsing errors, tree quota errors, and output quota errors never write to
   the C output. `catch_unwind` converts ordinary Rust unwinding panics
   to `RUSTPON_PANIC`, but cannot recover from a process abort, SIGSEGV
   from invalid C pointers, or a Zend longjmp. It is **not** an isolation
   boundary.
6. **Strict field behavior:** the core intentionally rejects decoded NUL
   bytes in field names and has resource caps that may reject inputs accepted
   by some PHP configurations. Do not silently fall back to PHP's native
   parser after a policy rejection without a separate explicit policy.

A future PHP module needs an independently reviewed, explicit C-to-Zend
construction layer, a negative/failure test matrix, worker reuse tests, and
PHP startup/shutdown integration. Multipart upload FD handoff and
`$_FILES` registry semantics are **not** exposed through this ABI yet.


## Multipart admission C ABI

The independent C ABI now exports `rustpon_v1_multipart_files_json`
and `rustpon_v1_multipart_upload_copy`; see
[the full header](include/rustpon_ffi.h) for signatures and limits.
Both require a complete dechunked multipart request and a trusted,
pre-existing 0700 spool directory provided by the server. The directory
path is **not** an HTTP-controlled argument. Input is capped at 256 KiB,
boundary length at 70 bytes and JSON/output at 1 MiB. Ingress transport
and request time limits still need to be enforced upstream.

The metadata function returns the admitted, **typed tagged JSON**
`$_FILES` tree. The payload function copies the bytes of one accepted
upload, selected by its original zero-based multipart **wire part index**,
into a C-owned output buffer. It returns `RUSTPON_NOT_ADMITTED` (7) for
overwritten, rejected, no-file and out-of-range indices. An accepted
zero-byte file returns `RUSTPON_OK` with `*required_len=0`. For nonempty
data, use a zero-capacity query then supply the exact output size.
Short buffers remain untouched, and returned bytes are not NUL-terminated.

These two calls are **one-shot and stateless**. The size query and subsequent
copy each independently parse the complete body, stage files in the trusted
directory, enforce `admission::admit` and close *all* file descriptors before
return. The body must be identical across calls. This is **not** an actual
OS file-descriptor handoff, a streaming API, a PHP upload registry or a
Zend `zval` bridge. A native C/Zend registry layer is still required.

Linux CI compiles a C caller exercising 64 repeated multipart requests.
The PHP 8.4 SAPI differential suite tests 78 typed metadata projections and
36 upload copies/rejections with no remaining staging directory entries.


## Single-parse multipart session API (experimental)

The new `ffi/src/session.rs` provides an optional **stateful** C ABI that
avoids reparsing for every FILES metadata query or payload copy:

`rustpon_v1_multipart_session_open` takes the complete multipart body,
raw boundary and a *trusted* spool path, and writes a nonzero `uint64_t`
process-local handle. `rustpon_v1_multipart_session_files_json` and
`rustpon_v1_multipart_session_upload_copy` accept that handle, and
`rustpon_v1_multipart_session_close` irrevocably releases the session's
admitted request and any remaining upload FDs. The caller must close a
successful handle **exactly once**, even on every application error path.

### Ownership and limits

- Session IDs are opaque integers, not raw pointers and **not secrets**.
  They are monotonically allocated per process and must **never** be
  exposed to untrusted clients or transferred across PHP-FPM workers.
  Stale/unknown handles return `RUSTPON_INVALID_HANDLE=8`.
- No more than **8 active sessions** are allowed per process. Additional
  opens return `RUSTPON_SESSIONS_FULL=9` without retaining another request.
  All operations use one process-local mutex. Operations on a single
  handle, including close, are serialized. There are no Rust-to-C
  callbacks while this mutex is held.
- The parser reads the complete body exactly **once**, and admission
  closes rejected or overwritten descriptors. Accepted uploads remain
  open until close. This eliminates repeated parsing but **introduces
  request-scope state** and a hard obligation to release handles.
- The open call resets `*out_handle` to zero on error. An unsuccessful
  metadata/payload call resets `*required_len` to zero except for
  `RUSTPON_BUFFER_TOO_SMALL`, which reports its exact required size.
  The caller owns every output buffer and all output remains untouched
  on undersized-buffer errors.
- The same 256 KiB body, 70-byte boundary, 4096-byte spool path and
  1 MiB serialized output quotas apply as in the one-shot ABI.
  Transport ingress controls still need to enforce time and body limits.
- A Rust panic while the registry lock is held can poison its mutex;
  future calls fail closed and leaked request FDs may persist until
  process shutdown. This needs a deliberate PHP-FPM failure policy
  before production use. Invalid C pointers and process aborts remain
  outside the guarantees.

### PHP runtime integration constraint

**Do not integrate this API into PHP request handling until RSHUTDOWN
or an equivalent guaranteed finally/cleanup mechanism invokes close**
for every opened handle, including Zend exception and bailout paths.
Neither a C `goto cleanup` on a normal return nor a destructor in an
ordinary stack frame guarantees cleanup after Zend `longjmp`.
The C shim must never execute Zend allocator, exception or bailout
functions while a Rust call or registry mutex is active.

The session test suite includes a real C caller that opens all eight
allowed handles, checks that each accepted upload FD remains open while
the handle is live, verifies FD counts after every close, and rejects
stale/double closes. Linux also compares the stateful metadata and
36 payload copy/rejection cases to PHP 8.4 SAPI within the existing
78-case multipart corpus. A **research-only native PHP 8.4 probe module**
now verifies 32 reused HTTP requests via genuine RINIT/RSHUTDOWN callbacks.
It is not a replacement SAPI upload handler.


## PHP request scopes and actual RSHUTDOWN hook

New process-local `rustpon_v1_scope_begin`,
`rustpon_v1_multipart_session_open_scoped` and `rustpon_v1_scope_end`
create a PHP-request-scoped group of upload sessions. `scope_end` drops
only the FDs belonging to the identified group; other groups and legacy
unscoped sessions survive. A scope ID is not a secret or an HTTP
authorization token.

A compiled C test uses a **C-only setjmp/longjmp** to simulate a bailout
only after Rust has returned, then invokes the equivalent of RSHUTDOWN.
The test verifies 48 bailout cleanups, cross-request isolation, eight
simultaneously live scope quota, stale-scope rejection and open-FD
counts returning to baseline.

The separate [PHP 8.4 probe extension](php_probe/README.md) attaches these
scope operations to **real PHP RINIT and RSHUTDOWN** callbacks and exercises
32 requests in one built-in PHP HTTP-server worker, covering normal return,
explicit exit, multiple admitted sessions and unhandled exceptions.
The module only exposes test functions that accept a fully buffered
multipart body and return byte strings/JSON. PHP-FPM FastCGI lifetime
verification, real input-handler integration, native Zend arrays and
native upload registry behavior remain future work.
