#define _XOPEN_SOURCE 700
/* Simulated PHP request shutdown and C-only bailout around Rust session API. */
#include "rustpon_ffi.h"
#include <assert.h>
#include <dirent.h>
#include <setjmp.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static size_t fd_count(void) {
    DIR *d = opendir("/proc/self/fd");
    assert(d != NULL);
    size_t count = 0;
    struct dirent *entry;
    while ((entry = readdir(d)) != NULL) {
        if (strcmp(entry->d_name, ".") != 0 && strcmp(entry->d_name, "..") != 0) {
            ++count;
        }
    }
    assert(closedir(d) == 0);
    return count;
}

static const uint8_t multipart[] =
    "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"ok.bin\"\r\n"
    "\r\nDATA\r\n--b--\r\n";

static uint64_t open_in_scope(uint64_t scope, const char *spool) {
    uint64_t handle = UINT64_MAX;
    int32_t status = rustpon_v1_multipart_session_open_scoped(
        scope, (const uint8_t *)"b", 1, multipart, sizeof(multipart) - 1,
        (const uint8_t *)spool, strlen(spool), &handle
    );
    assert(status == RUSTPON_OK && handle != 0 && handle != UINT64_MAX);
    return handle;
}

static void expect_payload(uint64_t handle) {
    uint8_t buffer[5] = {0, 0, 0, 0, 0xa5};
    size_t needed = SIZE_MAX;
    assert(rustpon_v1_multipart_session_upload_copy(
        handle, 0, buffer, 4, &needed) == RUSTPON_OK);
    assert(needed == 4 && memcmp(buffer, "DATA", 4) == 0 && buffer[4] == 0xa5);
}

static void simulate_bailout(const char *spool) {
    uint64_t *scope = malloc(sizeof(*scope));
    assert(scope != NULL);
    *scope = 0;
    jmp_buf request;
    if (setjmp(request) == 0) {
        assert(rustpon_v1_scope_begin(scope) == RUSTPON_OK);
        uint64_t leaked = open_in_scope(*scope, spool);
        expect_payload(leaked);
        /* Only jump between C frames; Rust has fully returned. */
        longjmp(request, 1);
    }
    /* Represents a C/Zend RSHUTDOWN callback, not a PHP extension. */
    assert(*scope != 0);
    assert(rustpon_v1_scope_end(*scope) == RUSTPON_OK);
    free(scope);
}

int main(void) {
    char spool[] = "/tmp/rustpon-ffi-scope-XXXXXX";
    assert(mkdtemp(spool) != NULL);
    size_t initial = fd_count();

    uint64_t a = 0, b = 0, legacy = 0;
    assert(rustpon_v1_scope_begin(&a) == RUSTPON_OK);
    assert(rustpon_v1_scope_begin(&b) == RUSTPON_OK);
    assert(a != 0 && b != 0 && b != a);
    uint64_t a1 = open_in_scope(a, spool);
    uint64_t a2 = open_in_scope(a, spool);
    uint64_t b1 = open_in_scope(b, spool);
    assert(rustpon_v1_multipart_session_open(
        (const uint8_t *)"b", 1, multipart, sizeof(multipart) - 1,
        (const uint8_t *)spool, strlen(spool), &legacy) == RUSTPON_OK);
    assert(fd_count() == initial + 4);

    assert(rustpon_v1_scope_end(a) == RUSTPON_OK);
    assert(fd_count() == initial + 2);
    assert(rustpon_v1_scope_end(a) == RUSTPON_INVALID_HANDLE);
    uint8_t out[8] = {0};
    size_t required = 999;
    assert(rustpon_v1_multipart_session_upload_copy(
        a1, 0, out, sizeof(out), &required) == RUSTPON_INVALID_HANDLE);
    assert(required == 0);
    assert(rustpon_v1_multipart_session_upload_copy(
        a2, 0, out, sizeof(out), &required) == RUSTPON_INVALID_HANDLE);
    uint64_t invalid = UINT64_MAX;
    assert(rustpon_v1_multipart_session_open_scoped(
        a, (const uint8_t *)"b", 1, multipart, sizeof(multipart) - 1,
        (const uint8_t *)spool, strlen(spool), &invalid) == RUSTPON_INVALID_HANDLE);
    assert(invalid == 0);
    expect_payload(b1);
    expect_payload(legacy);

    assert(rustpon_v1_scope_end(b) == RUSTPON_OK);
    assert(fd_count() == initial + 1);
    expect_payload(legacy);
    assert(rustpon_v1_multipart_session_close(legacy) == RUSTPON_OK);
    assert(fd_count() == initial);

    for (int i = 0; i < 48; ++i) {
        simulate_bailout(spool);
        assert(fd_count() == initial);
    }

    uint64_t scopes[8] = {0};
    for (int i = 0; i < 8; ++i) {
        assert(rustpon_v1_scope_begin(&scopes[i]) == RUSTPON_OK);
    }
    invalid = 99;
    assert(rustpon_v1_scope_begin(&invalid) == RUSTPON_SESSIONS_FULL);
    assert(invalid == 0);
    for (int i = 0; i < 8; ++i) {
        assert(rustpon_v1_scope_end(scopes[i]) == RUSTPON_OK);
    }
    assert(fd_count() == initial);
    assert(rmdir(spool) == 0);
    puts("FFI scoped cleanup: multi-request isolation, 48 C-only bailouts, 8-scope quota, all FDs reclaimed");
    return 0;
}
