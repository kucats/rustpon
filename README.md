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
- Only `Content-Disposition: form-data` with a nonempty final `name` and
  optional `filename` is supported. Header and parameter names ignore ASCII
  case; quoted parameter escapes are decoded only for \" and \\,
  while other backslashes remain literal. Unquoted `name` and `filename`
  values retain PHP-compatible non-token bytes such as `=`, `:`, backslash
  and UTF-8, stopping at horizontal whitespace or `;`. PHP 8.4 also ignores
  trailing text following an unquoted value's whitespace terminator or a
  quoted value's closing quote up to the next semicolon; later parameters
  (including duplicate `name`/`filename` values) still take effect.
  Like PHP 8.4 HTTP SAPI,
  repeated `name` or
  `filename` parameters use the last value. Well-formed unknown parameters
  (including `filename*`) are ignored rather than interpreted; empty semicolon
  separators are accepted. Raw names, filenames, binary values, duplicates,
  and part order are retained.
- Optional upload content type is opaque client-provided metadata: PHP 8.4
  accepts empty values, unusual labels without a slash and non-token bytes.
  Only the bytes before the first semicolon reach `$_FILES[*]["type"]`.
  Leading horizontal whitespace is stripped, but trailing spaces/tabs (also
  before a semicolon) are retained. Nested multipart types remain unsupported.
  Invalid/duplicate essential
  header lines, folding, control bytes, transfer encodings, and malformed
  disposition parameters are rejected.
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
it revalidates the original private directory's device/inode and permissions,
then verifies that the opened inode has zero remaining hard links before writing
request bytes. The opened inode must also remain a regular file with no
group/other or setuid/setgid/sticky permission bits, and its owner must match
the spool directory's owner both before and after unlink.
During multipart upload ingress, each staged-file write block uses a cloned
descriptor to check the original inode's identity, UID/GID, exact permission
mode, regular-file type and zero hard-link count before writing, after each
successful partial write, and before retrying an interrupted write. A detected
inode or permission change aborts the request even in PHP-compatible
recoverable-write-error mode, rather than publishing upload error 7. A normal
16,401-byte binary upload is compared byte-for-byte with PHP 8.4 HTTP SAPI.
The repeated metadata checks add filesystem operations and only narrow the
race window; they cannot make writes atomic, prevent every in-place rewrite,
or substitute for audited descriptor-relative open/unlink operations.
Before copying a completed upload, `Upload::copy_to` rechecks that its
file descriptor still refers to a regular, unlinked (`nlink=0`), private
inode without special permission bits whose on-disk length matches the
recorded upload length. The parser also captures the staging inode's device,
inode, UID, GID and exact permission mode before accepting upload bytes.
The copy rejects a replacement descriptor or a changed private mode **before
emitting output**, even if the replacement is anonymous and the same size.
Rust regression tests swap two equal-length upload descriptors and change
0600 to 0400, then verify rejection and successful retry after restoration.
The PHP 8.4 HTTP SAPI differential suite includes two distinct equal-length
binary uploads. These checks do not prevent in-place data modifications or
make file staging atomic. The per-read/write metadata snapshot additionally
checks owner UID, group GID, inode identity and exact mode bits. The Linux
PHP 8.4 HTTP SAPI differential suite checks its upload temp files use mode
0600; Rust tests inject special permission bits and verify fail-closed copy.
The copy uses positional (`pread`-style) reads from explicit offsets rather
than changing the descriptor's shared file offset. A destination writer or
other code seeking through a cloned descriptor cannot skip or duplicate
blocks during the copy; this does not protect against concurrent writes to
the source inode. A mismatch
fails before writing destination bytes; this is a point-in-time integrity
check. Metadata is also revalidated immediately **after each positional read
and before its bytes are written**, detecting observable source modifications
during the read without emitting the affected block. Deterministic fault
injection covers permission changes and same-size rewrites at this boundary.
Metadata is revalidated after **every successful destination write**, including
short writes within an 8192-byte read block, and before retrying an interrupted
destination write. A custom writer may have modified the staged inode before
returning `Interrupted`; no further bytes are offered until validation succeeds.
Source-read `Interrupted` retries also revalidate the staged inode before
attempting another positional read; injected permission tampering fails before
the retry, while a transient interruption resumes within a fixed retry quota.
A faulty positional-read adapter reporting more bytes than its supplied
buffer is rejected with `InvalidData` in the shared copy/readback helper,
before its count can be used as a slice bound. Fault-injection regressions cover
1-byte reads, an 8192-byte block, and a short final block. This is defensive
validation of an invalid I/O contract, not a PHP or operating-system CVE fix.
The check also runs after the loop (including empty uploads). Persistent permission, link-count,
or length changes, and modifications to inode mtime/ctime, now abort before
subsequent blocks are emitted. Deterministic fault injection checks that a
destination writer mutating a cloned source descriptor after the first block
or after a seven-byte partial write cannot receive the remaining bytes,
including same-length rewrites. PHP 8.4
HTTP SAPI differential coverage includes binary uploads spanning more than
four blocks. Timestamp checks are not atomic and cannot detect every
concurrent rewrite (including clock-granularity or timestamp-restoration
cases). A failed copy may already have emitted a prefix, so callers **must
discard the entire destination on every copy error**. For callers that need a result which is never
partially published, `Upload::copy_to_vec(max_bytes)` stages the entire
payload in a private, explicitly size-capped `Vec<u8>` and returns it only
after all copy checks succeed. An error drops the private buffer; the
application's `max_bytes` cap is enforced before allocation. The admitted-request adapter also exposes `AdmittedRequest::copy_accepted_to_vec(index, max_bytes)`, which returns `None` for rejected or overwritten upload indices and uses the same all-or-error, application-capped buffer for surviving uploads. Its Rust regression test uses the 8,221-byte binary payload from the PHP 8.4 HTTP SAPI `file-transactional-vec-oracle` case and verifies cap rejection, exact byte equality and retry without partial publication. This opt-in API
adds a full in-memory copy and does not guarantee an atomic source snapshot,
prevent system OOM, or change `copy_to`'s streaming contract. Per-block validation
does not provide an atomic source snapshot or guarantee detection of
same-length in-place rewrites. After each completed block, the copy also
re-reads that block using positional I/O and compares the bytes against what
was emitted. This adds a bounded-memory second integrity signal even when
metadata changes are not observable, at the cost of an extra read pass. It
does not make the copy atomic, and any failure still requires discarding the
whole destination.
This rejects privileged staging into a
private directory owned by another local account and injected inode permission
relaxation before request data is written. Deterministic fault-injection tests cover directory retargeting,
filename replacement, hard-link creation before unlink, and directory replacement
or permission relaxation immediately after unlink. A detected race fails closed without
writing request bytes; an empty orphan may remain in a renamed directory.
These pathname checks do **not** close the final check/unlink race:
descriptor-relative creation and unlinking with an independently audited
OS interface are required before treating a mutable spool path as safe.

