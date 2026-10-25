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
