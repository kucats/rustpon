# Coverage-guided parser fuzzing (research only)

The isolated fuzz workspace adds `libfuzzer-sys = 0.4.13`, pinned to an
exact version. The upstream rust-fuzz/libfuzzer project uses MIT/Apache-2.0
for its Rust code and NCSA for bundled LLVM libFuzzer code. This dependency
is not linked into the production parser or FFI crate.

With nightly Rust and cargo-fuzz installed, run from the repository root:

```sh
cargo +nightly fuzz run urlencoded -- -max_len=8192 -max_total_time=120
cargo +nightly fuzz run multipart -- -max_len=8193 -max_total_time=120
```

For multipart the first byte chooses one of four legal MIME boundaries;
ASCII `0` selects `b` and ASCII `1` selects `AaB03x`. The remaining
bytes are the multipart body. For URL-encoded input all bytes are body bytes.

Each target checks that whole-body, five chunk sizes and the blocking-Read
adapter agree on accept/reject. Accepted requests must have identical ordered
metadata, raw field bytes, statistics and copied upload bytes. Rejected inputs
may have different first quota errors due to preflight order. A fresh private
0700 directory is used for each multipart input and checked for persistent
staging files after every parse. Inputs and resource budgets are bounded.

Linux CI checks fuzz formatting, compiles both targets and executes fixed
seed tests. This does **not** mean long-running fuzz campaigns or sanitizers
have completed. Existing PHP 8.4 CLI/SAPI differential tests remain the
compatibility oracle. A PHP-FPM production rollout remains prohibited without
further lifecycle testing and independent security review.

## Bounded Linux CI mutation campaign

Linux CI additionally **executes** both libFuzzer binaries with the checked-in
seed corpora, fixed seed `20261026`, at most 256 executions per target and
a 20-second time budget per target. This is a short mutation smoke campaign,
not a long-running sanitizer or exhaustive coverage campaign. The binaries
are built in the isolated fuzz workspace; no fuzzing dependency is introduced
into the production parser or FFI.

For every successfully parsed nonempty upload, the multipart harness injects
destination `StorageFull` errors after 0, 1, half and all-but-one bytes.
It checks the exact partial destination prefix, then copies the original
staged upload again from offset zero and verifies the complete byte sequence.
The spool is checked for leaked staged files after every parse. These checks
exercise the caller-visible nontransactional copy contract; callers still
must discard any partially written destination.

## Admission boundary fuzz target

`admission` fuzzes the **consuming** server-policy handoff, not just the
wire decoder. It compares independently projected typed FILES metadata,
checks byte-for-byte accepted upload copies, and asserts rejected/overwritten
wire indices cannot be copied. For every parsable body it replays the request
under a one-byte-too-small server body limit, a one-byte-too-small file limit
(when applicable), and a zero-byte FILES metadata budget. Each denial must
release staged descriptors and leave the private spool empty. Inputs remain
bounded to 8192 body bytes and no external dependency is added.

```sh
cargo +nightly fuzz run admission -- -max_len=8193 -max_total_time=120
```

CI also executes a fixed-seed 256-run mutation smoke campaign for this target.
The existing PHP 8.4 SAPI oracle includes advisory MAX_FILE_SIZE collisions,
zero-byte uploads and binary replacements. Short smoke runs do not establish
long-term coverage or native PHP-FPM safety.

## Instrumented sanitizer-coverage CI (Linux)

The `fuzz-sanitized` GitHub Actions job uses **nightly Rust** and the
version-pinned `cargo-fuzz 0.13.2` CLI. Unlike directly running a
`cargo build`-produced libFuzzer binary, `cargo fuzz run` enables
libFuzzer's **SanitizerCoverage** feedback and **AddressSanitizer** for
the fuzz harness and Rust parser dependency. The job executes all three
targets (`urlencoded`, `multipart`, `admission`) with deterministic
seed `20261026`, 256 executions and a 30-second safety limit each.
The separate Linux/macOS stable-toolchain checks continue to run.

A failing fuzz run stops CI and uploads any `fuzz/artifacts/` crash
inputs as a GitHub Actions artifact (14-day retention). These inputs
are **untrusted**: reproduce in an isolated environment, minimize using
`cargo +nightly fuzz tmin <target> <crash-file>`, then add a synthetic,
bounded, deterministic regression to the checked-in corpus or unit tests.
Never add private request bodies or credentials to the corpus.

For a local instrumented reproduction, install the pinned CLI with
`cargo install cargo-fuzz --version 0.13.2 --locked`, install nightly,
and run `cargo +nightly fuzz run --sanitizer address admission -- -runs=256`.
The campaign is deliberately short: a successful run does not prove
complete coverage, leak freedom, or PHP-FPM safety. The regular PHP 8.4
CLI/SAPI differential tests remain independent of sanitizer findings.