## PHP 8.4 URL-encoded Content-Length admission

`urlencoded::parse_reader_with_php_content_length(reader, limits,
content_length, php_post_max_bytes)` consumes **exactly** the trusted
Content-Length bytes and leaves any following transport message unread.
The hard `Limits::body_bytes` cap is checked before reading. A declared
zero-byte body publishes empty fields without reading. When a nonzero PHP
`post_max_size` is exceeded, the function drains exactly the declared
body without allocating a parser or publishing fields, while retaining the
body byte count. An early EOF, an overreporting `Read` implementation, or
persistent interruptions fail closed. A PHP limit of zero is unlimited.

Accepted requests enable PHP 8.4 percent-decoded NUL-name truncation and
preserve explicit empty names for later `max_input_vars` accounting. The
new Rust regression tests cover exact-size admission, the PHP limit
boundary, hard-limit preflight, transport pipelining, truncation and a
misbehaving reader. PHP 8.4 CLI `parse_str()` provides the expected
accepted-field oracle; HTTP transport framing and Content-Length
validation remain the caller's responsibility. This adapter does not
register `$_POST` with Zend or replace an HTTP SAPI.

The `urlencoded` CLI exposes this adapter with `--php-content-length BYTES`
and optional `--php-post-max-size BYTES`. It uses the trusted declared length
rather than reading stdin to EOF, truncates decoded NUL names, and retains
ignored names for `--php-max-input-vars` accounting. The PHP quota flag
requires a Content-Length; the Content-Length mode currently supports only
PHP's default `&` input separator (custom separator flags are rejected).
For example:

```sh
printf 'a=1&b=2NEXT' | cargo run --offline --locked --bin urlencoded -- \\
  --php-content-length 7 --php-post-max-size 7
```

The Linux CI compares accepted and oversized URL-encoded POST trees with a
real PHP 8.4 HTTP SAPI using `tools/differential_urlencoded_content_length.py`.
When `--php-max-input-vars N` is combined with `--php-content-length`,
the harness uses HTTP SAPI counting: separator-only blank pairs consume slots
and PHP 8.4 HTTP SAPI accepts up to N+1 wire tokens (including ignored names).
Without Content-Length, the same flag retains CLI `parse_str()` counting,
which admits N nonblank tokens. The SAPI-specific Rust entry point is
`parse_reader_with_php_content_length_sapi_vars`; its caller must project
with `max_input_vars.saturating_add(1)`. These behaviors were measured against
PHP 8.4.24 and are checked against the CI PHP 8.4 HTTP SAPI oracle.
The CLI is a test harness: callers must validate HTTP Content-Length framing
and independently enforce transport quotas before invoking the adapter.

## PHP 8.4 HTTP GET query-string adapter

