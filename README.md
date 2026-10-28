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
- Optional upload content type accepts a `token/token` media type followed
  by semicolon parameters; only bytes before the first semicolon are retained,
  matching PHP 8.4 `$_FILES[*]["type"]` (including pre-semicolon spaces).
  Nested multipart types remain unsupported. Invalid/duplicate essential
  headers, folding, control bytes, transfer encodings, and unknown disposition
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
spool directory trusted and stable for the request lifetime. At construction,
the parser resolves the directory to an absolute canonical path, preventing
later working-directory changes or ancestor symlink retargeting from redirecting
uploads. It rejects a symlink at the final directory component and detects
identity changes during canonicalization. Before each upload, the parser
rechecks the resolved directory's original device/inode identity and
private permissions; a symlink swap, a recreated directory, or a permission
relaxation fails closed. A missing directory still follows the opt-in PHP
`UPLOAD_ERR_NO_TMP_DIR=6` policy. After creating a staging file, the parser
also checks that the directory still has its original identity and that the
pathname still names the opened file before unlinking it. After unlink,
it verifies that the opened inode has zero remaining hard links before writing
request bytes. Deterministic fault-injection tests cover directory retargeting,
filename replacement, and hard-link creation at this point. A detected race fails closed without
writing request bytes; an empty orphan may remain in a renamed directory.
These pathname checks do **not** close the final check/unlink race:
descriptor-relative creation and unlinking with an independently audited
OS interface are required before treating a mutable spool path as safe.

## PHP 8.4 URL-encoded separator option

The default URL-encoded decoder splits fields only at raw `&`. PHP's
`arg_separator.input` accepts a set of raw separator bytes, which need not
include `&`. Use `FormParser::with_php_arg_separators(b"|")` or
`urlencoded::parse_reader_with_php_arg_separators(reader, limits, b"|")`
to select a set. The CLI equivalent is `urlencoded --php-arg-separators '|'`.
An empty set follows PHP's fallback to `&`. Separators are recognized
before percent-decoding; `%7C` does not split when `|` is configured.
Even `=` can be a separator, in which case it takes precedence over the
name/value delimiter. The separator lookup uses a fixed-size byte bitmap.

The earlier `--php-semicolon-separator` flag and
`with_php_semicolon_separator()` API remain shorthands for `&;`; the two
CLI separator options are mutually exclusive. The option composes with PHP
input-variable and nesting limits. The differential suite compares custom
separator sets, encoded separators, empty settings and `=` precedence
against the installed PHP CLI `parse_str()` oracle. This is a request
parsing prototype, not a PHP SAPI integration.

## PHP 8.4 percent-decoded NUL names (opt-in)

The strict URL-encoded parser rejects decoded NUL bytes in variable names.
PHP 8.4 `parse_str()` instead truncates a decoded name at its first NUL,
ignoring the field when the resulting name is empty. Use
`FormParser::with_php_nul_name_truncation()` or
`urlencoded::parse_reader_with_php_nul_name_truncation(reader, limits, separators)`
to enable this behavior, or pass `--php-nul-name-truncation` to the
`urlencoded` CLI. This option composes with custom raw separators and
the existing PHP field-count/nesting projections. Value bytes, including
decoded NULs, remain intact. The raw input name/body and retention
budgets are enforced *before* truncation, so an attacker cannot evade
resource limits by placing a NUL near the beginning of a long name.
The PHP CLI differential suite checks the decoded-NUL cases against
PHP 8.4 `parse_str()`. This does not claim raw-NUL HTTP SAPI equivalence.

## PHP max_input_vars and ignored names (opt-in)

PHP 8.4 counts syntactically present URL-encoded pairs with empty decoded
variable names against `max_input_vars`, even though they are omitted from
`$_POST`. For example, at a limit of 2, `a=1&=ignored&b=2` publishes
only `a`. Blank separator-only pairs such as `a=1&&b=2` do not consume
a variable slot. A name truncated to empty by `%00` also consumes a slot.

When `--php-max-input-vars N` is selected, the URL-encoded CLI now preserves
ignored empty-name pairs in the internal wire representation, so the POST
projection counts them in input order before omitting them from the output.
Library callers can opt in with `FormParser::with_php_input_var_counting()`
or `urlencoded::parse_reader_with_php_input_var_counting(reader, limits,
separators, truncate_nul)` before using `into_post_with_max_input_vars()`
or `into_post_with_php_limits()`. Without this parser option, empty-name
pairs are discarded as before; using only the POST adapter cannot recover
their positions. Raw input, part-count and memory quotas continue to apply
to the preserved pairs. The PHP CLI differential suite covers both explicit
empty names and NUL-truncated names, including custom separators.

## PHP-style adapter scope

An opt-in `admission::admit_with_php_post_max_size` adapter models PHP
8.4 `post_max_size` at the metadata publication boundary. When the
complete body exceeds a nonzero limit, it publishes an empty `$_FILES`
projection and immediately releases every staged upload; a zero PHP limit
is unlimited. The CLI oracle is `multipart --admit --php-post-max-size BYTES`.
The CLI now uses `parse_reader_with_php_limits` to enforce this quota
during ingress. Once the body exceeds the nonzero PHP threshold, the parser
and any staged uploads are dropped immediately; the remaining body is drained
without parsing or disk writes. The returned empty wire request retains the
actual byte count for admission and independent hard-quota enforcement.
Zero disables the PHP quota. Malformed framing encountered *before* the
threshold still fails closed, unlike an HTTP SAPI that can preflight a trusted
Content-Length; this is not a Zend `$_POST` implementation or SAPI handler.
The Linux differential checks exact, one-byte-over, one-byte-under, zero,
and multi-buffer upload cases against PHP 8.4's HTTP SAPI.

