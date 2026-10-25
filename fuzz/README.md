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