`urlencoded::parse_reader_with_php_query_string` processes a bounded,
transport-extracted QUERY_STRING using PHP 8.4 GET semantics. Unlike URL-encoded
POST (which PHP's HTTP SAPI splits at `&` regardless of INI settings),
GET honors `arg_separator.input`. Empty configuration falls back to
`&`; escaped separators are decoded after splitting, and decoded NUL
truncates variable names. The companion `_counting_vars` API preserves
empty-name fields for the GET input-variable quota.

When the trusted SAPI supplies the exact QUERY_STRING byte length, use
`parse_reader_with_php_query_string_length` or its
`_length_counting_vars` companion. They consume exactly that many bytes
without reading into the next request, reject premature EOF as `Truncated`,
and enforce the independent hard body quota before any input is read.
A zero-length query succeeds without consuming input. The caller must supply
an already-validated length; these APIs do not parse HTTP framing.

The CLI `urlencoded --php-query-string --php-arg-separators '|;'`
emits a binary-safe `get_tree`. Append `--php-query-length N` when stdin
contains an N-byte query followed by unrelated bytes; the suffix is ignored. With `--php-max-input-vars N`, GET counts
N nonblank wire pairs including ignored names, but not separator-only blanks.
No POST SAPI N+1 adjustment applies. GitHub Actions compares against real PHP
8.4 HTTP SAPI with custom INI separators, percent escapes, collisions and
quota boundaries. Upstream HTTP framing and path parsing remain out of scope:
this adapter does not install native `$_GET` values in Zend.

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
Multipart Content-Length ingress uses a checked length-limited reader rather
than `Read::take`: a custom I/O source claiming more bytes than its short
read buffer is rejected as `InvalidData`, even when its claimed count fits
the parser's larger 8192-byte buffer. Fault injection covers the accepted
and over-`post_max_size` paths, including a malformed second-block read and
automatic staged-upload cleanup. The PHP 8.4 HTTP SAPI oracle also checks an
8199-byte binary upload (including NUL and 0xff) under both admission modes.
These checks do not authenticate HTTP framing or eliminate filesystem
pathname races; callers still need a trusted upstream transport.


An additional opt-in PHP 8.4 nesting slice is exposed by
`Request::into_post_with_php_limits(limits, max_input_vars,
max_input_nesting_level)` and the `urlencoded` CLI flags
`--php-max-input-vars` and `--php-max-input-nesting-level`.
Ordinary fields exceeding the nesting level are discarded while later fields
are retained; discarded fields still count toward `max_input_vars`. As in
PHP 8.4, an over-depth field also removes any previously registered top-level
variable with the same normalized root name. A later valid field recreates
that root at its new insertion-order position; numeric root keys follow PHP
integer-key matching. Fields already excluded by `max_input_vars` do not
trigger this removal. Detached nodes remain charged against the adapter's
resource limits. The wire parser's hard limits and FILES upload-name policy
remain strict.
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


## PHP 8.4 lexical comments (research evaluator)

The bounded `php_eval` lexer now treats `//`, `#` and `/* ... */`
comments as whitespace between supported PHP tokens, including within
arithmetic expressions, assignments, and between output statements.
Line comments end at LF, CR, or a PHP closing tag; `#[` is reserved for
unsupported PHP attributes rather than misread as a hash comment.
Unterminated block comments (including the overlapping `/*/` sequence)
fail closed without publishing buffered output. Source-size and work
budgets still apply. This does not add general PHP lexical modes,
string interpolation, attributes, or mixed PHP/HTML/PHP templates.

Fixed and seeded comment-placement cases are compared byte-for-byte
with the installed PHP 8.4 CLI, while malformed and unterminated
comments are exercised as strict rejections.

## PHP 8.4 source-position magic constant (research evaluator)

The bounded `php_eval` interpreter recognizes case-insensitive
`__LINE__` as a signed integer at its token's source position. It counts
CRLF as one line break and bare CR or LF as one break each, matching PHP 8.4
CLI on the supported source forms. The magic constant works in arithmetic,
assignments, nested expressions, and comma-separated final `echo` arguments.
Longer identifiers such as `__LINE__suffix` are rejected rather than
accepted as a magic-constant prefix. The existing 4096-byte source limit
bounds source-position scanning; unsupported magic constants, string values,
includes, file context and PHP compilation semantics are not implemented.

The PHP CLI differential suite checks fixed and generated line-ending cases
against the installed PHP 8.4 interpreter, including CR, LF, CRLF, mixed-case
spelling, source-position changes and malformed identifier boundaries.
This remains a standalone research evaluator, not a Zend compiler.

## PHP 8.4 empty statements and terminal top-level return (research evaluator)

The bounded `php_eval` CLI now accepts standalone `;` statements anywhere
between supported statements, including repeated semicolons. Each empty
statement consumes one work unit. It also accepts case-insensitive top-level
`return;` and `return expression;` as the **final** statement, with optional
closing `?>`; PHP 8.4 permits omitting the semicolon immediately before that
closing tag. The return expression is evaluated with the existing scalar
rules, but its value is not printed or used as a CLI exit status. Previously
buffered output is committed only if the entire accepted program succeeds.

A PHP script may contain syntactically valid but unreachable statements
after `return`. This research evaluator currently **rejects** those programs
instead of skipping their validation. Unsupported expressions and malformed
comments after `return` likewise fail closed. PHP may have emitted earlier
output before a runtime exception; rustpon retains its stricter atomic-output
contract. Rust unit tests cover terminal returns, empty-statement work
exhaustion and rejected trailing code; the PHP CLI differential suite compares
fixed and seeded accepted programs with PHP 8.4. No general control-flow,
function-return stack or Zend execution lifecycle is implied.

## PHP 8.4 terminal exit/die and CLI status (research evaluator)

The standalone `php_eval` CLI recognizes case-insensitive `exit` and `die`
as terminal statements, with no argument (`exit;`, `exit();`) or one
bounded int/bool/null expression (`exit(257);`, `die(true);`). PHP 8.4
allows a single trailing argument comma. The expression's signed 64-bit
integer value determines the Unix process status modulo 256; `exit(-1)`
returns status 255 and `exit(256)` returns status 0. The evaluator flushes
previously buffered stdout before exiting with that status.

Only terminal use is supported. Although real PHP can execute `exit`
before later valid statements, this research parser rejects all source after
`exit`/`die` (except whitespace/comments and an optional closing PHP
tag), because it has no syntax-only pass for unreachable code. Invalid
expressions, unclosed comments, and unsupported string exit messages fail
closed with no stdout. The CLI differential suite checks stdout **and**
process status against PHP 8.4, including negative and out-of-range exit
values. This is not a complete PHP termination, shutdown-handler, or
exception-lifecycle implementation.

## PHP 8.4 scalar var_dump() statements (research evaluator)

The bounded `php_eval` CLI supports case-insensitive, statement-only
`var_dump(value[, ...]);` for integer, boolean and null expressions. It
emits PHP 8.4's exact scalar type tags and newline-delimited output
(`int(...)`, `bool(true/false)`, `NULL`), without echo-style coercion.
Arguments are evaluated in source order, including supported local variable
updates; one trailing comma and a terminal `?>` are accepted. The 1,024-byte
cumulative output budget applies across `echo`, `print` and `var_dump`,
and no output is published if a later statement fails. The differential
suite checks fixed and seeded byte-for-byte cases against the PHP 8.4 CLI.

This is not general `var_dump` support: arrays, strings, objects, references,
named arguments and expression-level calls are not modeled. Runtime errors
and unsupported syntax fail closed rather than reproducing PHP's partially
flushed stdout.

## PHP 8.4 discarded scalar expression statements (research evaluator)

The bounded `php_eval` interpreter now executes standalone scalar expressions
such as `1+2;`, `abs(-3);`, `isset($missing);`, and
`(var_dump(7)+1);` without printing the expression's result. Side effects
from supported nested calls still emit their PHP-compatible output in order.
A final expression can end with `?>` instead of a semicolon, with the
existing single-newline closing-tag rule. Expression parsing continues to
enforce the source, depth, work and output quotas; malformed expressions,
unsupported calls, and runtime errors reject the entire program before
publishing buffered stdout. Simple variable-starting statements still follow
the existing assignment/update grammar; general PHP expression statements,
string values, and arbitrary calls are not yet implemented.

The PHP 8.4 CLI differential corpus includes fixed examples, 64 seeded
arithmetic statements, and strict rejection cases for malformed syntax,
runtime errors, and unsupported PHP re-entry after terminal template bytes.

## PHP 8.4 ordered output and local statement execution (research evaluator)

The bounded `php_eval` CLI accepts multiple `echo` and `print` statements,
interleaved with local assignments, `unset()`, and scalar increment/decrement.
The interpreter evaluates statements in source order, so subsequent expressions
observe earlier variable changes. A program with no output statement succeeds
with empty stdout. Mixed-case output keywords and comma-separated `echo`
arguments retain their existing PHP 8.4 behavior.

```sh
printf '%s' '<?php $x=1; echo $x; $x+=2; print $x;' |
  cargo run --offline --locked --bin php_eval
# stdout: 13
```

The 1024-byte output quota is **cumulative** across all output statements.
No bytes are published until the complete program succeeds, including later
syntax, division-by-zero and resource-limit checks. PHP CLI can emit earlier
bytes before a runtime exception; rustpon intentionally rejects the whole
program instead. Only the existing bounded scalar expression grammar and
simple local variables are supported: arbitrary PHP control flow, user-defined
functions, arrays, strings, Zend execution and SAPI integration are not.
The PHP 8.4 CLI differential suite compares fixed and generated interleaved
statements byte-for-byte and separately tests malformed later statements.

## Bounded scalar variables in the research PHP evaluator

The `php_eval` binary now accepts up to 64 assignments to at most 32
case-sensitive, ASCII-named local integer/boolean variables, interleaved
with `echo` and `print` statements. Expressions can read previously initialized variables,
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


### PHP 8.4 terminal print and case-insensitive echo (research)

The bounded `php_eval` CLI accepts `print expression;`
statements alongside `echo expression;`, including the parenthesized
form `print(expression);`. Both keywords are case-insensitive, matching PHP
8.4. The interpreter evaluates the existing integer/boolean/null subset and
writes its bytes only after the complete program passes validation. PHP
`print` also works inside supported scalar expressions and returns the
integer 1 after emitting its operand. For example, `echo print 7;`
produces `71`, while `echo (print 0)+(print 1);` produces `012`.
Nested print operands, case-insensitive spelling, and output order are
compared byte-for-byte with PHP 8.4 CLI. Terminal `echo` accepts
comma-separated scalar expressions (for example, `echo 1, true, null, 2;`),
concatenating their PHP 8.4 string representations without separators. `print`
still accepts exactly one expression. A separate 1024-byte output quota
prevents unbounded output accumulation; the source and evaluation-work limits
also apply to each argument. Malformed expressions, unknown variables, quota
failures, and later-argument evaluation errors produce no stdout, even when
an earlier argument was valid. PHP itself can emit partial output before a
runtime exception; this fail-closed prototype intentionally does not.
The differential suite checks both keywords against real PHP 8.4 CLI,
including mixed-case spelling, scalar conversions, prior assignments,
comma lists, and rejection boundaries.

### PHP 8.4 scalar increment/decrement statements and expressions (research)

The bounded `php_eval` interpreter accepts prefix `++$x;`/`--$x;` and
postfix `$x++;`/`$x--;` as standalone statements for initialized scalar
variables. It also evaluates these operators inside scalar expressions, including
`echo ++$x, $x++, $x;`, `print $x++;`, function arguments, and arithmetic.
Prefix returns the updated value; postfix returns the previous tagged value.
Updates are visible to subsequent operands and `echo` arguments. PHP 8.4
binds these updates before exponentiation (`++$x**2` is `(++$x)**2`).
Integer values change by one; PHP 8.4 leaves booleans unchanged, converts
incremented null to integer one, and leaves decremented null as null.
The usual assignment-count (for standalone statements), work and variable
limits apply. Float promotion on overflow and undefined-variable warning paths
remain unsupported; malformed lvalues such as `++($x)` and `($x)++` fail
closed. Output is published only after the complete program succeeds.

```sh
printf '%s' '<?php $x=null; ++$x; $x++; echo $x;' |
  cargo run --offline --locked --bin php_eval
# stdout: 2
```

The differential suite compares both token placements, mixed scalar tags,
expression evaluation order, exponentiation precedence, boundary values,
repeated updates, and malformed syntax with PHP 8.4 CLI.

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
`unset($name)`, `unset($a,$b,)`, and interleaved assignments and output
statements. Removing a defined variable makes subsequent `isset()`
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

## PHP 8.4 predefined integer constants (research evaluator)

The bounded `php_eval` CLI recognizes the exact, case-sensitive PHP
constants `PHP_INT_MAX` (9223372036854775807), `PHP_INT_MIN`
(-9223372036854775808), and `PHP_INT_SIZE` (8) on its explicit 64-bit
PHP ABI. The constants participate in arithmetic, comparisons, bitwise
operations, assignments and builtin calls; `PHP_INT_MIN` also supplies
the otherwise unrepresentable minimum signed integer directly.
Names are matched as complete identifiers: `php_int_max` and
`PHP_INT_MAXIMUM` are not accepted. Operations that PHP would promote
to floating point, such as `PHP_INT_MAX+1` or `-PHP_INT_MIN`, remain
unsupported and are rejected without output.

The Rust regression checks exact boundaries and malformed names. The
PHP 8.4 CLI differential suite compares fixed and seeded arithmetic,
ordering, and variable-update programs against the installed PHP binary.
These predefined constants do not imply general constant lookup,
64-bit compatibility on a 32-bit host, or Zend integration.

## PHP 8.4 upload error constants (research evaluator)

The bounded `php_eval` interpreter now recognizes all eight standard PHP
upload status constants as signed integers: `UPLOAD_ERR_OK=0`,
`UPLOAD_ERR_INI_SIZE=1`, `UPLOAD_ERR_FORM_SIZE=2`,
`UPLOAD_ERR_PARTIAL=3`, `UPLOAD_ERR_NO_FILE=4`,
`UPLOAD_ERR_NO_TMP_DIR=6`, `UPLOAD_ERR_CANT_WRITE=7`, and
`UPLOAD_ERR_EXTENSION=8`. Status 5 is intentionally undefined.
Constants are case-sensitive, require complete identifier boundaries,
and participate in existing scalar arithmetic, comparisons, assignments,
and type predicates. The Rust regressions and PHP 8.4 CLI oracle verify
their values and integer types, plus rejection of unknown names.
This exposes constants in the standalone language experiment; it does
not populate the PHP `$_FILES` superglobal or implement a Zend SAPI.

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
variable remains a notice-free probe. Redundant parentheses around a
simple variable preserve this behavior: `isset((($missing)))` returns false
and `empty((($missing)))` returns true, without an undefined-variable warning.
Mixed parenthesized and bare operands in `isset()` are supported. The original
value tags are retained, and expression parsing obeys existing source, work
and recursion limits.

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
It also accepts `is_float()` (alias `is_double()`), `is_string()`,
`is_array()`, `is_object()`, `is_resource()`, `is_iterable()`, and
`is_countable()`. These return `false` for every supported int/bool/null
value, matching PHP 8.4; the evaluator cannot construct the other value
kinds yet. Each predicate takes exactly one supported expression, inspects
the actual `Value` tag without coercing it, and returns a PHP boolean.
For example, `is_int(true)` is false, `is_int(true+0)` is true, and
`is_bool(is_int(1))` is true. Existing source/work/depth limits apply to
nested calls. Missing/extra arguments, unknown functions and unknown
variables fail closed without output. The PHP 8.4 CLI differential suite
checks fixed type boundaries, nested expressions and seeded values,
including 72 fixed and 96 seeded absent-kind predicate calls. Unsupported
arity and unknown function names are rejected. Strings, floats, arrays,
references, user functions and general Zend type
semantics remain outside this research-only subset. In particular,
`is_numeric()` recognizes supported integers but has no numeric-string or
floating-point support; `is_scalar(null)` is false.

### PHP 8.4 scalar `is_callable()` (research evaluator)

The bounded `php_eval` interpreter now recognizes case-insensitive
`is_callable(value[, syntax_only])` for its existing integer, boolean and
null values. All return `false`, matching PHP 8.4: none of these values
represent a callable, including when `syntax_only` is true. The optional
second argument is evaluated and coerced to boolean, including its runtime
errors, even if the first argument cannot be callable. One trailing comma
after the first or second argument is accepted. Calls preserve existing
depth/work limits and fail without publishing buffered output on errors.
The third by-reference `callable_name` output parameter, callable strings,
arrays, objects and function invocation remain unsupported. PHP 8.4 CLI
differential tests cover fixed and 96 seeded calls, argument order,
mixed-case names, nested predicates and malformed/unsupported arity.

### PHP 8.4 finite/NaN/infinite predicates (research evaluator)

The bounded `php_eval` evaluator additionally accepts case-insensitive
`is_finite()`, `is_infinite()`, and `is_nan()` with exactly one argument.
For its existing integer, boolean and null values, PHP 8.4 coerces the
argument to a finite numeric value: `is_finite()` returns `true`,
while `is_infinite()` and `is_nan()` return `false`. These calls
return PHP boolean values and retain the existing depth/work limits.
Floating-point values (including `INF` and `NAN`), numeric strings,
and PHP's warning/deprecation emission are not modeled. The PHP 8.4
CLI differential suite covers fixed and seeded cases, mixed-case names,
nested calls and strict rejection of unsupported arity.

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

### PHP 8.4 CLI terminal closing-tag subset

The bounded `php_eval` harness accepts `?>` as the final terminator of
`echo`, `print`, simple assignments, compound assignments, standalone
prefix/postfix `++` and `--`, and `unset()`, even without a preceding
semicolon. It also retains terminal `return` / `exit` support. Like PHP
8.4, it consumes at most one immediately following LF, CR, or CRLF.
The CLI differential corpus includes the mutation/unset cases and checks
their observable output against PHP 8.4; Rust regressions cover the
corresponding parse and rejection boundaries.

A terminal template suffix is emitted byte-for-byte after PHP consumes
one immediately following CR, LF or CRLF. UTF-8 output remains in the
scalar output buffer; suffixes containing arbitrary binary bytes (including
NUL and invalid UTF-8) use a separate staged byte buffer, written after
scalar output. All output shares the 1,024-byte transactional quota.
Top-level return/exit suppress unreachable template output. Further PHP
opening tags still fail closed; mixed PHP/HTML/PHP scripts require a
proper mode-switching lexer. The PHP 8.4 CLI differential corpus checks
binary suffixes, CRLF handling, output order and return/exit suppression.

### PHP 8.4 trailing commas in scalar builtins (research evaluator)

The bounded `php_eval` interpreter accepts a single trailing comma in
supported scalar builtin calls, including `intdiv()`, `abs()`, `min()`,
`max()`, the type predicates, `boolval()`, and `intval()`. Both one- and
two-argument `intval()` forms are supported. `min()` retains PHP's special
two-argument tie behavior with a trailing comma. Repeated commas, missing
required arguments, and a trailing comma in the `empty()` language construct
still fail closed. Fixed and seeded programs are compared byte-for-byte with
PHP 8.4 CLI; this does not add general function calls or a PHP VM.

### PHP 8.4 opening tags and short echo (research evaluator)

The bounded `php_eval` CLI recognizes the case-insensitive `<?php` long
opening tag and the `<?=` short-echo tag. Short echo evaluates an ordered
comma-separated expression list and can continue with normal supported
statements after a semicolon. Like explicit `echo`, the closing `?>` tag
may terminate the statement without a semicolon. The PHP 8.4 CLI differential
suite compares the output bytes of these tag forms, including mixed-case
long tags, comments, magic line numbers, and subsequent statements.

A short echo still requires a semicolon or closing tag; terminal UTF-8
template output is supported, while PHP re-entry and unreachable PHP code
after `exit` remain rejected.
This does not implement general mixed HTML/PHP templates or short `<?`
tags. Output is committed only after the complete supported program succeeds.

### PHP 8.4 expression-level `var_dump()` (research evaluator)

The bounded `php_eval` interpreter now accepts `var_dump()` inside scalar
expressions, as well as its existing statement form. PHP 8.4 evaluates the
arguments left to right, emits each scalar's typed representation, and returns
`null`; the interpreter models that return tag so nested calls and
`is_null(var_dump(...))` behave consistently. Both forms share the same
argument parser, depth/work accounting, and cumulative 1,024-byte output
budget. Failed programs do not publish staged output. No strings, arrays,
objects, references, or arbitrary user-defined function calls are supported.

Rust regression tests cover nested calls, evaluation order, variable
side effects, invalid argument lists and excessive nesting. The Linux
PHP 8.4 CLI oracle adds 60 deterministic fixed/seeded successful programs
and six strict rejection cases. This is a small language-compatibility slice,
not a Zend replacement or a production PHP runtime.

## PHP 8.4 scalar strlen() compatibility slice

The standalone `php_eval` research harness supports `strlen(value)` for
its existing integer, boolean and null value tags. PHP 8.4 coerces these
values to strings before counting bytes: `strlen(PHP_INT_MIN)` is 20,
`strlen(true)` is 1, and `strlen(false)` and `strlen(null)` are 0.
Calls accept an optional trailing comma, and the function name is
ASCII-case-insensitive. Arguments are evaluated exactly once in order,
including nested `var_dump()` side effects. The existing source, work,
recursion and output bounds remain in effect; invalid arity and unsupported
expressions fail closed without publishing partial stdout.

This slice does not add PHP string values, Unicode/string encodings, or
general-purpose string handling. The Linux PHP 8.4 CLI differential suite
checks fixed and seeded scalar conversions byte-for-byte and separately
checks `ArgumentCountError` for invalid arity. `strlen(null)` may emit a
PHP deprecation diagnostic on stderr under other PHP error-reporting
settings; the differential oracle compares successful stdout.

## PHP 8.4 scalar `var_export()` (research CLI)

The bounded `php_eval` CLI supports `var_export()` for integer, boolean
and null expressions, with PHP 8.4's exact scalar literal output (`123`,
`true`, `false`, `NULL`), no implicit newline, and a null return value.
The optional `$return` flag is evaluated after the value; false emits
output, while true is explicitly rejected because this VM does not yet
represent PHP strings. Trailing commas, nested calls, work/depth limits
and the cumulative 1,024-byte output cap apply. Output remains private
until the complete program succeeds. Differential tests compare fixed
and seeded scalar cases byte-for-byte against PHP 8.4 CLI.

