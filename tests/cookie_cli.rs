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
fn cookie_cli_nested_array_fails_without_json() {
    let (ok, stdout, stderr) = run_array(b"a=first; a[x][y]=second");
    assert!(!ok);
    assert!(stdout.is_empty());
    assert!(stderr.contains("UnsupportedArrayName"), "{stderr}");
}