For transports that have **already validated** an unambiguous Content-Length,
`parse_reader_with_php_content_length` consumes exactly that many bytes.
When the declared length exceeds nonzero PHP `post_max_size`, it preflights
before creating a multipart parser or staging any upload, drains only the
declared body, and publishes empty POST/FILES metadata through admission.
The server's independent hard body quota is checked before any read. A short
body or read error fails closed; bytes beyond Content-Length remain unread for
the transport to handle. The local `multipart --admit --content-length BYTES`
harness exposes this behavior for differential testing. It does **not** parse
HTTP headers, resolve conflicting Content-Length/Transfer-Encoding, or serve
requests, and must never be fed untrusted framing metadata directly.
The HTTP SAPI differential suite exercises both the streaming fallback and
trusted-length preflight against the same PHP 8.4 responses.

An additional opt-in PHP 8.4 nesting slice is exposed by
`Request::into_post_with_php_limits(limits, max_input_vars,
max_input_nesting_level)` and the `urlencoded` CLI flags
`--php-max-input-vars` and `--php-max-input-nesting-level`.
Ordinary fields exceeding the nesting level are discarded while later fields
are retained; discarded fields still count toward `max_input_vars`. The
wire parser's hard limits and FILES upload-name policy remain strict.
Differential tests compare the resulting tagged trees with PHP CLI
`parse_str()` under explicit `-d max_input_nesting_level` and
`-d max_input_vars` settings, including zero, boundary and combined cases.

An opt-in field-count compatibility slice is available through
`Request::into_post_with_max_input_vars(limits, max_input_vars)` and
`urlencoded --php-max-input-vars N`. It retains the first N ordinary
form fields and skips later fields while continuing to process uploads.
`max_input_vars=0` drops all ordinary fields. PHP 8.4 `parse_str()`
is used as the oracle for six exact boundary/collision cases in CI.
The ordinary `into_post` entry point remains fail-closed and does not
apply this PHP limit. An additional opt-in `files::Limits::max_file_uploads`
quota projects only the first N nonempty-filename uploads (while retaining
empty-filename controls until the quota is reached), as observed with PHP
8.4.24's HTTP SAPI. Use `multipart --files --php-max-file-uploads N` or
`multipart --admit --php-max-file-uploads N` for the oracle harness. Uploads
excluded by this quota are dropped at admission, including their staging FDs;
raw wire parsing and its independent strict resource quotas are unchanged.
An independent opt-in `files::Limits::max_upload_bytes` models PHP 8.4
`upload_max_filesize` for metadata and admission. `Some(0)` disables
this PHP quota; positive limits accept the exact byte count and produce
`UPLOAD_ERR_INI_SIZE=1` for oversized files. This error takes precedence
over form-supplied `MAX_FILE_SIZE` (error 2), and rejected uploads lose
their staged descriptors without preventing later files from succeeding.
The CLI exposes `--files/--admit --php-upload-max-filesize BYTES`.
It now passes this quota to `parse_reader_with_php_upload_max_bytes`,
which enforces it while streaming: when an upload exceeds the nonzero
limit, any staged prefix is truncated and subsequent bytes are drained
without disk writes. Later uploads continue. The logical received size
is retained for PHP error=1 metadata; copying a discarded upload fails.
Zero disables the PHP quota. The core `Parser::new` and FFI retain
strict-only behavior unless explicitly configured. Independent wire
limits remain in force. HTTP SAPI differential tests check metadata,
while Rust split-point tests verify staging and descriptor cleanup.
Both CLI quota flags may now be combined in either order for
`--files` and `--admit`; the count limit includes oversized
uploads and overwritten filenames. Seven combined-limit cases
are compared against PHP 8.4 HTTP SAPI in both modes and both
option orders. This is **not yet a full PHP limit policy**:
the wire parser may reject requests before projection (including empty
names or too many parts), and nesting, append exhaustion, allocation and
upload quota errors still use strict rejection. See issue #4.


The adapter preserves ordered fields, applies PHP-style root/bracket
normalization, expands bracket paths, handles numeric string keys and
append-after-negative-index behavior, and models scalar/array replacement.
Upload names protect their canonical and generated `tmp_name` keys from later
fields. A single ASCII whitespace byte inside a bracket index (such as
`a[ ]` or `a[\\t]`) is an append marker, matching PHP 8.4; two whitespace
bytes or whitespace followed by another byte remain part of the literal key.
The PHP upload-name path separately trims leading bracket whitespace when
projecting `$_FILES` (for example, `f[ x]` becomes index `x`).
For ordinary fields, a missing first closing bracket flattens the name
(`a[x` becomes `a_x`); after a completed bracket, an unfinished tail
is ignored (`a[x][y` becomes `a[x]`). This matches PHP 8.4
`php_register_variable_ex` and is checked by the CLI/SAPI differential
corpora. For file controls, malformed bracket syntax (such as `f[x` or
`f[x][`) stops registration of subsequent uploads, matching PHP 8.4
rfc1867 behavior; earlier valid files and later ordinary form fields remain
visible. Bracket-only upload roots are ignored without stopping later
files; an empty name is still rejected by the strict wire parser. The
admission gate closes descriptors for every discarded file.
This is distinct from strict resource-limit failures, which still reject the
whole request.

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
returns an error and drops *all* staging descriptors. `Policy::max_total_file_bytes`
adds a server-controlled aggregate staged-upload budget alongside the
per-upload limit. It counts every uploaded payload, including those later
rejected by `MAX_FILE_SIZE` or overwritten by PHP-style key collisions.
The exact limit is inclusive; a one-byte overage fails admission before
metadata is exposed. This is an admission-time check, **not** a bound on
parsing-time disk use: configure `Limits::file_bytes_total` at ingress too. For otherwise valid
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


## Multipart C ABI with admitted upload copying

The experimental `ffi/` crate additionally exports
`rustpon_v1_multipart_files_json` and `rustpon_v1_multipart_upload_copy`.
These one-shot calls reparse the complete multipart request, use a trusted
0700 spool directory, pass through the *consuming* Admission gate and return
either the typed PHP-shaped FILES JSON tree or the raw bytes of a specific
accepted upload. The original zero-based multipart wire part index selects
the upload, not a normalized PHP array index. Rejected and superseded
uploads cannot be copied. Zero-byte admitted files are supported.