For the signed 64-bit minimum `PHP_INT_MIN`, PHP 8.4's `var_export()`
prints `-9223372036854775807-1`, rather than the plain decimal
`-9223372036854775808`. This keeps the exported source re-evaluable
as an integer instead of treating an out-of-range decimal literal as
floating-point. The research evaluator now reproduces that byte sequence
for the minimum integer from constants, variables, and arithmetic;
`echo` and `var_dump` retain their distinct existing formatting.
The Linux PHP CLI differential suite includes these boundary cases.

## PHP 8.4 scalar `print_r()` (research CLI)

The bounded `php_eval` interpreter accepts case-insensitive `print_r(value)`
and `print_r(value, false)` for its existing integer, boolean, and null
values, both as statements and nested expressions. PHP 8.4 emits the same
scalar text as `echo` without a newline, then returns `bool(true)`; the
return tag is retained for nested calls and type predicates. Arguments are
evaluated in order, including nested `var_dump()` side effects. The optional
trailing comma and final `?>` statement terminator are supported.

A truthy `$return` flag is evaluated but rejected because PHP returns a
string in that mode and the bounded VM has no string value type. Arrays,
objects, and user-defined functions remain unsupported. Source, work,
recursion, and the cumulative 1,024-byte output budget are enforced; failed
programs never publish partial stdout. Rust regressions cover nested output,
type tags, side effects, invalid arity, unsupported return strings, and output
exhaustion. The PHP 8.4 CLI differential suite compares fixed and seeded
scalar cases byte-for-byte and verifies the oracle's `ArgumentCountError`
for invalid arity. No third-party dependencies are added.

