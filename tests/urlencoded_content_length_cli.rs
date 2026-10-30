//! End-to-end CLI coverage for trusted Content-Length admission.
//! The library owns the transport boundary; this harness intentionally
//! ignores bytes after the declared length, as a server would for pipelining.
use std::io::Write;
use std::process::{Command, Stdio};

fn run(args: &[&str], body: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_urlencoded"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Err(error) = child.stdin.take().unwrap().write_all(body) {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    child.wait_with_output().unwrap()
}

#[test]
fn cli_content_length_is_exclusive_and_php_nul_names_are_truncated() {
    let body = b"x%00tail=1&b%5B%5D=2";
    let mut pipelined = body.to_vec();
    pipelined.extend_from_slice(b"&not_in_request=3");
    let length = body.len().to_string();
    let output = run(&["--php-content-length", &length], &pipelined);
    assert!(output.status.success(), "{:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(r#"["s","78"],["s","31"]"#));
    assert!(stdout.contains(r#"["s","62"],["a","#));
    assert!(!stdout.contains("6e6f745f696e5f72657175657374"));
}

#[test]
fn cli_php_post_max_size_discards_only_when_exceeded() {
    let body = b"a=1&b=2";
    for (limit, publishes) in [(0, true), (6, false), (7, true), (8, true)] {
        let output = run(
            &[
                "--php-content-length",
                "7",
                "--php-post-max-size",
                &limit.to_string(),
            ],
            body,
        );
        assert!(output.status.success(), "{limit}: {:?}", output.stderr);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.contains(r#"["s","61"],["s","31"]"#), publishes);
        if !publishes {
            assert!(stdout.contains(r#""post_tree":["a",[]]"#));
        }
    }
}

#[test]
fn cli_content_length_zero_ignores_stdin_and_truncation_fails_closed() {
    let empty = run(&["--php-content-length", "0"], b"a=1");
    assert!(empty.status.success(), "{:?}", empty.stderr);
    assert!(
        String::from_utf8(empty.stdout)
            .unwrap()
            .contains(r#""post_tree":["a",[]]"#)
    );

    let short = run(&["--php-content-length", "10"], b"a=1");
    assert!(!short.status.success());
    assert!(short.stdout.is_empty());
    assert!(String::from_utf8_lossy(&short.stderr).contains("Truncated"));
}

#[test]
fn cli_content_length_options_are_validated_before_reading() {
    let cases: &[&[&str]] = &[
        &["--php-post-max-size", "3"],
        &["--php-content-length", "bad"],
        &["--php-content-length", "18446744073709551616"],
        &["--php-content-length", "4", "--php-arg-separators", "|"],
        &["--php-content-length", "4", "--php-semicolon-separator"],
        &["--php-content-length", "4", "--php-content-length", "5"],
    ];
    for args in cases {
        let output = run(args, b"a=1");
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
    // Independent server hard limit is checked before any body read.
    let huge = run(&["--php-content-length", "18446744073709551615"], b"");
    assert!(!huge.status.success());
    assert!(huge.stdout.is_empty());
    assert!(String::from_utf8_lossy(&huge.stderr).contains("BodyBytes"));
}

#[test]
fn cli_content_length_applies_http_sapi_var_counting() {
    let cases: &[(&[u8], &str, &str, &str)] = &[
        (b"a=1&b=2&c=3&d=4", "2", "63", "64"),
        (b"&&a=1&b=2", "2", "61", "62"),
        (b"a=1&=ignored&b=2&c=3", "1", "61", "62"),
        (b"a=1&b=2", "0", "61", "62"),
    ];
    for &(body, limit, accepted, rejected) in cases {
        let output = run(
            &[
                "--php-content-length",
                &body.len().to_string(),
                "--php-max-input-vars",
                limit,
            ],
            body,
        );
        assert!(output.status.success(), "{limit}: {:?}", output.stderr);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains(&format!(r#"["s","{accepted}"]"#)),
            "{limit}: {stdout}"
        );
        assert!(
            !stdout.contains(&format!(r#"["s","{rejected}"]"#)),
            "{limit}: {stdout}"
        );
    }
}

#[test]
fn cli_query_string_honors_ini_separators_and_publishes_get_tree() {
    let output = run(
        &["--php-query-string", "--php-arg-separators", "|;"],
        b"a=1|n%00tail=2;%7C=encoded&x=3",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains(r#""get_tree":["a","#));
    assert!(json.contains(r#"["s","61"],["s","31"]"#));
    assert!(json.contains(r#"["s","6e"],["s","32"]"#));
    assert!(json.contains(r#"["s","7c"],["s","656e636f64656426783d33"]"#));
    assert!(!json.contains(r#""post_tree":"#));
}

#[test]
fn cli_query_string_counts_get_quota_without_post_sapi_adjustment() {
    let body = b"a=1||=ignored;b=2|c=3";
    let cases = [
        ("1", false, false),
        ("2", false, false),
        ("3", true, false),
        ("4", true, true),
    ];
    for (quota, accepted_b, accepted_c) in cases {
        let output = run(
            &[
                "--php-query-string",
                "--php-arg-separators",
                "|;",
                "--php-max-input-vars",
                quota,
            ],
            body,
        );
        assert!(output.status.success(), "{quota}: {:?}", output.stderr);
        let json = String::from_utf8(output.stdout).unwrap();
        assert!(json.contains(r#"["s","61"],["s","31"]"#));
        assert_eq!(json.contains(r#"["s","62"],["s","32"]"#), accepted_b);
        assert_eq!(json.contains(r#"["s","63"],["s","33"]"#), accepted_c);
    }
    let bad = run(
        &["--php-query-string", "--php-content-length", "3"],
        b"a=1",
    );
    assert!(!bad.status.success());
    assert!(bad.stdout.is_empty());
}
