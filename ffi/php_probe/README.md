# PHP 8.4 native RINIT/RSHUTDOWN lifecycle probe

This is a **research-only PHP extension** which demonstrates that the
Rust multipart session registry can be attached to actual PHP request
initialization and shutdown callbacks. It is not a replacement for
PHP's builtin HTTP POST/multipart parser, does not populate `$_FILES` or
`$_POST`, and does not implement an upload registry or remedy a PHP CVE.

The module uses real PHP 8.4 `PHP_RINIT_FUNCTION` and
`PHP_RSHUTDOWN_FUNCTION`. Each request receives a server-side Rust scope,
stored in thread-local Zend module globals. All sessions opened in that
scope are released at RSHUTDOWN, even when userland fails to close anything.

## Build and run (Linux)

First build the independent Rust FFI library. The PHP extension requires
PHP development headers, `phpize`, `php-config`, a working C compiler and
libtool.

```sh
cargo build --manifest-path ffi/Cargo.toml --offline
cd ffi/php_probe
phpize
./configure --enable-rustpon-probe
make -j2
cd ../..
python3 tools/php_probe_lifecycle.py \
  --module ffi/php_probe/modules/rustpon_probe.so \
  --library ffi/target/debug/librustpon_ffi.so
```

This executable probe only operates with a trusted administrator-defined
`rustpon_probe.spool_dir` PHP INI_SYSTEM setting. The test runner creates
a private 0700 directory and starts the local PHP CLI built-in HTTP server
with the extension loaded. The module exports three test-only functions:

- `rustpon_probe_open(string $body, string $boundary): bool`: open a
  scoped admitted multipart request supplied as a PHP string. The
  configured private spool directory is never a userland argument.
- `rustpon_probe_metadata(): string|false`: returns the typed, tagged
  `$_FILES` JSON projection for the most recently opened session.
- `rustpon_probe_copy(int $wireIndex): string|false`: returns the
  admitted binary file contents. Neither scope IDs nor handles are
  exposed to PHP userland.

The CI smoke test sends **32 HTTP requests through one reused PHP worker**:
normal return, explicit `exit`, multiple scoped uploads and an unhandled
exception. It compares OS-level open file descriptor counts after each
request, and checks no staging directory entries remain. These tests run
against the **built-in PHP CLI HTTP server**, not php-fpm itself. They do
exercise actual engine RINIT/RSHUTDOWN hooks; a separate php-fpm FastCGI
worker-lifecycle matrix remains required for production integration.

## Security limitations

This probe must not be exposed as a production extension. It currently
parses a complete request body supplied by PHP userland rather than
installing a replacement SAPI post handler, and only returns PHP string
objects. Native Zend `zval` arrays for `$_POST` and `$_FILES`, proper
upload tmp-path registration, uploads moved atomically, partial upload
errors, ingress quotas, and session-progress behavior remain outstanding.

The Rust FFI never calls Zend and does not expose Rust-owned pointers.
Zend allocation happens **after** the Rust function returns. Rust
`catch_unwind` does not catch Zend `longjmp` or process aborts; the
real engine request shutdown callback is what closes the scope. The
probe uses a global process-local lock and at most 8 concurrent scopes
and 8 live sessions. Memory and FD exhaustion policies and PHP-FPM
worker request deadlines still require review.

Because `INI_SYSTEM` settings are administrator-managed, do not
change the spool directory at runtime based on user-controlled data.
Its permissions and lifetime are the server's responsibility.