## PHP 8.4 integer `pow()` compatibility slice

The standalone `php_eval` research interpreter now evaluates case-insensitive
`pow(base, exponent)` with two scalar int/bool/null arguments, an optional
trailing comma, nested calls and left-to-right argument evaluation. It shares
the existing checked integer exponentiation with `**` and compound `**=`.
PHP 8.4 results are reproduced when the exponent is nonnegative and the result
fits a signed 64-bit integer, including `0**0`, `PHP_INT_MIN**1`, and
boolean/null coercion. Trivial bases 0, 1, and -1 retain exact integer
results even for exponents larger than 32 bits (shared with `**` and `**=`). Negative exponents and overflow promote to floating
point in PHP; rustpon rejects those cases without publishing partial output.

The Linux PHP 8.4 CLI oracle suite adds fixed edge cases, 128 deterministic
bounded generated cases, and malformed/unsupported calls. This does not add
floating-point values, general PHP functions, or production SAPI integration.

## PHP 8.4 CLI inline HTML opener handling

The experimental `php_eval` command supports a terminal binary-safe HTML suffix
after `?>`, including embedded NUL and non-UTF-8 bytes. With PHP's default
`short_open_tag=0`, literal `<?` sequences
such as `<?xml`, `<?phpx`, and `<?php?>` remain ordinary HTML output.
Only `<?=` or a case-insensitive `<?php` followed by a space, tab, CR,
LF, or EOF is recognized as a PHP opening tag. A recognized PHP re-entry
after the terminal suffix is still rejected rather than interpreted.
The same 1,024-byte cumulative output cap applies to literal HTML.
A UTF-8 BOM directly preceding a recognized leading PHP opening tag is
preserved as three output bytes, before PHP expression output, matching
PHP 8.4 CLI. The BOM counts against the shared output quota. Other leading
HTML (including a BOM followed by arbitrary text) remains unsupported until
a general PHP/HTML mode-switching lexer is available.
The differential test suite compares this narrow behavior with PHP 8.4 CLI,
including byte-for-byte non-UTF-8 templates, BOM-prefixed scripts, and
exact/one-byte-over 1,024-byte output boundaries. A stale strict-rejection
case for valid binary HTML was corrected after Linux CI detected it;
this does not claim complete PHP template or binary-output support.

