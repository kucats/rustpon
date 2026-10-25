use std::io::Write;
use std::process::{Command, Stdio};

fn run(body: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_urlencoded"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body).unwrap();
    child.wait_with_output().unwrap()
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
