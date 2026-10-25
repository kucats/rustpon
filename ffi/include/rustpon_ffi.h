#ifndef RUSTPON_FFI_H
#define RUSTPON_FFI_H

#include <stddef.h>
#include <stdint.h>

/*
 * Version 1 experimental ABI. No callbacks into PHP, no Zend pointers,
 * no retained input pointers and no unwind/longjmp crossing this boundary.
 *
 * All non-NULL pointers must point to valid, correctly aligned storage for
 * their stated lengths for the entire call. The input, output and required
 * size ranges must not overlap. The caller owns and frees all buffers.
 *
 * A NULL input is valid only when input_len == 0.
 * A NULL output with output_capacity == 0 requests the required size.
 * On RUSTPON_BUFFER_TOO_SMALL the output is untouched and *required_len is
 * the complete required byte count (excluding any NUL terminator).
 * On errors other than BUFFER_TOO_SMALL, *required_len is zero.
 * Returned bytes are binary-safe JSON, NOT NUL terminated.
 *
 * This function is pure per-call, has no handles or process-global mutable
 * state, and does not register PHP uploads. It is not a PHP extension.
 * Memory and CPU during parsing are controlled by the Rust core limits.
 * The bridge additionally caps input at 256 KiB and output at 1 MiB.
 */
#ifdef __cplusplus
extern "C" {
#endif

enum rustpon_status {
    RUSTPON_OK = 0,
    RUSTPON_INVALID_ARGUMENT = 1,
    RUSTPON_PARSE_REJECTED = 2,
    RUSTPON_POST_REJECTED = 3,
    RUSTPON_BUFFER_TOO_SMALL = 4,
    RUSTPON_PANIC = 5,
    RUSTPON_OUTPUT_LIMIT = 6,
    RUSTPON_NOT_ADMITTED = 7,
    RUSTPON_INVALID_HANDLE = 8,
    RUSTPON_SESSIONS_FULL = 9
};

uint32_t rustpon_ffi_abi_version(void);

int32_t rustpon_v1_form_post_json(
    const uint8_t *input,
    size_t input_len,
    uint8_t *output,
    size_t output_capacity,
    size_t *required_len
);

/*
 * Complete multipart body + trusted, existing private spool directory.
 * boundary is raw bytes without "--". spool_dir is a UTF-8 byte path
 * (not NUL terminated, may not contain NUL) that must be supplied by trusted
 * server configuration, not an HTTP-controlled field.
 *
 * The parser stages uploads anonymously then closes them before returning.
 * _files_json returns typed tagged JSON of the admitted $_FILES metadata.
 * _upload_copy returns binary bytes for an admitted wire part index only.
 * A zero-byte admitted upload returns OK with *required_len = 0.
 * Invalid/rejected/overwritten indices return NOT_ADMITTED.
 *
 * Both calls are one-shot, re-parse on each query/copy invocation and have
 * no retained handles. Never assume a request-specific resource persists.
 * Accepted data is still copied, not transferred as a raw OS file descriptor.
 * Same memory, pointer and output publication contract as form_post_json.
 * Input and body each must be <= 256 KiB, boundary <= 70 bytes, spool path
 * <= 4096 bytes. Only Linux/macOS prototype targets are supported.
 */
int32_t rustpon_v1_multipart_files_json(
    const uint8_t *boundary, size_t boundary_len,
    const uint8_t *body, size_t body_len,
    const uint8_t *spool_dir, size_t spool_len,
    uint8_t *output, size_t output_capacity,
    size_t *required_len
);

int32_t rustpon_v1_multipart_upload_copy(
    const uint8_t *boundary, size_t boundary_len,
    const uint8_t *body, size_t body_len,
    const uint8_t *spool_dir, size_t spool_len,
    size_t wire_part_index,
    uint8_t *output, size_t output_capacity,
    size_t *required_len
);

/*
 * Experimental stateful multipart sessions: one parse + one admission.
 * Integer handles are NOT secrets and must never be exposed to web clients.
 * At most 8 sessions may be live per process. A session owns anonymous
 * upload descriptors until explicitly closed; callers MUST close in a
 * request-shutdown/finally path, including on Zend bailout. Leaked handles
 * retain FDs until process termination and exhaust the pool.
 *
 * Operations are serialized by a process-local mutex. No callbacks into
 * Zend occur while holding it. These functions are not currently wired into
 * PHP-FPM; worker request-shutdown integration needs separate review.
 * A stale, invalid, already closed handle returns INVALID_HANDLE (8).
 * Full handle pool returns SESSIONS_FULL (9).
 * All input/output pointers follow the same C ABI validity requirements.
 */
int32_t rustpon_v1_multipart_session_open(
    const uint8_t *boundary, size_t boundary_len,
    const uint8_t *body, size_t body_len,
    const uint8_t *spool_dir, size_t spool_len,
    uint64_t *out_handle
);
int32_t rustpon_v1_multipart_session_files_json(
    uint64_t handle, uint8_t *output, size_t output_capacity,
    size_t *required_len
);
int32_t rustpon_v1_multipart_session_upload_copy(
    uint64_t handle, size_t wire_part_index,
    uint8_t *output, size_t output_capacity, size_t *required_len
);
int32_t rustpon_v1_multipart_session_close(uint64_t handle);

/*
 * Request scope for PHP RINIT/RSHUTDOWN or equivalent.
 * `scope_begin` returns a process-local integer scope ID.
 * `session_open_scoped` binds an admitted session to that scope.
 * `scope_end` atomically invalidates all its sessions and closes the FDs.
 * Other scopes and unscoped legacy sessions are unaffected.
 * Unknown or already ended scopes return RUSTPON_INVALID_HANDLE (8).
 * At most eight simultaneous scopes and eight sessions per process.
 * Successful scope_begin MUST have exactly one matching scope_end in the
 * PHP request-shutdown hook, including post-exception/bailout requests.
 * Scope IDs are not access controls; keep them within the trusted C shim.
 */
int32_t rustpon_v1_scope_begin(uint64_t *out_scope);
int32_t rustpon_v1_multipart_session_open_scoped(
    uint64_t scope,
    const uint8_t *boundary, size_t boundary_len,
    const uint8_t *body, size_t body_len,
    const uint8_t *spool_dir, size_t spool_len,
    uint64_t *out_handle
);
int32_t rustpon_v1_scope_end(uint64_t scope);

#ifdef __cplusplus
}
#endif
#endif