The C caller owns all buffers; Rust-owned staging descriptors are always
closed before returning. Querying size and copying require **two full
parses**, not a persistent session, and there is no OS FD handoff or
Zend/SAPI upload registration. Limits: 256 KiB body per call, 70-byte
boundary, UTF-8 spool path length up to 4096 bytes and 1 MiB output.

Linux CI runs a compiled C smoke consumer and a real PHP 8.4 multipart
SAPI differential comparison (78 metadata cases, 36 payload
copies/rejections), checking staging cleanup. See
[`ffi/README.md`](ffi/README.md) for the pointer safety contract and
remaining PHP integration requirements.


## Bounded, single-parse multipart sessions

The experimental FFI also supports a **stateful** multipart lifecycle:
`rustpon_v1_multipart_session_open` returns a process-local 64-bit
session ID. `session_files_json` and `session_upload_copy` access the
same admitted parser result without reparsing. `session_close` destroys
the request and closes all remaining anonymous upload FDs.

The registry permits at most eight live requests per process, serializes
operations under a mutex and rejects stale handles. Session IDs are **not
secrets**, and no native Rust pointers are passed to C. A successful
open must be closed on every application path. **Production PHP-FPM
integration requires mandatory RSHUTDOWN cleanup**; a Zend `longjmp` may
skip normal stack-based cleanup. A poisoned registry fails closed but
can retain OS resources until the worker terminates.

Linux CI compares **78** stateful typed FILES projections and **36**
stateful upload copies/rejections with PHP 8.4 SAPI. A real C caller
verifies the eight-handle cap, invalid/duplicate close rejection,
and OS-level FD counts returning to their baseline on close.

The optional native PHP 8.4 lifecycle probe is now implemented,
but it is not a production SAPI parser or complete Zend upload-registry
integration. See [`ffi/README.md`](ffi/README.md).


## Native PHP 8.4 request-shutdown lifecycle probe

The experimental [PHP module](ffi/php_probe/README.md) now builds against
PHP 8.4 headers and runs inside **actual PHP RINIT/RSHUTDOWN hooks**.
The module begins one scoped Rust handle group on every PHP request,
then closes all its admitted multipart sessions at RSHUTDOWN. It never
exposes Rust handles or lets PHP userland select the upload spool path.

The CI suite reuses **one PHP CLI HTTP server process for 32 requests**,
including normal return, `exit`, multiple opens and an unhandled
exception. After each HTTP request, the test checks the OS-level file
descriptor count returned to its initial value. The C ABI suite also
simulates 48 C-only bailout paths and verifies unrelated scopes survive
bulk cleanup.

**Scope:** This is a research/debug extension, not a replacement PHP
request handler. It does not produce native `$_POST` or `$_FILES` Zend
arrays, register/move uploads, or exercise an actual PHP-FPM FastCGI worker.
The follow-up production gates remain PHP-FPM lifecycle tests, protocol
integration, concurrency and resource fault campaigns, and independent
security review.


## Bounded scalar variables in the research PHP evaluator

The `php_eval` binary now accepts up to 64 assignments to at most 32
case-sensitive, ASCII-named local integer/boolean variables before its one final
`echo` statement. Expressions can read previously initialized variables,
use integer unary `+`/`-`, parentheses and `+`/`-`/`*`, and reassign
existing variables. Each name is capped at 64 bytes, while the existing source,
AST-depth-equivalent recursion, and work budgets remain active. Unknown
variables, unsupported expressions and resource exhaustion fail closed without
producing output. The PHP special variable `$GLOBALS` is intentionally rejected.

```sh
printf '%s' '<?php $x=7; $x=$x+2; echo $x*3;' |
  cargo run --offline --locked --bin php_eval
# stdout: 27
```

`tools/differential_php_eval.py` compares deterministic single-expression
and variable-assignment programs against actual PHP CLI. This **does not**
provide general PHP lexing/AST, PHP scope or reference semantics, dynamic
types, superglobals, arbitrary statements, or Zend/SAPI execution. The
interpreter has no file/network access and is not connected to the FFI layer.


### PHP 8.4 statement-level increment/decrement (research)

The bounded `php_eval` interpreter accepts prefix `++$x;`/`--$x;` and
postfix `$x++;`/`$x--;` as standalone statements for initialized scalar
variables. Integer values change by one; PHP 8.4 leaves booleans unchanged,
converts incremented null to integer one, and leaves decremented null as null.
The usual assignment-count, work and variable limits apply. Overflow that
PHP promotes to float, undefined-variable warning paths, and increment or
decrement *expressions* are deliberately rejected without output.

```sh
printf '%s' '<?php $x=null; ++$x; $x++; echo $x;' |
  cargo run --offline --locked --bin php_eval
# stdout: 2
```

The differential suite compares both token placements, mixed scalar tags,
boundary values, repeated updates, and malformed syntax with PHP 8.4 CLI.

### PHP 8.4 scalar compound assignments (research)

The standalone `php_eval` interpreter accepts `$x += expression;`,
`$x -= expression;`, `$x *= expression;`, `$x /= expression;`,
and `$x %= expression;` for previously defined integer, boolean, or null
variables. RHS expressions use the existing PHP precedence and signed-64-bit
arithmetic. All five operators store an integer result for the supported
subset; the usual assignment-count, work and variable limits remain in force.
The operator and `=` must be adjacent. Multiplication is checked before
publication, including `PHP_INT_MIN * -1`; overflowing results that PHP
promotes to float are rejected. Division assignment shares the exact-quotient
implementation with binary `/`: non-integral results and
`PHP_INT_MIN / -1` (which PHP promotes to float) are rejected. A zero
divisor fails closed. Modulo follows PHP's signed remainder rules, including
`PHP_INT_MIN % -1 == 0`, and rejects a zero divisor without publishing a
partial result.

