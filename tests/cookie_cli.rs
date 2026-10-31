use std::io::Write;
use std::process::{Command, Stdio};

fn run(header: &[u8]) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cookie"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(header).unwrap();
    let result = child.wait_with_output().unwrap();
    (
        result.status.success(),
        String::from_utf8(result.stdout).unwrap(),
        String::from_utf8(result.stderr).unwrap(),
    )
}

#[test]
fn cookie_cli_emits_binary_safe_first_wins_projection() {
    let (ok, stdout, stderr) = run(b"a.b=first; a_b=second; p=%00%FF+");
    assert!(ok, "{stderr}");
    assert_eq!(
        stdout,
        "{\"cookie_pairs\":[[\"615f62\",\"6669727374\"],[\"70\",\"00ff2b\"]]}\n"
    );
}

#[test]
fn cookie_cli_rejects_nested_names_without_partial_json() {
    let (ok, stdout, stderr) = run(b"a=1; nested[x]=2");
    assert!(!ok);
    assert!(stdout.is_empty());
    assert!(stderr.contains("UnsupportedArrayName"), "{stderr}");
}

#[test]
fn cookie_cli_caps_the_header_before_parsing() {
    let oversized = vec![b'a'; 64 * 1024 + 1];
    let (ok, stdout, stderr) = run(&oversized);
    assert!(!ok);
    assert!(stdout.is_empty());
    assert!(stderr.contains("HeaderBytes"), "{stderr}");
}

fn run_array(header: &[u8]) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cookie"))
        .arg("--php-arrays")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(header).unwrap();
    let result = child.wait_with_output().unwrap();
    (
        result.status.success(),
        String::from_utf8(result.stdout).unwrap(),
        String::from_utf8(result.stderr).unwrap(),
    )
}

#[test]
fn cookie_cli_one_level_array_is_binary_safe() {
    let (ok, stdout, stderr) = run_array(b"a[x]=%00%FF; a[x]=%2B");
    assert!(ok, "{stderr}");
    assert_eq!(
        stdout,
        "{\"cookie_tree\":[\"a\",[[[\"s\",\"61\"],[\"a\",[[[\"s\",\"78\"],[\"s\",\"2b\"]]]]]]]}\n"
    );
}

