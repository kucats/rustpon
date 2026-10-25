# rustpon

`rustpon` is an experimental, standalone Rust implementation of bounded
multipart request parsing and PHP-style `$_POST` field projection. It has no
PHP/Zend integration or FFI, opens no listener, and makes no production
readiness claim. The crate forbids unsafe Rust and has no third-party Cargo
dependencies.

The parser produces an ordered wire representation and stages uploads in
anonymous files. A separate adapter models selected 64-bit PHP 8.4 field-name,
array-key, collision, and append-index behavior. It deliberately returns a
bounded error for malformed or exhausted cases where PHP may partially publish
values. This prototype does not implement `$_FILES` materialization,
`MAX_FILE_SIZE`, extension callbacks, sessions, or Zend lifecycle handling.

## Requirements and checks

Rust stable with edition 2024 support and a Unix target.
The GitHub Actions workflow runs formatting, tests, and Clippy on Linux and
macOS. No PHP installation is needed for the Rust test suite.

```sh
cargo fmt --all -- --check
cargo test --offline --locked --all-targets
cargo clippy --offline --locked --all-targets -- -D warnings
```

## Command-line harness

The harness reads the multipart body from stdin and writes JSON only after a
complete request succeeds. Its spool directory must be private and writable by
the caller. It starts no listener.

```sh
cargo build --offline --locked --bin multipart
spool_dir="$(mktemp -d)"
printf '%b' '--demo\r\nContent-Disposition: form-data; name="hello"\r\n\r\nworld\r\n--demo--\r\n' |
  target/debug/multipart demo "$spool_dir"
rmdir "$spool_dir"
```

Add `--post` before the boundary to emit the tagged field tree:

```sh
spool_dir="$(mktemp -d)"
target/debug/multipart --post demo "$spool_dir" < request-body.bin
rmdir "$spool_dir"
```

The JSON uses tagged strings (`["s", "hex-bytes"]`) and integer keys
(`["i", "decimal"]`) so binary values and PHP key types remain unambiguous.
The ordinary mode reports raw names and payloads as hexadecimal. Upload payload
bytes remain available through the Rust API; the POST tree contains field
values only.

## Wire parser contract

- The caller supplies an extracted boundary and dechunked body. HTTP headers,
  chunk framing, decompression, and transport deadlines are upstream concerns.
- Boundaries contain 1–70 allowed ASCII bytes. Delimiters and header lines use
  CRLF. The body begins with `--BOUNDARY`.
- Only `Content-Disposition: form-data` with a nonempty `name` and optional
  `filename` is supported. Header and parameter names ignore ASCII case;
  quoted pairs are unescaped. Raw names, filenames, binary values, duplicates,
  and part order are retained.
- Optional content type is restricted to `token/token`; parameters and nested
  multipart types are unsupported. Invalid/duplicate essential headers,
  folding, control bytes, transfer encodings, and unknown disposition
  parameters are rejected.
- Preambles, epilogues, delimiter padding, and bare LF are rejected. A closing
  delimiter commits the parse; only EOF or one trailing CRLF followed by EOF is
  accepted.
- Call `Parser::finish` only at transport EOF. A failed feed or dropped parser
  aborts the request and releases all staged uploads. `parse_reader` provides
  the same lifecycle for a `Read` source.

## Resource and ownership model

`Limits` bounds body bytes, parts, fields, files, header counts and bytes,
field/file payloads, names, retained arena bytes, and parser capacity. Uploads
use exclusive mode-0600 files in the caller's private directory and are
unlinked before writing. Submitted filenames are never used as paths. The
successful request owns its upload descriptors; dropping it releases them.

The adapter adds limits for normalized name bytes, nesting depth, retained
nodes and keys, protected upload names, lookup work, and combined retained
capacities. Its ordered flat node arena avoids recursive ownership and charges
nodes abandoned by later scalar/array replacement until the request is
dropped. Parsing and conversion return an error before the CLI writes a
partial JSON response.

These capacity counters are accounting limits, not RSS or complete system-OOM
guarantees. Allocator metadata, kernel buffers, caller memory, output buffering,
and process-wide concurrency are outside the model. The caller must keep the
spool directory trusted and stable for the request lifetime.

## PHP-style adapter scope

The adapter preserves ordered fields, applies PHP-style root/bracket
normalization, expands bracket paths, handles numeric string keys and
append-after-negative-index behavior, and models scalar/array replacement.
Upload names protect their canonical and generated `tmp_name` keys from later
fields. A malformed upload name rejects the whole conversion rather than
returning a partial tree.