```sh
printf '%s' '<?php $x=true; $x+=4; $x-=2; echo $x;' |
  cargo run --offline --locked --bin php_eval
# stdout: 3
```

PHP's undefined-variable warning path and overflow promotion to floating
point are deliberately unsupported and rejected without output. The PHP
8.4 CLI differential suite covers deterministic, generated, boolean/null,
boundary and malformed-assignment cases, including division/modulo-by-zero,
non-integral division, and signed division overflow rejection. This is not a
general PHP assignment engine or a Zend/SAPI integration.

### PHP 8.4 `unset()` local-variable lifecycle (research)

The bounded `php_eval` interpreter accepts case-insensitive
`unset($name)`, `unset($a,$b,)`, and interleaved assignments before the
final `echo`. Removing a defined variable makes subsequent `isset()`
false and `empty()` true; removing an undefined variable succeeds. A
subsequent assignment can reuse the freed binding slot, while the cumulative
64-assignment, source-byte and work quotas still apply. Only simple ASCII
local variables are supported; array elements, object properties, references
and `$GLOBALS` are rejected. Parsing remains fail-closed, without partial
output or a Zend symbol table.

```sh
printf '%s' '<?php $x=7; unset($x); echo isset($x);' |
  cargo run --offline --locked --bin php_eval
# stdout: empty (PHP false)
```

The PHP CLI differential suite compares missing, existing, redefined and
multiple-variable unsets, including generated programs and malformed input.
This does not model PHP destructors, references or request-scoped superglobals.

## PHP 8.4 integer literal lexical compatibility

The research-only `php_eval` interpreter recognizes bounded PHP 8.4
integer literal forms: decimal, legacy leading-zero octal, explicit `0o`
octal, `0b` binary and `0x` hexadecimal (including uppercase prefixes).
Underscores are accepted **only between digits valid for that radix**.
Values are checked against signed 64-bit integer bounds before any output.
Invalid digits, dangling separators and missing digits are rejected.

The differential CLI corpus includes fixed radix edge cases and 192
deterministically generated radix expressions/assignments, checked against
the installed PHP 8.4 CLI. PHP itself promotes overflowing integer literals
to floating-point; this bounded integer-only prototype intentionally rejects
those cases instead of emulating float promotion. No Zend numeric conversion
or dynamic typing is implied.


## Exact integer division and PHP signed remainder (research evaluator)

The bounded `php_eval` grammar now supports `/` and `%` at the same
precedence and left associativity as `*`. Exact signed integer division
matches PHP 8.4's integer result. Fractional division and integer division
overflow (which PHP promotes to floating point) are **explicitly rejected**
rather than rounded or misrepresented. Division and remainder by zero fail
closed; the PHP-specific `PHP_INT_MIN % -1 == 0` case is handled without
triggering Rust's signed remainder overflow. For other negative operands,
the remainder follows the sign of the dividend, as in PHP.

The PHP CLI oracle exercises deterministic positive/negative quotient and
remainder cases, including the minimum signed 64-bit integer boundary, plus
fractional/zero-divisor rejections. These tests do not implement PHP floating
point, exceptions, typed zvals or full PHP runtime error reporting.

## Bounded PHP 8.4 integer comparisons in final echo

The research-only `php_eval` command supports one final integer comparison
using `<`, `<=`, `>`, `>=`, `==`, `!=`, `<>`, `===`, or `!==`.
Both operands use the existing exact, checked 64-bit integer arithmetic grammar,
including previously assigned integer variables. Integer operands make PHP loose
and strict equality equivalent within **this deliberately narrow subset**.
Comparison has lower precedence than arithmetic. Echo renders true as `1`
and false as **empty stdout**, matching PHP CLI rather than emitting `0`.

```sh
printf '%s' '<?php $x=3; $y=5; echo $x * 2 >= $y;' |
  cargo run --offline --locked --bin php_eval
# stdout: 1
```

Comparisons inside parentheses and assignments are now supported for integer and
boolean values. Comparison chains, strings, floats, general dynamic type
coercion and control-flow conditions remain *unsupported*.
These forms fail closed without output. PHP CLI differential tests cover
both truth values, precedence, signed integer boundaries and deterministic
generated expressions. No interpreter is connected to PHP-FPM or production
Zend/SAPI request handling.

## PHP 8.4 integer exponentiation (research evaluator)

The bounded `php_eval` interpreter now implements `**` for **exact signed
64-bit integer results**, using checked exponentiation without floating-point
fallback. As in PHP 8.4, `**` binds more tightly than unary signs and is
right-associative: `-2**2` is `-4`, while `2**3**2` is `512`.
It also works with the existing variables, arithmetic and final-echo integer
comparisons. The implementation charges the existing work/depth budgets and
uses logarithmic-time checked integer exponentiation rather than unbounded
multiplication loops.

PHP promotes some powers to floating point (for example `2**63` and
`2**-1`). This prototype **rejects** those cases, along with exponents
outside its bounded unsigned 32-bit range, without producing partial output.
The PHP 8.4 CLI differential suite adds fixed precedence/boundary examples
and 192 deterministic generated exact-power programs, plus invalid-input
regressions. This remains a research-only interpreter, not a PHP VM or
production PHP-FPM integration.

## Typed integer and boolean values (research evaluator)

The bounded `php_eval` interpreter stores `Value::Int(i64)` and
`Value::Bool(bool)` in local variables and expression results. Case-insensitive
`true` and `false` literals, comparisons in assignments and parentheses,
and boolean-to-integer coercion in existing arithmetic are supported.
PHP 8.4 truthiness governs loose equality and ordering with a boolean;
strict `===` / `!==` preserve types. Echo emits `1` for true,
**no bytes** for false, and `0` for integer zero.

```sh
printf '%s' '<?php $flag=3<4; echo ($flag+2)===3;' |
  cargo run --offline --locked --bin php_eval
# stdout: 1
```

