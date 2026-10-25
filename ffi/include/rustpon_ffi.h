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
    RUSTPON_OUTPUT_LIMIT = 6
};

uint32_t rustpon_ffi_abi_version(void);

int32_t rustpon_v1_form_post_json(
    const uint8_t *input,
    size_t input_len,
    uint8_t *output,
    size_t output_capacity,
    size_t *required_len
);

#ifdef __cplusplus
}
#endif
#endif