## PHP 8.4 error-control operator (research evaluator)

The bounded `php_eval` CLI accepts PHP's unary `@` error-control operator
for its supported scalar expressions. Within the operand, reading an undefined
simple variable yields the PHP 8.4 `null` value while suppressing its warning:
`echo @($missing + 1);` outputs `1`, and
`echo @var_dump($missing);` outputs `NULL` followed by a newline.
Nested `@` operators, parentheses, scalar conversions, builtins, and ordinary
variable updates retain the existing depth, work, and output limits.

Only undefined-variable reads are modeled as recoverable warnings. The
operator does not swallow division-by-zero errors, unsupported constructs,
unknown variables outside its operand, or resource-limit failures. Programs
that fail are rejected transactionally without partial stdout, unlike PHP
scripts that may have emitted output before a later runtime error. The PHP
8.4 CLI differential corpus includes 22 successful cases and eight strict
rejections; Rust regression tests also cover excessive nesting. This does
not implement Zend's general warning/notice machinery or error handlers.

## PHP 8.4 error-reporting constants (research evaluator)

The bounded `php_eval` interpreter recognizes the 16 standard PHP 8.4
`E_*` error-level constants as signed integer values. `E_ERROR=1`,
`E_WARNING=2`, `E_STRICT=2048`, `E_USER_DEPRECATED=16384`, and
`E_ALL=30719` are checked alongside the remaining error levels against
PHP 8.4 CLI. In PHP 8.4, `E_ALL` excludes the `E_STRICT` bit.
Names are case-sensitive and require a full identifier boundary. Existing
bitwise operations, integer comparisons, variable assignments, and type
predicates can use these values. The PHP CLI differential suite checks
all values and types, bitmask combinations, and unknown-name rejection.
This does not implement PHP's error handler, `error_reporting()`, or
Zend runtime warning/exception behavior. No new dependencies are required.

