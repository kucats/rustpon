#define _XOPEN_SOURCE 700
/* C caller smoke test: no PHP linked or callbacks. Synthetic data only. */
#include "rustpon_ffi.h"
#include <assert.h>
#include <dirent.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/stat.h>

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


static void multipart_smoke(void) {
    char spool[] = "/tmp/rustpon-ffi-c-XXXXXX";
    assert(mkdtemp(spool) != NULL);
    const uint8_t boundary[] = "b";
    const uint8_t entity[] =
        "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"old.bin\"\r\n"
        "\r\nOLD\r\n"
        "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"new.bin\"\r\n"
        "\r\nNEW-DATA\r\n"
        "--b\r\nContent-Disposition: form-data; name=\"none\"; filename=\"\"\r\n"
        "\r\n\r\n"
        "--b--\r\n";
    const size_t entity_len = sizeof(entity) - 1;
    const size_t dir_len = strlen(spool);
    size_t required = 0;
    for (size_t iteration = 0; iteration < 64; ++iteration) {
        assert(rustpon_v1_multipart_files_json(
            boundary, 1, entity, entity_len, (uint8_t *)spool, dir_len,
            NULL, 0, &required) == RUSTPON_BUFFER_TOO_SMALL);
        assert(required > 0 && required <= 1024 * 1024);
        uint8_t *json = malloc(required + 1);
        assert(json != NULL);
        memset(json, 0xa5, required + 1);
        size_t written = 0;
        assert(rustpon_v1_multipart_files_json(
            boundary, 1, entity, entity_len, (uint8_t *)spool, dir_len,
            json, required, &written) == RUSTPON_OK);
        assert(written == required && json[required] == 0xa5);
        assert(json[0] == '[');
        free(json);

        /* Original part 0 has been superseded by part 1. */
        uint8_t payload[9] = {0};
        memset(payload, 0xa5, sizeof(payload));
        required = 18;
        assert(rustpon_v1_multipart_upload_copy(
            boundary, 1, entity, entity_len, (uint8_t *)spool, dir_len, 0,
            payload, sizeof(payload), &required) == RUSTPON_NOT_ADMITTED);
        assert(required == 0);
        for (size_t i = 0; i < sizeof(payload); ++i) {
            assert(payload[i] == 0xa5);
        }
        assert(rustpon_v1_multipart_upload_copy(
            boundary, 1, entity, entity_len, (uint8_t *)spool, dir_len, 1,
            NULL, 0, &required) == RUSTPON_BUFFER_TOO_SMALL);
        assert(required == 8);
        memset(payload, 0xa5, sizeof(payload));
        written = 0;
        assert(rustpon_v1_multipart_upload_copy(
            boundary, 1, entity, entity_len, (uint8_t *)spool, dir_len, 1,
            payload, 7, &written) == RUSTPON_BUFFER_TOO_SMALL);
        assert(written == 8 && payload[0] == 0xa5);
        written = 0;
        assert(rustpon_v1_multipart_upload_copy(
            boundary, 1, entity, entity_len, (uint8_t *)spool, dir_len, 1,
            payload, 8, &written) == RUSTPON_OK);
        assert(written == 8 && memcmp(payload, "NEW-DATA", 8) == 0);
        assert(payload[8] == 0xa5);
        assert(rustpon_v1_multipart_upload_copy(
            boundary, 1, entity, entity_len, (uint8_t *)spool, dir_len, 2,
            payload, sizeof(payload), &written) == RUSTPON_NOT_ADMITTED);
    }
    assert(rmdir(spool) == 0);
}

static size_t fd_count(void) {
    DIR *dir = opendir("/proc/self/fd");
    assert(dir != NULL);
    size_t count = 0;
    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
        if (strcmp(entry->d_name, ".") != 0 && strcmp(entry->d_name, "..") != 0) {
            ++count;
        }
    }
    assert(closedir(dir) == 0);
    return count;
}