The PHP CLI oracle adds deterministic boolean assignment, comparison,
coercion and nested-expression cases plus malformed-token rejections.
Chained comparisons, binary logical operators, strings, floats,
arrays, references, control flow and unbounded work remain unsupported. This
is not a full Zend value model, AST, PHP VM or production PHP-FPM integration.

### PHP 8.4 `isset()` probes and scalar-expression `empty()` (research)

The bounded evaluator recognizes case-insensitive `isset($name)` and
`empty($name)` without raising an unknown-variable error. `isset` returns
false for undefined or null variables and accepts multiple simple variables
with an optional trailing comma. `empty` additionally evaluates supported
integer/boolean/null expressions, including arithmetic, comparisons, nested
builtins and parentheses, then applies PHP truthiness. A bare undefined
variable remains a notice-free probe. The original value tags are retained,
and expression parsing obeys existing source, work and recursion limits.

The PHP 8.4 CLI differential suite compares defined/undefined variables,
scalar expressions, nested probes and 256 generated expression programs.
Malformed argument lists and unsupported forms are rejected without output.
`isset()` still accepts only simple ASCII `$identifier` operands. Array
offsets, properties, references and the full symbol table remain unsupported;
`empty($missing + 1)` is rejected because general undefined-variable
expression semantics are not modeled. This is not a general PHP language
parser or production runtime.

### PHP 8.4 null literal and scalar predicates (research)

The bounded interpreter now represents PHP `null` as a distinct
`Value::Null` tag. Case-insensitive `null` literals, assignments, echo
(empty output), arithmetic and bitwise binary integer coercion (zero),
truthiness (false), loose versus strict comparisons, and `min()`/`max()`
selection are compared with the installed PHP 8.4 CLI. For comparisons
against null, PHP compares truthiness: `null <=> -1` is `-1`, while
`null <=> 0` is `0`. The unary bitwise `~null` raises a PHP
`TypeError`; the prototype returns its bounded `BitwiseTypeError`
without publishing output. `is_null()`, `is_scalar()`, and
`is_numeric()` inspect the original tag; integers are numeric,
booleans are scalar but not numeric, and null is neither. The differential
suite adds fixed boundary/exception cases and seeded generated comparisons.
This is limited scalar modeling, not Zend's full zval, nullable type
declarations, null-safe operators, or a production runtime.

### PHP 8.4 unary logical negation

The bounded scalar evaluator accepts unary `!` on its existing integer and
boolean values, returning a boolean using PHP truthiness (`0` is false;
nonzero integers are true). Repeated negation, variables, parentheses,
comparisons and integer exponentiation use the existing bounded recursion
and work budgets. As in PHP 8.4, `!2**2` negates the power result, while
`!false+2` evaluates to integer `3`. The CLI differential suite checks
59 additional fixed and generated programs against PHP 8.4 and rejects
malformed negations without publishing output. Short-circuit `&&` / `||`
and other unsupported PHP operators remain unavailable.

### PHP 8.4 integer bitwise negation

The research `php_eval` evaluator also accepts unary `~` on **integer**
operands. It uses signed 64-bit bitwise complement, including `PHP_INT_MIN`
and `PHP_INT_MAX`, with the same unary/power precedence as PHP 8.4.
For example, `~2**3` yields `-9`, and `~~0` yields `0`.
PHP 8.4 throws `TypeError` for `~true` and other boolean operands;
this prototype rejects them with a bounded `BitwiseTypeError` instead of
pretending to implement Zend exception objects. No partial stdout is emitted.
The CLI differential suite checks 85 additional fixed/generated programs
and verifies five boolean TypeError cases against PHP CLI.

### PHP 8.4 binary integer bitwise operators (research)

The bounded `php_eval` interpreter now supports binary `&`, `^`, and `|`
over signed 64-bit integer values. PHP 8.4 coerces boolean operands to
integers for these binary operators, unlike unary `~` (which raises
`TypeError` for booleans). Operator precedence follows PHP 8.4:
comparisons bind tighter than `&`, followed by `^`, then `|`.
Parenthesized expressions and existing bounded variable assignments
use the same evaluation rules. For example, `1 & 3 == 1` evaluates
to integer `0`, while `(1 & 3) === 1` evaluates to boolean `true`.
The prototype still rejects unsupported `&&` and `||` syntax, missing
operands, and unknown variables without publishing partial stdout.

The PHP 8.4 CLI differential suite compares fixed precedence and
boolean-coercion examples plus deterministic generated cases. This is
not a general PHP language implementation or Zend/SAPI integration.

### PHP 8.4 bitwise compound assignments (research)

The bounded `php_eval` evaluator now supports `&=`, `^=` and `|=`
for previously defined integer, boolean and null variables. Both operands
are converted to signed 64-bit integers and the assigned value is tagged
as an integer, matching PHP 8.4 for these scalar types. The complete
right-hand expression is evaluated before the assignment; for example,
`$x=1; $x&=2|4; echo $x;` emits `0`. Negative operands and
`PHP_INT_MIN`/`PHP_INT_MAX` use fixed-width bitwise operations.
Missing variables, malformed operator tokens, unsupported logical
operators, and exhausted source/work/assignment budgets fail closed.
The PHP 8.4 CLI oracle includes fixed type/boundary cases and 96
seeded generated compound-assignment programs. Strings, arrays,
references and PHP's full compound-assignment semantics remain unsupported.

### PHP 8.4 integer shifts (research)

The bounded `php_eval` interpreter supports integer `<<` and `>>`
with PHP 8.4 precedence (addition before shifts, shifts before comparison).
Boolean operands are converted to integers. Shift counts of 64 or more
saturate: left shifts return zero, while right shifts return zero or
minus one according to the sign. Left shifts use signed 64-bit bit patterns,
including `1 << 63 == PHP_INT_MIN`; right shifts are arithmetic.
Negative counts fail closed as `NegativeShift` rather than constructing
a Zend `ArithmeticError`. The same checked shift implementation now handles
`$x <<= expression;` and `$x >>= expression;` for previously defined
integer, boolean and null variables. The right-hand expression is evaluated
before assignment; the result is always tagged as an integer. The CLI oracle
compares fixed edge cases and 96 seeded compound-shift programs, including
signed 64-bit boundaries, saturated counts, type coercion, and negative-count
exception categories, against PHP 8.4. Unknown variables, malformed compound
tokens and exhausted assignment/work budgets fail closed.
This remains a deliberately limited evaluator, not a PHP VM.