#[test]
fn cookie_cli_five_level_array_promotes_scalar_root() {
    let (ok, stdout, stderr) = run_array(b"a=first; a[x][y][z][w][v]=second");
    assert!(ok, "{stderr}");
    assert!(stdout.contains(r#"["s","7365636f6e64"]"#));
    assert!(!stdout.contains(r#"["s","6669727374"]"#));
}

#[test]
fn cookie_cli_two_level_array_emits_typed_binary_json() {
    let (ok, stdout, stderr) = run_array(b"a[x][y]=%00%FF; a[x][y]=last; a[0][]=tail");
    assert!(ok, "{stderr}");
    assert!(stdout.contains(r#"["s","6c617374"]"#));
    assert!(stdout.contains(r#"["s","7461696c"]"#));
    assert!(!stdout.contains(r#"["s","00ff"]"#));
}

fn run_limited(header: &[u8], array_mode: bool, limit: &str) -> (bool, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cookie"));
    if array_mode {
        command.arg("--php-arrays");
    }
    let mut child = command
        .args(["--php-max-input-vars", limit])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(header).unwrap();
    let result = child.wait_with_output().unwrap();
    (
        result.status.success(),
        String::from_utf8(result.stdout).unwrap(),
        String::from_utf8(result.stderr).unwrap(),
    )
}

#[test]
fn cookie_cli_php_variable_quota_counts_duplicates() {
    let (ok, stdout, stderr) = run_limited(b"a=1; a=2; b=3", false, "2");
    assert!(ok, "{stderr}");
    assert_eq!(stdout, concat!(r#"{"cookie_pairs":[["61","31"]]}"#, "\n"));
    let (ok, stdout, stderr) = run_limited(b"a=1", false, "0");
    assert!(ok, "{stderr}");
    assert_eq!(stdout, concat!(r#"{"cookie_pairs":[]}"#, "\n"));
}

#[test]
fn cookie_cli_php_variable_quota_bounds_array_projection() {
    let (ok, stdout, stderr) = run_limited(b"a[x]=1; a[y]=2; a[z]=3", true, "2");
    assert!(ok, "{stderr}");
    assert!(stdout.contains(r#"["s","78"]"#));
    assert!(stdout.contains(r#"["s","79"]"#));
    assert!(!stdout.contains(r#"["s","7a"]"#));
    let (ok, stdout, stderr) = run_limited(b"a[x]=1", true, "0");
    assert!(ok, "{stderr}");
    assert_eq!(stdout, concat!(r#"{"cookie_tree":["a",[]]}"#, "\n"));
}

#[test]
fn cookie_cli_three_level_arrays_preserve_last_wins_and_binary_data() {
    let (ok, stdout, stderr) = run_array(b"a[x][y][z]=%00%FF; a[x][y][z]=last; a[x][y][]=tail");
    assert!(ok, "{stderr}");
    assert!(stdout.contains(r#"["s","6c617374"]"#));
    assert!(stdout.contains(r#"["s","7461696c"]"#));
    assert!(!stdout.contains(r#"["s","00ff"]"#));
}

#[test]
fn cookie_cli_four_level_arrays_preserve_binary_last_wins_and_append() {
    let (ok, stdout, stderr) =
        run_array(b"a[x][y][z][w]=%00%FF; a[x][y][z][w]=last; a[x][y][z][]=tail");
    assert!(ok, "{stderr}");
    assert!(stdout.contains(r#"["s","6c617374"]"#));
    assert!(stdout.contains(r#"["s","7461696c"]"#));
    assert!(!stdout.contains(r#"["s","00ff"]"#));
}

#[test]
fn cookie_cli_trailing_unclosed_fifth_bracket_replaces_complete_leaf() {
    let (ok, stdout, stderr) = run_array(b"a[x][y][z][w]=ok; a[x][y][z][w][v=bad");
    assert!(ok, "{stderr}");
    assert!(stdout.contains(r#"["s","626164"]"#));
    assert!(!stdout.contains(r#"["s","6f6b"]"#));
}

#[test]
fn cookie_cli_noncontiguous_brackets_use_only_the_complete_path() {
    let (ok, stdout, stderr) =
        run_array(b"a[x]noise[y]=first; a[x]=second; a[x][=third; b=ok");
    assert!(ok, "{stderr}");
    assert!(stdout.contains(r#"["s","7468697264"]"#));
    assert!(!stdout.contains(r#"["s","6669727374"]"#));
    assert!(!stdout.contains(r#"["s","7365636f6e64"]"#));
    assert!(stdout.contains(r#"["s","62"],["s","6f6b"]"#));
}

#[test]
fn cookie_cli_64_level_boundary_and_65_level_root_reset() {
    let accepted = format!("a{}=at_limit", "[x]".repeat(64));
    let (ok, stdout, stderr) = run_array(accepted.as_bytes());
    assert!(ok, "{stderr}");
    assert!(stdout.contains(r#"["s","61745f6c696d6974"]"#));

    let overflow = format!("a[x]=old; a{}=ignored; b=ok", "[x]".repeat(65));
    let (ok, stdout, stderr) = run_array(overflow.as_bytes());
    assert!(ok, "{stderr}");
    assert!(stdout.contains(r#"["s","62"],["s","6f6b"]"#));
    assert!(!stdout.contains(r#"["s","61"]"#));
}

fn run_nesting(header: &[u8], level: &str, max_vars: Option<&str>) -> (bool, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cookie"));
    command.args(["--php-arrays", "--php-max-input-nesting-level", level]);
    if let Some(max_vars) = max_vars {
        command.args(["--php-max-input-vars", max_vars]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(header).unwrap();
    let result = child.wait_with_output().unwrap();
    (
        result.status.success(),
        String::from_utf8(result.stdout).unwrap(),
        String::from_utf8(result.stderr).unwrap(),
    )
}

#[test]
fn cookie_cli_configurable_nesting_zero_and_overflow_reset() {
    let (ok, stdout, stderr) = run_nesting(b"a=old; a[x]=discarded; b=ok", "0", None);
    assert!(ok, "{stderr}");
    assert_eq!(
        stdout,
        "{\"cookie_tree\":[\"a\",[[[\"s\",\"62\"],[\"s\",\"6f6b\"]]]]}\n"
    );

    let (ok, stdout, stderr) =
        run_nesting(b"a[x]=old; a[x][y]=discarded; b=too_late", "1", Some("2"));
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "{\"cookie_tree\":[\"a\",[]]}\n");

    let (ok, stdout, stderr) = run_nesting(b"a[x][y]=accepted", "2", None);
    assert!(ok, "{stderr}");
    assert!(stdout.contains(r#"["s","6163636570746564"]"#));
}

#[test]
fn cookie_cli_rejects_unimplemented_ini_nesting_above_hard_limit() {
    let (ok, stdout, stderr) = run_nesting(b"a[x]=value", "129", None);
    assert!(!ok);
    assert!(stdout.is_empty());
    assert!(stderr.contains("NestingLevel"), "{stderr}");
}

#[test]
fn cookie_cli_configured_128_level_depth_and_129_guard() {
    let boundary = format!("a{}=accepted", "[x]".repeat(128));
    let (ok, stdout, stderr) = run_nesting(boundary.as_bytes(), "128", None);
    assert!(ok, "{stderr}");
    assert!(stdout.contains(r#"["s","6163636570746564"]"#));

    let reset = format!("a=old; a{}=ignored; b=kept", "[x]".repeat(129));
    let (ok, stdout, stderr) = run_nesting(reset.as_bytes(), "128", None);
    assert!(ok, "{stderr}");
    assert!(!stdout.contains(r#"["s","61"]"#));
    assert!(stdout.contains(r#"["s","62"]"#));

    let (ok, stdout, stderr) = run_nesting(b"a[x]=value", "129", None);
    assert!(!ok);
    assert!(stdout.is_empty());
    assert!(stderr.contains("NestingLevel"), "{stderr}");
}