static void stateful_multipart_smoke(void) {
    char spool[] = "/tmp/rustpon-ffi-session-XXXXXX";
    assert(mkdtemp(spool) != NULL);
    const uint8_t entity[] =
        "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"old.bin\"\r\n\r\nOLD\r\n"
        "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"new.bin\"\r\n\r\nACCEPTED\r\n"
        "--b--\r\n";
    size_t baseline_fds = fd_count();
    uint64_t handles[8] = {0};
    for (size_t i = 0; i < 8; ++i) {
        assert(rustpon_v1_multipart_session_open(
            (const uint8_t *)"b", 1, entity, sizeof(entity) - 1,
            (const uint8_t *)spool, strlen(spool), &handles[i]) == RUSTPON_OK);
        assert(handles[i] > 0);
        assert(fd_count() == baseline_fds + i + 1);
    }
    uint64_t excess = UINT64_MAX;
    assert(rustpon_v1_multipart_session_open(
        (const uint8_t *)"b", 1, entity, sizeof(entity) - 1,
        (const uint8_t *)spool, strlen(spool), &excess) == RUSTPON_SESSIONS_FULL);
    assert(excess == 0);
    /* Anonymous FDs keep their payload after the configured spool directory
     * disappears. If session copy reparses, these reads would fail. */
    assert(rmdir(spool) == 0);

    for (size_t i = 0; i < 8; ++i) {
        size_t required = UINT64_MAX;
        uint8_t buffer[32];
        memset(buffer, 0xa5, sizeof(buffer));
        assert(rustpon_v1_multipart_session_upload_copy(
            handles[i], 0, buffer, sizeof(buffer), &required) == RUSTPON_NOT_ADMITTED);
        assert(required == 0 && buffer[0] == 0xa5);
        assert(rustpon_v1_multipart_session_upload_copy(
            handles[i], 1, NULL, 0, &required) == RUSTPON_BUFFER_TOO_SMALL);
        assert(required == 8);
        size_t got = 0;
        assert(rustpon_v1_multipart_session_upload_copy(
            handles[i], 1, buffer, 7, &got) == RUSTPON_BUFFER_TOO_SMALL);
        assert(got == 8 && buffer[0] == 0xa5);
        assert(rustpon_v1_multipart_session_upload_copy(
            handles[i], 1, buffer, sizeof(buffer), &got) == RUSTPON_OK);
        assert(got == 8 && memcmp(buffer, "ACCEPTED", 8) == 0);
        assert(rustpon_v1_multipart_session_files_json(
            handles[i], NULL, 0, &got) == RUSTPON_BUFFER_TOO_SMALL);
        assert(got > 0);
        assert(rustpon_v1_multipart_session_close(handles[i]) == RUSTPON_OK);
        assert(fd_count() == baseline_fds + 7 - i);
        assert(rustpon_v1_multipart_session_close(handles[i]) == RUSTPON_INVALID_HANDLE);
        assert(rustpon_v1_multipart_session_upload_copy(
            handles[i], 1, buffer, sizeof(buffer), &got) == RUSTPON_INVALID_HANDLE);
        assert(got == 0);
    }
    assert(rustpon_v1_multipart_session_close(0) == RUSTPON_INVALID_HANDLE);
    assert(mkdir(spool, 0700) == 0);
    uint64_t renewed = 0;
    assert(rustpon_v1_multipart_session_open(
        (const uint8_t *)"b", 1, entity, sizeof(entity) - 1,
        (const uint8_t *)spool, strlen(spool), &renewed) == RUSTPON_OK);
    assert(renewed > handles[7]);
    assert(fd_count() == baseline_fds + 1);
    assert(rustpon_v1_multipart_session_close(renewed) == RUSTPON_OK);
    assert(fd_count() == baseline_fds);
    assert(rmdir(spool) == 0);
}

int main(void) {
    assert(rustpon_ffi_abi_version() == 0x00010000);
    const uint8_t binary[] = "a=one&a=two&list[]=A&x=%00%ff&n[2]=z";
    const uint8_t invalid[] = "bad%00name=value";
    uint8_t *size_too_big = calloc(262145, sizeof(uint8_t));
    assert(size_too_big != NULL);
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
    free(size_too_big);
    multipart_smoke();
    stateful_multipart_smoke();
    puts("FFI C smoke: 128 form, 64 one-shot multipart, 8 stateful handles, quota/close checks OK");
    return 0;
}