The behavior is a bounded compatibility experiment for 64-bit PHP 8.4-style
semantics; it is not a substitute for differential validation against every
PHP build, extension, configuration, or request lifecycle. Framing policy is
intentionally strict and can differ from PHP's parser on malformed input.
There is no claim that this prototype fixes any PHP vulnerability.

## Test coverage

The Rust tests exercise arbitrary input chunking, binary fields, duplicate and
colliding names, malformed/truncated framing, quota edges, upload cleanup,
request destruction, reservation failures, adapter normalization and key
semantics, upload-name protection, and bounded serialization. They include
deterministic random input/partition smoke tests. No libFuzzer/AFL++, sanitizer,
Miri, or true system-OOM campaign has been run.

## URL-encoded request support

`urlencoded::FormParser` incrementally decodes dechunked
`application/x-www-form-urlencoded` bodies, including percent escapes that
cross feed boundaries. It returns the same ordered `Request` IR used by
multipart, so `Request::into_post` works without a separate PHP adapter.
`urlencoded::parse_reader` provides a blocking reader wrapper.

The decoder treats `&` as the separator and the first `=` as the
name/value separator. It decodes `+` as space and valid percent triplets
into bytes. Malformed percent escapes remain literal. Empty names and
blank pairs are skipped. Decoded NUL bytes in names are rejected deliberately.
The input byte limit and temporary retained-capacity limit apply to raw
encoded bytes. Name/value limits also apply to raw encoded lengths, which
is stricter than PHP for heavily percent-encoded input.

To exercise the same POST projection from a shell:

```sh
printf '%s' 'x=hello+world&a%5By%5D=2' | cargo run --offline --locked --bin urlencoded
```

This decoder is a standalone prototype: no Cookie parser, configurable
`arg_separator.input`, SAPI entry point, FFI, or `$_GET` publication has
been implemented. PHP runtime memory and request-lifecycle vulnerabilities
are not fixed by this addition.


## PHP 8.4 URL-encoded differential test

Linux CI now compares the Rust URL-encoded POST tree with real PHP 8.4
`parse_str()` using binary-safe tagged JSON (typed keys, original insertion
order and hexadecimal field values). The synthetic corpus contains 28 fixed
cases plus 64 deterministic generated cases, covering duplicates, percent
escapes, binary/Unicode values, scalar/array collisions and integer keys.

```sh
cargo build --offline --locked --bin urlencoded
python3 tools/differential_urlencoded.py --rust-bin target/debug/urlencoded
```

Three strict rejections (decoded NUL name and excessive raw name/value sizes)
are tested separately. PHP 8.4 is selected by family and CI prints the exact
installed patch version. It is not pinned to precisely PHP 8.4.27. Tests
cover `parse_str()` semantics, not HTTP SAPI registration, `$_FILES`, session
upload progress or Rust/Zend FFI. These boundaries remain future integration
work.


## PHP multipart SAPI differential coverage

A second differential suite exercises PHP's real HTTP request registration
via the CLI built-in server bound to `127.0.0.1`. The client sends actual
`multipart/form-data` requests, then compares the PHP `$_POST` tree
against Rust's `multipart --post` CLI projection. For flat file fields,
the same suite also compares submitted filenames, MIME types, lengths,
and actual upload bytes read from `$_FILES` temporary files with the
raw Rust multipart IR. Server-side temporary paths are excluded from
test output and from the repository.

```sh
cargo build --offline --locked --bin multipart
python3 tools/differential_multipart.py --rust-bin target/debug/multipart
```

The corpus includes deterministic synthetic examples, nested and
duplicate field names, binary values, upload parts and pseudorandom
combinations. PHP 8.4 is selected at the major/minor version level in CI.
The file comparisons currently cover distinct flat file-field names;
nested `$_FILES` registration and common error codes are covered by later
sections. Other upload error paths, PHP/Zend integration, and session upload
progress remain future work.

## Delimiter CPU-work budget

The multipart parser now enforces `Limits::boundary_comparisons` independently
of the input byte quota. The default allows 16 comparisons per configured
default maximum body byte, rejecting highly repetitive near-delimiters when
the work budget is exhausted. The counter is checked before each comparison,
and exhaustion aborts the entire request, dropping all upload handles. A
dedicated regression checks the exact/plus-one budget and a maximum-length
near-match upload, followed by request isolation.

This work quota bounds *delimiter comparisons*, not total CPU time, overhead
from allocations, I/O, PHP runtime callbacks, or the upstream HTTP decoder.
A linear-time matcher remains a worthwhile future replacement.

## Experimental nested FILES projection