### PHP 8.4 integer exponentiation assignment (research)

The bounded `php_eval` interpreter supports `$x **= expression;` for
previously initialized integer, boolean and null variables. It shares the
checked exponentiation implementation with binary `**`, including PHP 8.4
right associativity and expression precedence. For example,
`$x=2; $x**=3**2; echo $x;` emits `512`. The stored result is an integer.
Negative exponents, floating-point promotion on overflow, missing variables
and malformed tokens fail closed; no floating-point behavior is claimed.
The PHP CLI oracle covers fixed type/boundary cases and 64 seeded exact
integer exponentiation assignments. The usual source, work and assignment
budgets still apply.

### PHP 8.4 `intdiv()` builtin (research)

The bounded `php_eval` interpreter now recognizes case-insensitive
`intdiv(numerator, denominator)` with exactly two integer/boolean expression
arguments, including nested calls and variables. PHP 8.4 truncates signed
integer division toward zero, unlike the `/` operator, which can produce a
float. For example, `intdiv(-7, 3)` returns `-2`; `intdiv(7, 3)`
returns `2`. The interpreter checks the zero divisor and the signed
`PHP_INT_MIN / -1` overflow separately, returning bounded errors without
publishing partial output. It does not instantiate Zend exceptions.
Malformed calls, missing arguments, unsupported names and resource-limit
exhaustion fail closed. The PHP 8.4 CLI differential suite compares fixed and
seeded generated calls and verifies `DivisionByZeroError` and
`ArithmeticError` on the reference runtime. Other builtins and arbitrary
function definitions/calls remain unsupported.

### PHP 8.4 scalar type predicates (research evaluator)

The bounded `php_eval` evaluator implements case-insensitive `is_int()`
(and its PHP aliases `is_integer()`, `is_long()`), `is_bool()`, `is_null()`,
`is_scalar()`, and `is_numeric()` for its supported scalar types.
Each takes exactly one supported integer/boolean/null expression, inspects the
actual `Value` tag without coercing it, and returns a PHP boolean.
For example, `is_int(true)` is false, `is_int(true+0)` is true, and
`is_bool(is_int(1))` is true. Existing source/work/depth limits apply to
nested calls. Missing/extra arguments, unknown functions and unknown
variables fail closed without output. The PHP 8.4 CLI differential suite
checks fixed type boundaries, nested expressions and seeded values.
Strings, floats, arrays, references, user functions and general Zend type
semantics remain outside this research-only subset. In particular,
`is_numeric()` recognizes supported integers but has no numeric-string or
floating-point support; `is_scalar(null)` is false.

### PHP 8.4 scalar `(int)` and `(bool)` casts (research)

The bounded `php_eval` interpreter now recognizes PHP 8.4's
case-insensitive `(int)` / `(integer)` and `(bool)` / `(boolean)` casts,
including whitespace inside the cast parentheses, nesting, and casts of
variables and expressions. Integer casts preserve the signed 64-bit range;
boolean casts use PHP truthiness and preserve a boolean type tag. Casts
share the existing unary precedence, depth/work quotas and fail-closed
output policy. For example, `(int)-2**2` yields `-4`, while
`is_bool((bool)2)` yields `true`. The PHP CLI differential corpus
covers case variants, boundary values, precedence, nested casts and
invalid/unsupported syntax. String, float, array and object casts are
not implemented; this is not a general PHP runtime.

### PHP 8.4 explicit `boolval()` and `intval()` (research)

The bounded `php_eval` evaluator supports case-insensitive
`boolval(value)` and `intval(value)` for its existing signed 64-bit
integer, boolean and null values. `boolval()` uses PHP truthiness (only zero is
false among supported integers); `intval()` converts booleans to 0 or 1
and preserves the full signed 64-bit integer range; null converts to zero.
Both return the
appropriate runtime value tag, so `is_bool(boolval(1))` and
`is_int(intval(true))` are true. Nested calls, variable assignments,
source/work/depth quotas, and fail-closed rejection of missing arguments,
unknown variables, and unsupported function names are retained.

PHP 8.4 also accepts `intval(value, base)` for integer/boolean values.
The optional base expression is fully evaluated and charged to the usual
source/work/depth budgets, but PHP ignores its numeric value for these input
types (including bases outside 2–36). Malformed, missing and third arguments
fail closed. String parsing with a numeric base, floats, named arguments and
other PHP value types remain unsupported. The deterministic PHP 8.4 CLI
differential suite compares fixed boundaries, nested/invalid calls and
generated scalar/base combinations. No Zend function dispatch or native PHP
runtime integration is implied.

### PHP 8.4 `abs()` builtin (research)

The bounded `php_eval` interpreter supports case-insensitive `abs(value)`
for integer and boolean expressions. Boolean operands convert to integers;
negative integers use checked absolute value, and nested `abs()`/`intdiv()`
calls share the existing expression depth and work budgets. On PHP 8.4,
`abs(PHP_INT_MIN)` promotes to a floating-point value. This integer-only
prototype rejects that case as `UnsupportedFloat` before writing output.
Missing or extra arguments, unknown functions and variables are rejected.
The PHP CLI differential suite adds fixed and seeded generated exact-integer
cases, plus malformed-call and minimum-integer rejection tests. No Zend
function dispatch or float support is implied.

### PHP 8.4 scalar `min()` and `max()` (research)