## Interrupted readback integrity checks

The post-write upload readback now reuses the initial positional reader's
metadata-checked `Interrupted` retry path. A concurrent staged-inode
permission or identity change during readback aborts before another read is
attempted. Repeated interrupts terminate at the existing bounded retry
quota; short/overreported reads fail closed. Rust fault-injection tests cover
inode permission tampering, transient and persistent interruptions, and an
invalid read count. The PHP 8.4 HTTP SAPI differential corpus includes an
unchanged 16,395-byte binary upload crossing two 8-KiB copy blocks. This
strengthens observable mutation detection but is not an atomic source
snapshot; a streaming destination must still be discarded after any error.


## PHP 8.4 HTTP Cookie scalar slice (experimental)

`cookie::parse_scalar_header(header, limits)` accepts a *single*, already-extracted,
transport-validated Cookie header value. It handles semicolon separators,
leading segment whitespace, PHP's first-wins duplicate rule (including
collisions after dot/space name normalization), raw-URL-decoding of **values**
without converting '+' to space, and binary values including decoded NUL.
Cookie *names* are not percent-decoded. The result owns ordered, normalized
scalar name/value byte pairs and can emit binary-safe hex tuple JSON.
The caller must validate HTTP framing and the behavior of multiple headers.

PHP 8.4 flattens an **unterminated** bracket in a Cookie name to an
underscore (for example, `foo[bar` becomes `foo_bar`), and ignores a
name beginning with `[` because it has no root variable. The first scalar
wins after that normalization. Completed bracket paths, such as
`foo[bar]`, still fail closed with `UnsupportedArrayName` rather than
silently producing incorrect `$_COOKIE` arrays. This standalone slice does
not register variables with Zend/SAPI, implement scalar/array collisions, or
replace PHP's cookie request lifecycle. Header, pair, name, value and retained
byte quotas are independent; they are not full process RSS guarantees.