`files::project(&request, files::Limits::default())` creates a bounded,
transactional metadata-only PHP-shaped tree from the completed multipart
wire request. It preserves upload-name nesting, implicit array append
indices, collisions, and the metadata columns `name`, `full_path`,
`type`, `tmp_name`, `error`, and `size`. The input `Request`
retains all anonymous upload file descriptors. The projection never
contains the real temporary path; for successful uploads `tmp_name` is
the constant `__RUSTPON_STAGED__`. File sizes and error codes are
represented as ASCII **strings** in this testing tree, not native PHP
integer zvals. A real PHP/Zend adapter still needs typed values, validated
filesystem handoff, and session/upload-progress lifecycle separation.

```sh
cargo build --offline --locked --bin multipart
target/debug/multipart --files demo "$spool_dir" < request-body.bin
```

The PHP SAPI differential harness now also compares the sanitized nested
`$_FILES` metadata tree, including empty filename error 4, same-name
overwrite, numeric indices, and nested append arrays. Raw upload history
comparison is restricted to unambiguous, distinct flat file fields because
the Rust wire IR intentionally keeps overwritten parts. The metadata
projection has its own byte/capacity limits in addition to the existing
POST tree limits. These bounds are not an RSS guarantee.

## Ordered MAX_FILE_SIZE handling

The FILES metadata projection now recognizes the form-supplied
`MAX_FILE_SIZE` field (ASCII case-insensitive) **only when encountered before
an upload**. It parses a signed decimal prefix using bounded arithmetic.
When the uploaded payload exceeds a nonzero form limit it represents PHP's
`UPLOAD_ERR_FORM_SIZE` (2): the submitted `name`/`full_path` remain visible,
while `type`, `tmp_name` and `size` are empty, empty and zero respectively.
Empty filenames continue to yield `UPLOAD_ERR_NO_FILE` (4). A subsequent
`MAX_FILE_SIZE` field updates the limit for later uploads only.

The differential suite now checks these behaviors against the PHP 8.4 SAPI,
including a threshold-equal upload, an after-upload field, a zero limit,
negative limit, lower-case field name, nested uploads, and a later limit
change. The exact reference patch revision is printed in Linux CI.

**Security boundary:** This is a metadata-only compatibility projection.
The Rust wire Request still owns the staged payload for rejected uploads.
Consumers must enforce upload acceptance separately before copying staged
bytes. `MAX_FILE_SIZE` is supplied by the client and cannot replace
server-controlled `post_max_size`, `upload_max_filesize`, or Rust's resource
budgets. The remaining PHP upload error states (1, 3, 6, 7, 8), truncation
behavior and native integer zvals still require implementation and testing.


## Consuming upload admission gate

The **optional** high-level `admission::admit(request, policy)` consumes the
raw `Request`, verifies server-controlled body and per-file limits, and
builds a bounded metadata projection transactionally. Server quota failure
returns an error and drops *all* staging descriptors. For otherwise valid
requests, upload parts rejected by the client-supplied `MAX_FILE_SIZE` or
an empty filename are closed immediately. Only the accepted indices can be
copied using `AdmittedRequest::copy_accepted(index, writer)`.

```rust
let admitted = rustpon::admission::admit(request, rustpon::admission::Policy::default())?;
for part_index in admitted.accepted_indices() {
    // Consume this index via copy_accepted() on a mutable admitted request.
}
```

For a concrete JSON-only inspection of the admitted metadata and indices:

```sh
target/debug/multipart --admit demo "$spool_dir" < request-body.bin
```

The Linux PHP 8.4 multipart SAPI differential suite compares the admitted
metadata tree against the real sanitized `$_FILES` representation. It checks
accepted indices for ordered `MAX_FILE_SIZE` cases and no-file controls.
A process-isolated test also checks the actual open-FD count across repeated
request admission, server rejection, and drop (Linux `/proc/self/fd` and
macOS `/dev/fd`).

**Trust boundary:** The low-level `Request::upload_mut()` remains available
for debugging and wire-level experiments, and bypasses this higher-level
admission gate. Runtime integration must use only `AdmittedRequest` or an
equivalent sealed interface, not the raw parser request. This policy operates
*after* wire parsing; do not rely on it to bound CPU, memory, or disk during
the parse. Configure wire `Limits` and HTTP transport limits at ingress.

This gate is intentionally stricter than PHP's ordinary upload registration:
an over-server-quota upload aborts the request rather than returning a
PHP-native error 1. Metadata `error` and `size` are still testing strings,
and canonical PHP collision/overwrite selection is not a validated physical
file handoff. No Zend FFI or production upload registry is included.


## Canonical FILES-to-FD association