The bounded `php_eval` evaluator now supports case-insensitive `min()`
and `max()` with **two or more** integer/boolean expressions. The
winner retains its original value type. PHP 8.4's direct two-argument
`min()` fast path selects the **second** operand on a loose-comparison tie:
`min(true, 2)` returns integer `2`, while `min(2, true)` returns boolean
`true`. Three-or-more-argument `min()` and all `max()` calls retain the
first operand on ties. Dynamic function calls are outside this prototype. Signed 64-bit
extremes, nested functions, assignments and type predicates use the
existing depth and work quotas; each argument also consumes work.
Missing arguments, malformed lists and unknown variables fail closed.

PHP also accepts a single array argument to these functions. Arrays,
strings, floats and that overload are outside this prototype; no
Zend function dispatcher or exception objects are implemented. The
PHP 8.4 CLI differential suite compares fixed type-tie/boundary cases
and 384 seeded scalar-program checks against the reference runtime.

### PHP 8.4 spaceship comparison

The research `php_eval` binary supports `<=>` for its bounded integer and
boolean values. It returns a **typed integer** `-1`, `0` or `1`, using
PHP's boolean truthiness rules when either operand is boolean. The operator
shares the existing comparison precedence; parenthesized results can be
used in arithmetic or strict comparisons. Chained comparisons and unsupported
types remain rejected. The PHP 8.4 CLI differential suite includes fixed
boundary/type cases and deterministic generated comparisons. This adds no
Zend runtime integration or general PHP expression support.

## Bounded Interrupted I/O retries and deterministic parser fault campaign

Both blocking `parse_reader` adapters (multipart and URL-encoded) now
permit **at most 1024 consecutive** `io::ErrorKind::Interrupted` reads.
A successful nonempty read resets the counter. A permanently interrupted
reader returns `Error::Io(Interrupted)` instead of spinning forever.
On multipart failure, staged anonymous uploads are dropped with the parser;
no request or partially projected POST/FILES tree is published.

Regression tests exercise recoverable interruptions between tiny reads,
permanent interruptions after a file has been staged, and fresh-request
isolation. URL-encoded tests also run **256 deterministic mutated or arbitrary
byte inputs** through one-shot and 1/3/17-byte chunked feeds, asserting
identical successful field bytes or identical errors. The existing multipart
mutation corpus and FD lifecycle tests remain active. The PHP 8.4
`parse_str()` differential corpus adds eight targeted percent-decoding,
encoded-delimiter and duplicate-key cases.

This is deterministic fault injection, **not** continuous coverage-guided
fuzzing, a sanitizer campaign, or evidence of PHP-FPM production readiness.
The retry bound applies to these synchronous adapters, not to arbitrary
external transports or native Zend callbacks.

## Bounded upload-copy I/O and PHP SAPI byte oracle

`Upload::copy_to` now copies exactly the staged upload length using an 8192-byte
buffer, returning `UnexpectedEof` if a staged descriptor is unexpectedly
short. Both source reads and destination writes have a limit of 1024
**consecutive** `Interrupted` errors, reset after forward progress.
The shared staged-file write path now also bounds consecutive interrupted
writes and rejects `WriteZero`. These limits prevent unbounded retry loops
in synchronous adapters, without adding dependencies or exposing descriptors.

Fault-injection tests exercise 1023 recoverable interruptions, 1024
consecutive interruptions, zero-byte writes, short writes, injected
`StorageFull` after a partial copy, and retry from offset zero. On a failed
copy the caller **must discard any partially written destination**; the
admitted request still owns its source descriptor. The PHP 8.4 SAPI/FFI
differential suite includes eight additional byte-for-byte upload cases
at zero, one and 8192-byte buffer boundaries, including NUL and high bytes.

This does not make arbitrary external writers transactional or guarantee
successful upload persistence; the caller remains responsible for destination
cleanup and for request-scope resource release. Native PHP-FPM deployment
and independent security review remain outstanding.

## Fail-closed handling of invalid custom I/O byte counts

The standalone multipart and URL-encoded `parse_reader` adapters now reject
a custom `Read` implementation that reports more bytes than the provided
buffer capacity, returning `Io(InvalidData)` instead of indexing out of
bounds. The shared upload-copy `write_chunk` path similarly rejects a
`Write` implementation that reports more bytes written than supplied.
These are defensive checks for **violations of Rust's standard I/O trait
contracts**, not normal PHP HTTP input cases. A failed multipart parse drops
staged upload descriptors; a failed upload copy may leave a prefix in the
caller-owned destination, which the caller must discard. The original upload
can be copied again from offset zero.

Deterministic fault-injection tests exercise both invalid reader counts,
invalid writer counts, exact retry bytes, and process-level file-descriptor
cleanup after an upload has already been staged. Existing PHP 8.4 CLI/SAPI
differential suites remain the compatibility oracle for valid requests.
This does not constitute coverage-guided fuzzing or native PHP-FPM readiness.

## Coverage-guided parser fuzzing (research)

The separate `fuzz/` workspace adds bounded libFuzzer targets for multipart
and URL-encoded input. It checks chunk-invariant accept/reject decisions,
ordered bytes, metadata, upload copies and staging cleanup. Linux CI builds
and smoke-tests the harness; the PHP 8.4 CLI/SAPI oracle still runs separately.
No continuous fuzz campaign or PHP-FPM production readiness is claimed.
See [fuzz/README.md](fuzz/README.md) for usage and dependency licenses.

## Bounded fuzz mutations and upload-copy ENOSPC fault matrix

Linux CI now executes a short, reproducible libFuzzer mutation campaign
(256 executions per target, 20-second cap) for both request parsers in
addition to compiling the targets and running fixed seeds. The multipart
harness injects destination `StorageFull` at four offsets for every
accepted nonempty file, checks the partial output and retries the source
copy byte-for-byte. This expands fault injection across generated multipart
inputs; it is **not** a long-running sanitizer campaign or production PHP-FPM
validation. See [fuzz/README.md](fuzz/README.md).

## Admission-boundary fuzzing and PHP SAPI collision oracle