The `cookie` CLI reads at most 64 KiB plus one overflow byte from stdin
and prints the tagged `cookie_pairs` array only on complete success.
Linux CI compares scalar cases with a real PHP 8.4 HTTP SAPI, including
duplicate names, percent escapes, whitespace and a multi-block value.

### PHP 8.4 default-depth Cookie array registration (opt-in)

The separate `cookie::array::parse_one_level_header` API (name retained
for compatibility) and `cookie --php-arrays` harness register bracket paths
through PHP 8.4's **default** `max_input_nesting_level=64`. A fixed-size
stack array stores the parsed indices; append markers, nested scalar
promotion, integer keys, replacement and binary-safe tagged JSON are
preserved at every supported level. Scalar root names remain first-wins.
When an explicit integer key reaches `i64::MAX`, subsequent `[]` appends
at that array level (including intermediate nested appends) are ignored,
matching PHP 8.4 HTTP SAPI. Other Cookie variables continue to register;
skipped appends still consume the wire-pair and `max_input_vars` quotas.
The independent hard name, pair, header, value and retained-byte quotas
continue to apply. The Cookie array adapter also caps cumulative structural
slot allocations via `cookie::Limits::nodes` (default 4,096). Each newly
inserted root or nested element consumes one slot; later replacement/removal
does not refund it. Updating an existing key does not allocate another slot.
Exhaustion returns `cookie::Error::Nodes` and the CLI emits no partial JSON.
`cookie --php-arrays --max-cookie-nodes N` allows boundary and failure
injection tests. This is a Rust-specific allocation-amplification guard,
not PHP `max_input_vars`, `memory_limit`, or a guarantee about process RSS.
The explicit
`cookie::array::parse_one_level_header_with_php_ini_limits` API and
`cookie --php-arrays --php-max-input-nesting-level N` CLI flag also model
PHP 8.4's configurable INI nesting limit (0–128), including zero disabling
all array registration and an over-depth pair deleting its existing root.
These settings can be combined with `--php-max-input-vars N` independently.
Values above 128 fail with `NestingLevel` instead of claiming compatibility
with a configuration the Rust prototype cannot represent.

At the default depth, an over-depth path (65 or more complete bracket
levels) causes PHP 8.4 HTTP SAPI to remove the previously registered normalized root, discards that
variable, and may register a later valid occurrence of the same name.
The Rust projection now matches this behavior; the ignored wire name
still counts against `max_input_vars`. PHP 8.4 HTTP SAPI ignores
noncontiguous suffixes after the last complete bracket: `a[x]junk[y]`
registers `a[x]`, and `a[x][` also registers `a[x]`. A later valid
assignment can overwrite that leaf; the wire pair still consumes
`max_input_vars`. The same behavior is implemented in the Rust array
projection and covered by HTTP SAPI differential tests. A bracket inside a
completed index remains part of that key (for example, `a[x][y[z]` has
second-level key `y[z`). An exhausted `[]` append is ignored without
wrapping or rejecting unrelated subsequent Cookie variables.

Linux CI compares the default 64-level boundary and configured 127-/128-level
registration against PHP 8.4 HTTP SAPI, with 129-level overflow, root removal,
re-registration and independent input-variable quotas. Rust tests cover the
configured boundary, truncated suffixes, and resource limits on Linux/macOS.
The explicit 128-level
maximum bounds the stack-index table and is distinct from PHP's default 64;
no compatibility is claimed for PHP INI settings greater than 128. This remains standalone request metadata projection,
not native Zend/$_COOKIE integration, an FPM implementation, or a claim
of full PHP INI compatibility.


## PHP 8.4 HTTP Cookie max_input_vars admission

The opt-in `cookie::parse_scalar_header_with_php_max_input_vars` and
`cookie::array::parse_one_level_header_with_php_max_input_vars` adapters
match PHP 8.4 HTTP SAPI variable-count admission while retaining independent
hard header-byte, wire-pair, name, value, and retained-byte limits.
`cookie --php-max-input-vars N [--php-arrays]` exposes the same behavior.
Without this flag, existing parsing behavior is preserved.
PHP `max_input_vars=0` admits no Cookie variables. Each nonempty wire name
consumes a slot, including duplicates and names PHP ignores because the root
starts with `[`; empty names (`=value`) and separator-only segments do not.
Once the quota is reached, later variables are not registered or decoded.
Every remaining nonempty wire segment still counts against the independent
hard `Limits::pairs` budget, including variables skipped by PHP's quota.
An over-budget header fails without publishing partial JSON; PHP's
`max_input_vars` must not bypass the server's resource limit.
The entire header still undergoes hard byte-size and CR/LF/NUL validation.
A real PHP 8.4 HTTP SAPI is used as the differential oracle with limits
0, 1, 2 and 3. This is a standalone experimental admission adapter, not a
Zend or native PHP-FPM integration.