The admission gate now reconciles staged upload descriptors with the **final
PHP-style FILES tree**. Every uploaded part receives a synthetic internal
`tmp_name` identity in a second bounded projection; only identity leaves
surviving PHP's normalized-key, scalar/array replacement, numeric-key and
append semantics are eligible to retain descriptors. All overwritten or
superseded upload FDs are closed before admission returns.

The identity projection is never serialized or exposed to applications.
Selection visits only the generated `tmp_name` columns; arbitrary filenames
and submitted field values cannot forge admission identities. An allocation
or projection failure aborts the entire transaction. This intentionally
performs a second metadata conversion and holds two bounded metadata arenas
transiently; their combined memory use and allocator overhead are not an
RSS guarantee.

`AdmittedRequest::accepted_file(index)` returns a typed Rust view of a
**successful, still-live upload**, with borrowed basename, submitted
`full_path`, content type, signed 64-bit `size`, and signed 64-bit
`error = 0`. It returns `None` for superseded or rejected parts.
The original wire index is preserved; this is not a Zend `zval`, PHP's
registered-upload table, or a stable on-disk pathname.

The PHP 8.4 multipart SAPI suite covers flat duplicates, nested-to-flat and
flat-to-nested replacements, numeric-index replacement, root-name
normalization and repeated `[]` appends. The corresponding Rust tests
verify that rejected indices cannot be copied and their descriptors are
closed. A future SAPI adapter must bind the physical file to the finalized
PHP metadata index, and enforce its own ownership and move/rename policy.


## Typed PHP FILES JSON projection

The multipart `--files` and `--admit` CLI modes now emit both
`files_tree` (legacy byte-string values) and `files_tree_typed`.
The typed output preserves the key type and insertion order, while
encoding `$_FILES` `error` and `size` as signed integer leaves of the
form `["i","3"]`. String leaves continue to use hexadecimal bytes
(`["s","..."]`). Numeric conversion checks that the synthesized leaf
is a valid signed 64-bit integer; invalid conversion returns an I/O
error rather than publishing a forged number.

The Rust API is `PostRequest::write_php_files_json(writer)`, intended
**only** for `files::project` or admitted FILES metadata. It does not
change the internal POST arena, allocate Zend values, or implement a
native PHP `zval` bridge.

The HTTP differential suite now compares both JSON representations
with PHP 8.4's actual integer/string `$_FILES` leaf types, using a
path-sanitizing PHP endpoint. A PHP field named e.g.
`f[tmp_name]` is distinguished from the real `tmp_name` metadata
column by its structural level. Additional regression coverage includes
nested `error`/`size` field names, zero-byte uploads and a Windows
backslash-containing filename. The parser preserves quoted filename
backslashes in accordance with PHP's observed multipart SAPI behavior;
this changes the older strict-parser test expectation for escaped
backslashes in filenames while leaving field-name processing separate.

**Still outstanding:** typed tagged JSON is a reference representation,
not a PHP `zval`; a native PHP extension must implement integer zvals,
real temporary-file registration, ownership transfer and a carefully
bounded C/Rust FFI boundary. The source-level payload quotas and
post-parse admission controls are unchanged.


## Isolated experimental C ABI for URL-encoded POST

The new [`ffi/README.md`](ffi/README.md) describes a separate `rustpon-ffi`
Rust crate exporting a **versioned C ABI** for one-shot URL-encoded
form decoding and PHP-style POST JSON serialization. The main parser crate
continues to forbid `unsafe`; the only raw pointer accesses are audited
and explicitly documented in the FFI crate.

- Header: `ffi/include/rustpon_ffi.h`
- Export: `rustpon_v1_form_post_json(input, len, output, cap, required_len)`
- No Rust-owned output pointer is retained; the caller allocates the buffer.
- Short buffers produce `RUSTPON_BUFFER_TOO_SMALL` and leave output unchanged.
- The bridge has independent 256 KiB input and 1 MiB output ceilings,
  in addition to the core's bounded memory/CPU quotas.
- Errors and ordinary Rust panics are translated to fixed numeric codes.
  Invalid pointers, process aborts and Zend `longjmp` are **not** catchable
  by this bridge.
- Linux CI compiles a **real C caller**, calls the ABI repeatedly and
  compares the library's binary-safe POST JSON against PHP 8.4
  `parse_str()`. Rust tests/formatting/Clippy cover Linux and macOS.

This initial ABI deliberately excludes multipart and uploaded file descriptor
handoff; native PHP `zval` population and `$_FILES` registration require
a separate reviewed C/Zend shim. Do not allow `zend_bailout()` or PHP
callbacks to cross an active Rust FFI frame.
