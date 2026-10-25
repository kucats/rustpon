/* C caller smoke test: no PHP linked or callbacks. Synthetic data only. */
#include "rustpon_ffi.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void round_trip(const uint8_t *input, size_t len) {
    size_t required = SIZE_MAX;
    int32_t status = rustpon_v1_form_post_json(input, len, NULL, 0, &required);
    assert(status == RUSTPON_BUFFER_TOO_SMALL);
    assert(required > 0 && required <= 1024 * 1024);

    uint8_t *output = malloc(required + 1);
    assert(output != NULL);
    memset(output, 0xa5, required + 1);
    size_t copied = 0;
    status = rustpon_v1_form_post_json(input, len, output, required, &copied);
    assert(status == RUSTPON_OK && copied == required);
    assert(output[required] == 0xa5); /* No implicit NUL or overrun. */
    assert(output[0] == '[');

    memset(output, 0xa5, required + 1);
    copied = 0;
    status = rustpon_v1_form_post_json(input, len, output, required - 1, &copied);
    assert(status == RUSTPON_BUFFER_TOO_SMALL && copied == required);
    for (size_t i = 0; i < required + 1; ++i) {
        assert(output[i] == 0xa5); /* No partially published JSON. */
    }
    free(output);
}

int main(void) {
    assert(rustpon_ffi_abi_version() == 0x00010000);
    const uint8_t binary[] = "a=one&a=two&list[]=A&x=%00%ff&n[2]=z";
    const uint8_t invalid[] = "bad%00name=value";
    const uint8_t size_too_big[] = "a=x";
    size_t required = SIZE_MAX;

    for (int i = 0; i < 128; ++i) {
        round_trip(binary, sizeof(binary) - 1);
    }
    assert(rustpon_v1_form_post_json(invalid, sizeof(invalid) - 1,
                                    NULL, 0, &required) == RUSTPON_PARSE_REJECTED);
    assert(required == 0);
    assert(rustpon_v1_form_post_json(NULL, 1, NULL, 0,
                                    &required) == RUSTPON_INVALID_ARGUMENT);
    assert(rustpon_v1_form_post_json(size_too_big, 262145, NULL, 0,
                                    &required) == RUSTPON_INVALID_ARGUMENT);
    assert(rustpon_v1_form_post_json(NULL, 0, NULL, 0,
                                    &required) == RUSTPON_BUFFER_TOO_SMALL);
    assert(required > 0);
    puts("FFI C smoke: 128 round trips, failures and cleanup OK");
    return 0;
}
