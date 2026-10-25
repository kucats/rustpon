# Experimental C ABI: URL-encoded POST projection

The \`ffi/\` crate is separate from the core Rust library. The core keeps
\`#![forbid(unsafe_code)]\`; all raw-pointer operations are confined to the
small \`ffi/src/lib.rs\` boundary. **This is not a PHP extension, SAPI
integration or a fix for PHP security vulnerabilities.**

## ABI v1

\`include/rustpon_ffi.h\` defines:

\`\`\`c
uint32_t rustpon_ffi_abi_version(void);
int32_t rustpon_v1_form_post_json(
    const uint8_t *input, size_t input_len,
    uint8_t *output, size_t output_capacity, size_t *required_len
);
\`\`\`

Pass the complete dechunked \`application/x-www-form-urlencoded\` request body.
The function creates and discards a fresh Rust parser and POST tree on **every
call**. A NULL \`output\` with zero capacity returns \`BUFFER_TOO_SMALL\` and
writes the required byte length (without a terminator). Allocate that many
bytes in the caller and repeat the call with identical input, or provide a
sufficiently large output buffer immediately.

The output is the core's binary-safe, tagged JSON POST tree. It is not NUL
terminated. All byte strings are hex-encoded in JSON, with explicit typed
array keys. Input is capped at 256 KiB, serialized output at 1 MiB; the core's
additional parser work and POST tree quotas apply unchanged.

\`\`\`sh
cargo test --manifest-path ffi/Cargo.toml --offline
cargo clippy --manifest-path ffi/Cargo.toml --offline --all-targets -- -D warnings
cargo build --manifest-path ffi/Cargo.toml --offline
cc -std=c11 -Wall -Wextra -Werror -Iffi/include ffi/tests/abi_smoke.c \
    -Lffi/target/debug -lrustpon_ffi \
    -Wl,-rpath,"$PWD/ffi/target/debug" -o ffi/target/abi-smoke
ffi/target/abi-smoke
python3 tools/differential_ffi.py \
    --library ffi/target/debug/librustpon_ffi.so
\`\`\`

The example shared library filename is Linux-specific (\`.dylib\` on macOS).
The C smoke and PHP oracle run on Linux CI; pure Rust FFI tests, rustfmt and
Clippy also run on macOS.

## Safety contract at the Zend/C boundary

1. **No PHP or Zend callbacks inside Rust:** do not call \`zend_bailout\`,
   \`zend_throw_exception\`, allocation helpers or other Zend APIs from the
   Rust side. Zend's \`longjmp\` must never cross a live Rust stack frame.
2. **C owns the buffers:** input, output, and the \`size_t\` result are live,
   correctly aligned, non-overlapping and unchanged by other threads during
   the call. Invalid C pointers are *undefined behavior* even if Rust checks
   for NULL and size bounds. Handle input pointers as untrusted in the
   *caller*, validating lengths against actual allocated storage.
3. **No retained pointers or callbacks:** the ABI has no \`void*\` handles,
   Rust allocations exposed to C, global per-request state, callbacks, or
   server-supplied file paths. The Rust-owned parser and JSON buffer are
   destroyed before returning to the caller.
4. **Return status, then touch PHP:** on return, the C shim can validate
   \`RUSTPON_OK\` and output bytes, and *then* invoke Zend APIs to construct
   actual \`zval\` objects or report errors. Convert status codes outside the
   FFI call. Never hold a Rust borrow across a PHP call.
5. **Fail without partial publication:** the bridge builds the full JSON
   internally. Too-small C output buffers remain completely untouched.
   Parsing errors, tree quota errors, and output quota errors never write to
   the C output. \`catch_unwind\` converts ordinary Rust unwinding panics
   to \`RUSTPON_PANIC\`, but cannot recover from a process abort, SIGSEGV
   from invalid C pointers, or a Zend longjmp. It is **not** an isolation
   boundary.
6. **Strict field behavior:** the core intentionally rejects decoded NUL
   bytes in field names and has resource caps that may reject inputs accepted
   by some PHP configurations. Do not silently fall back to PHP's native
   parser after a policy rejection without a separate explicit policy.

A future PHP module needs an independently reviewed, explicit C-to-Zend
construction layer, a negative/failure test matrix, worker reuse tests, and
PHP startup/shutdown integration. Multipart upload FD handoff and
\`$_FILES\` registry semantics are **not** exposed through this ABI yet.
