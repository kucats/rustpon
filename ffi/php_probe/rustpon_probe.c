/*
 * Research-only PHP extension to test RINIT/RSHUTDOWN cleanup of the
 * Rust multipart session registry. Does not replace SAPI input parsing.
 * No Rust callbacks or Zend longjmp may occur while Rust is executing.
 */
#ifdef HAVE_CONFIG_H
#include "config.h"
#endif

#include "php.h"
#include "php_ini.h"
#include "rustpon_ffi.h"
#include <stdint.h>
#include <string.h>

#define PHP_RUSTPON_PROBE_VERSION "0.1.0"

ZEND_BEGIN_MODULE_GLOBALS(rustpon_probe)
    uint64_t scope;
    uint64_t last_handle;
ZEND_END_MODULE_GLOBALS(rustpon_probe)

ZEND_DECLARE_MODULE_GLOBALS(rustpon_probe)
#define RUSTPON_PROBE_G(v) ZEND_MODULE_GLOBALS_ACCESSOR(rustpon_probe, v)

static PHP_GINIT_FUNCTION(rustpon_probe)
{
    rustpon_probe_globals->scope = 0;
    rustpon_probe_globals->last_handle = 0;
}

PHP_INI_BEGIN()
    PHP_INI_ENTRY("rustpon_probe.spool_dir", "", PHP_INI_SYSTEM, NULL)
PHP_INI_END()

PHP_MINIT_FUNCTION(rustpon_probe)
{
    if (rustpon_ffi_abi_version() != 0x00010000) {
        return FAILURE;
    }
    REGISTER_INI_ENTRIES();
    return SUCCESS;
}

PHP_MSHUTDOWN_FUNCTION(rustpon_probe)
{
    UNREGISTER_INI_ENTRIES();
    return SUCCESS;
}

PHP_RINIT_FUNCTION(rustpon_probe)
{
#if defined(ZTS) && defined(COMPILE_DL_RUSTPON_PROBE)
    ZEND_TSRMLS_CACHE_UPDATE();
#endif
    RUSTPON_PROBE_G(scope) = 0;
    RUSTPON_PROBE_G(last_handle) = 0;
    uint64_t scope = 0;
    int32_t status = rustpon_v1_scope_begin(&scope);
    if (status != RUSTPON_OK || scope == 0) {
        return FAILURE;
    }
    RUSTPON_PROBE_G(scope) = scope;
    return SUCCESS;
}

PHP_RSHUTDOWN_FUNCTION(rustpon_probe)
{
    uint64_t scope = RUSTPON_PROBE_G(scope);
    RUSTPON_PROBE_G(scope) = 0;
    RUSTPON_PROBE_G(last_handle) = 0;
    if (scope != 0 && rustpon_v1_scope_end(scope) != RUSTPON_OK) {
        return FAILURE;
    }
    return SUCCESS;
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_rustpon_probe_open, 0, 0, 2)
    ZEND_ARG_TYPE_INFO(0, body, IS_STRING, 0)
    ZEND_ARG_TYPE_INFO(0, boundary, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_rustpon_probe_copy, 0, 0, 1)
    ZEND_ARG_TYPE_INFO(0, index, IS_LONG, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_rustpon_probe_metadata, 0, 0, 0)
ZEND_END_ARG_INFO()

/* No client-controlled spool path or scope ID is exposed. This is a probe,
 * not a general-purpose multipart upload API or upload registry. */
PHP_FUNCTION(rustpon_probe_open)
{
    zend_string *body, *boundary;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_STR(body)
        Z_PARAM_STR(boundary)
    ZEND_PARSE_PARAMETERS_END();

    const char *spool = INI_STR("rustpon_probe.spool_dir");
    if (!spool || spool[0] == '\0' || RUSTPON_PROBE_G(scope) == 0) {
        RETURN_FALSE;
    }
    uint64_t handle = 0;
    int32_t status = rustpon_v1_multipart_session_open_scoped(
        RUSTPON_PROBE_G(scope),
        (const uint8_t *)ZSTR_VAL(boundary), ZSTR_LEN(boundary),
        (const uint8_t *)ZSTR_VAL(body), ZSTR_LEN(body),
        (const uint8_t *)spool, strlen(spool), &handle
    );
    if (status != RUSTPON_OK || handle == 0) {
        RETURN_FALSE;
    }
    /* Previous sessions stay owned by this request and RSHUTDOWN will close
     * all of them. Only the last is exposed to these probe functions. */
    RUSTPON_PROBE_G(last_handle) = handle;
    RETURN_TRUE;
}

