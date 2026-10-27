use std::io::Write;
use std::process::{Command, Stdio};

fn run_with_args(args: &[&str], body: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_urlencoded"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Invalid CLI options may terminate the child before the parent writes.
    // BrokenPipe is expected in that case; assert the process result below
    // rather than turning a correctly rejected invocation into a flaky test.
    if let Err(error) = child.stdin.take().unwrap().write_all(body) {
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::BrokenPipe,
            "unexpected stdin write error: {error}"
        );
    }
    child.wait_with_output().unwrap()
}

fn run(body: &[u8]) -> std::process::Output {
    run_with_args(&[], body)
}

#[test]
fn cli_custom_separator_and_conflicting_options() {
    let output = run_with_args(&["--php-arg-separators", "|"], b"a=1|b=2&c=3");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(r#"["s","61"],["s","31"]"#));
    assert!(stdout.contains(r#"["s","62"],["s","3226633d33"]"#));

    for args in [
        vec!["--php-arg-separators", "|", "--php-semicolon-separator"],
        vec!["--php-semicolon-separator", "--php-arg-separators", "|"],
        vec!["--php-arg-separators", "|", "--php-arg-separators", ";"],
    ] {
        let invalid = run_with_args(&args, b"a=1");
        assert!(!invalid.status.success(), "{args:?}");
        assert!(invalid.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn cli_invalid_options_reject_large_stdin_without_partial_output() {
    // A body larger than a pipe buffer makes the early-exit race reproducible.
    let body = vec![b'a'; 1024 * 1024];
    let output = run_with_args(
        &["--php-arg-separators", "|", "--php-semicolon-separator"],
        &body,
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn cli_commits_tagged_tree_only_after_successful_parse() {
    let output = run(b"x=hello+world&a%5By%5D=2");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with(r#"{"post_tree":["a","#));
    assert!(stdout.contains(r#"["s","68656c6c6f20776f726c64"]"#));
    assert!(stdout.contains(r#"["s","79"]"#));
    assert!(stdout.ends_with("}\n"));
}

#[test]
fn cli_does_not_publish_partial_trees_after_decoding_failure() {
    let output = run(b"a=ok&bad%00name=value");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Malformed"));
}

#[test]
fn cli_php_max_input_vars_counts_ignored_names_but_not_blank_pairs() {
    let cases: &[(&[&str], &[u8], bool)] = &[
        (&["--php-max-input-vars", "2"], b"a=1&=ignored&b=2", false),
        (&["--php-max-input-vars", "2"], b"a=1&&b=2", true),
        (
            &["--php-max-input-vars", "2", "--php-nul-name-truncation"],
            b"a=1&%00ignored=2&b=3",
            false,
        ),
        (
            &["--php-max-input-vars", "2", "--php-arg-separators", "|"],
            b"a=1|=ignored|b=2",
            false,
        ),
    ];
    for (args, body, includes_b) in cases {
        let output = run_with_args(args, body);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(r#"["s","61"],["s","31"]"#), "{args:?}");
        assert_eq!(
            stdout.contains(r#"["s","62"],["s","32"]"#),
            *includes_b,
            "{args:?}"
        );
    }
}