The isolated `fuzz/admission` target now exercises the **consuming**
`admission::admit` boundary, independently comparing typed FILES projections
and admitted payload bytes against the parsed wire request. It verifies
overwritten/rejected upload indices are inaccessible, then injects server
body-size, upload-size and metadata-quota denials with per-request staging
cleanup checks. The Linux fuzz smoke campaign runs 256 additional mutations
without adding a runtime dependency. Three new PHP 8.4 SAPI differential
cases cover client MAX_FILE_SIZE changes followed by duplicate filenames,
zero-length overwritten uploads and zero/binary array uploads.

This is a bounded research harness, not a guarantee of memory safety under
arbitrary I/O failures, exhaustive fuzz coverage, or a production Zend/FPM
integration.

## SanitizerCoverage + AddressSanitizer fuzz CI

A dedicated Linux CI job now runs all three isolated request-parser fuzz
targets using pinned `cargo-fuzz 0.13.2`, nightly Rust, libFuzzer
SanitizerCoverage feedback and AddressSanitizer. Each target has a
deterministic 256-execution/30-second budget. Crash inputs are uploaded
as short-retention CI artifacts for local minimization and checked-in
regression tests; no application secrets belong in a fuzz corpus.
The stable Linux/macOS test matrix and real PHP 8.4 CLI/SAPI oracles remain
unchanged. This is a bounded fuzz smoke campaign, not long-duration coverage
evidence, a CVE fix, or authorization for native PHP-FPM deployment.
See [fuzz/README.md](fuzz/README.md).

## Cancellation lifecycle fuzzing

The isolated `fuzz/lifecycle` target exercises URL-encoded and multipart
request cancellation at five transport cut points. A cancelled request
must reject subsequent input, a failed parser must preserve its error,
and an unrelated fresh request must produce the same bytes and metadata.
The multipart harness checks private spool cleanup after each cancellation.
Two synthetic seeds and deterministic unit regressions cover accepted,
truncated and binary-upload requests. Linux CI runs a bounded 256-input
SanitizerCoverage/AddressSanitizer campaign for this fourth target.
These are parser-level lifecycle invariants, not PHP-FPM shutdown tests.

### Known PHP 8.4 multipart delimiter ambiguity

A differential probe found that PHP 8.4.27's built-in SAPI truncates an
uploaded file at `\\r\\n--rustpon-diffX` when the declared boundary is
`rustpon-diff`, even though the suffix `X` is neither CRLF nor `--`.
The strict Rust parser retains those bytes as payload. This malformed
boundary-prefix ambiguity is intentionally **excluded from the compatible
corpus** and remains a documented behavioral difference. Do not substitute
the parser for PHP's production SAPI without an explicit ingress policy and
review of this discrepancy. A separate near-match with an incorrect boundary
byte (`rustpon-difx`) remains in the live PHP SAPI differential suite.

## PHP 8.4 upload Content-Type parameters (research)

The multipart wire parser now accepts parameterized part Content-Type headers
such as `text/plain; charset=UTF-8`. PHP 8.4's SAPI registers only the
media-type bytes preceding the first semicolon as the upload `type` field,
including whitespace immediately before that semicolon. The Rust wire IR
preserves those bytes; the existing FILES projection and admission gate
inherit the same value. It does not decode charset parameters or trust the
client's claimed MIME type. Invalid media types and nested multipart remain
unsupported.

The Linux PHP 8.4 SAPI differential suite now checks four additional
parameterized/mixed-case upload types; a Rust unit test also checks the
binary payload length, metadata and nested-multipart rejection. The oracle
checks upload metadata, not MIME sniffing or native Zend integration.

## Opt-in PHP 8.4 partial upload EOF compatibility

The `parse_reader_with_php_partial` API and `multipart --admit --php-partial-upload`
CLI mode preserve completed multipart parts if EOF occurs within a file body
whose headers were already parsed. The opt-in mode also recognizes EOF
immediately after a complete file Content-Disposition header line (before the
blank line): PHP 8.4 registers that file as partial, while an unterminated
header line remains a strict truncation error. The unfinished file is registered
with `UPLOAD_ERR_PARTIAL=3`, zero reported size, empty `type` and `tmp_name`.
Staged partial bytes are erased and cannot be copied through admission.
PHP's file size errors (1/2) and empty-filename error (4) take precedence.
The ordinary parser, trusted Content-Length path, and other incomplete
states remain fail-closed. A transport short read against declared
Content-Length must never be treated as an eligible partial upload.
The Linux differential suite compares this narrow behavior against PHP 8.4
HTTP SAPI; this is not an HTTP framing implementation or PHP-FPM replacement.

## PHP-mode staging failures (experimental)

The PHP-oriented reader functions enable recoverable upload **write** errors:
if writing an anonymous staged file fails, the affected upload is truncated,
marked non-copyable, and projected with PHP 8.4 `UPLOAD_ERR_CANT_WRITE` (7).
They also recover a **missing temporary directory** (`NotFound` when opening
an anonymous staging file) as `UPLOAD_ERR_NO_TMP_DIR` (6), without creating
a placeholder descriptor. Its received payload is counted against hard quotas
but never copied or admitted; later fields and uploads remain parseable.
Empty filenames retain `UPLOAD_ERR_NO_FILE` (4). PHP 8.4 checks temporary-file
creation before upload payload size, so error 6 takes precedence over
`upload_max_filesize` error 1 when the directory is missing.

The strict `Parser::new` mode still aborts and discards the whole request
on I/O failure. Other file-creation failures, unlink failures, and failures
to truncate a staged file remain fail-closed. Tests cover injected creation
failures, a real removed directory, subsequent successful uploads, and
descriptor isolation. A Linux HTTP SAPI oracle independently checks the
PHP 8.4 error-6 metadata and its precedence; write-error 7 remains covered
by deterministic fault injection rather than a live SAPI fault oracle.
This is a Rust-only compatibility model, not native PHP SAPI integration.