PHP_FUNCTION(rustpon_probe_copy)
{
    zend_long index;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_LONG(index)
    ZEND_PARSE_PARAMETERS_END();
    if (index < 0 || RUSTPON_PROBE_G(last_handle) == 0) {
        RETURN_FALSE;
    }
    uint64_t handle = RUSTPON_PROBE_G(last_handle);
    size_t needed = 0;
    int32_t status = rustpon_v1_multipart_session_upload_copy(
        handle, (size_t)index, NULL, 0, &needed
    );
    if (status == RUSTPON_OK && needed == 0) {
        RETURN_EMPTY_STRING();
    }
    if (status != RUSTPON_BUFFER_TOO_SMALL || needed > 1024 * 1024) {
        RETURN_FALSE;
    }
    /* PHP allocations happen strictly after Rust has returned. A Zend
     * bailout here cannot skip Rust stack cleanup; RSHUTDOWN owns session. */
    zend_string *result = zend_string_alloc(needed, 0);
    size_t copied = 0;
    status = rustpon_v1_multipart_session_upload_copy(
        handle, (size_t)index, (uint8_t *)ZSTR_VAL(result), needed, &copied
    );
    if (status != RUSTPON_OK || copied != needed) {
        zend_string_release(result);
        RETURN_FALSE;
    }
    ZSTR_VAL(result)[needed] = '\0';
    RETURN_STR(result);
}

PHP_FUNCTION(rustpon_probe_metadata)
{
    ZEND_PARSE_PARAMETERS_NONE();
    if (RUSTPON_PROBE_G(last_handle) == 0) {
        RETURN_FALSE;
    }
    uint64_t handle = RUSTPON_PROBE_G(last_handle);
    size_t needed = 0;
    int32_t status = rustpon_v1_multipart_session_files_json(
        handle, NULL, 0, &needed
    );
    if (status != RUSTPON_BUFFER_TOO_SMALL || needed > 1024 * 1024) {
        RETURN_FALSE;
    }
    zend_string *result = zend_string_alloc(needed, 0);
    size_t copied = 0;
    status = rustpon_v1_multipart_session_files_json(
        handle, (uint8_t *)ZSTR_VAL(result), needed, &copied
    );
    if (status != RUSTPON_OK || copied != needed) {
        zend_string_release(result);
        RETURN_FALSE;
    }
    ZSTR_VAL(result)[needed] = '\0';
    RETURN_STR(result);
}

static const zend_function_entry rustpon_probe_functions[] = {
    PHP_FE(rustpon_probe_open, arginfo_rustpon_probe_open)
    PHP_FE(rustpon_probe_copy, arginfo_rustpon_probe_copy)
    PHP_FE(rustpon_probe_metadata, arginfo_rustpon_probe_metadata)
    PHP_FE_END
};

zend_module_entry rustpon_probe_module_entry = {
    STANDARD_MODULE_HEADER,
    "rustpon_probe",
    rustpon_probe_functions,
    PHP_MINIT(rustpon_probe),
    PHP_MSHUTDOWN(rustpon_probe),
    PHP_RINIT(rustpon_probe),
    PHP_RSHUTDOWN(rustpon_probe),
    NULL,
    PHP_RUSTPON_PROBE_VERSION,
    PHP_MODULE_GLOBALS(rustpon_probe),
    PHP_GINIT(rustpon_probe),
    NULL,
    NULL,
    STANDARD_MODULE_PROPERTIES_EX
};

#ifdef COMPILE_DL_RUSTPON_PROBE
#ifdef ZTS
ZEND_TSRMLS_CACHE_DEFINE()
#endif
ZEND_GET_MODULE(rustpon_probe)
#endif
