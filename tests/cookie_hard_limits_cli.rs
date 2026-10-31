//! Standalone Cookie CLI hard-budget admission regressions.
use std::io::Write;
use std::process::{Command, Stdio};

fn run(args: &[&str], header: &[u8]) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cookie"))
        .args(args)
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
fn every_configurable_hard_budget_accepts_exact_boundary_and_rejects_one_less() {
    for arrays in [false, true] {
        let header: &[u8] = if arrays { b"a[x]=1" } else { b"a=1" };
        let name_len = if arrays { 4 } else { 1 };
        let budgets = [
            ("--max-cookie-header-bytes", header.len(), "HeaderBytes"),
            ("--max-cookie-pairs", 1, "Pairs"),
            ("--max-cookie-name-bytes", name_len, "NameBytes"),
            ("--max-cookie-value-bytes", 1, "ValueBytes"),
            ("--max-cookie-retained-bytes", name_len + 1, "RetainedBytes"),
        ];
        for (flag, at_boundary, error) in budgets {
            let limit = at_boundary.to_string();
            let mut args = Vec::new();
            if arrays {
                args.push("--php-arrays");
            }
            args.extend([flag, limit.as_str()]);
            let (ok, stdout, stderr) = run(&args, header);
            assert!(ok, "{flag} ({limit}) in array mode {arrays}: {stderr}");
            assert!(!stdout.is_empty());

            let below = (at_boundary - 1).to_string();
            args.pop();
            args.push(below.as_str());
            let (ok, stdout, stderr) = run(&args, header);
            assert!(!ok, "{flag} ({below}) in array mode {arrays}");
            assert!(stdout.is_empty(), "no partially published Cookie JSON");
            assert!(stderr.contains(error), "{flag}: {stderr}");
        }
    }
}

#[test]
fn hard_limits_reject_skipped_php_variables_and_empty_segment_floods() {
    for arrays in [false, true] {
        let mut mode = Vec::new();
        if arrays {
            mode.push("--php-arrays");
        }
        for (flag, expected) in [
            ("--max-cookie-pairs", "Pairs"),
            ("--max-cookie-name-bytes", "NameBytes"),
            ("--max-cookie-value-bytes", "ValueBytes"),
        ] {
            let mut args = mode.clone();
            args.extend(["--php-max-input-vars", "0", flag, "0"]);
            let (ok, stdout, stderr) = run(&args, b"a=1");
            assert!(!ok, "{flag} must not be bypassed by the PHP quota");
            assert!(stdout.is_empty());
            assert!(stderr.contains(expected), "{stderr}");
        }

        let mut args = mode.clone();
        args.extend(["--php-max-input-vars", "0", "--max-cookie-pairs", "2"]);
        let (ok, stdout, stderr) = run(&args, b";; a=1");
        assert!(!ok);
        assert!(stdout.is_empty());
        assert!(stderr.contains("Pairs"), "{stderr}");

        let mut args = mode;
        args.extend(["--max-cookie-pairs", "0"]);
        let (ok, stdout, stderr) = run(&args, b"");
        assert!(ok, "{stderr}");
        assert!(!stdout.is_empty());
    }
}

#[test]
fn hard_limit_configuration_is_tighten_only_and_never_partially_publishes() {
    for (flag, over_default) in [
        ("--max-cookie-header-bytes", "65537"),
        ("--max-cookie-pairs", "129"),
        ("--max-cookie-name-bytes", "1025"),
        ("--max-cookie-value-bytes", "8193"),
        ("--max-cookie-retained-bytes", "65537"),
    ] {
        for args in [
            vec![flag, over_default],
            vec![flag, "1", flag, "1"],
            vec![flag, "not-a-number"],
            vec![flag, "-1"],
        ] {
            let (ok, stdout, stderr) = run(&args, b"a=1");
            assert!(!ok, "{args:?}");
            assert!(stdout.is_empty(), "{args:?}");
            assert!(stderr.contains("usage: cookie"), "{args:?}: {stderr}");
        }
    }

    let (ok, stdout, stderr) = run(&["--max-cookie-header-bytes", "0"], b"a=1");
    assert!(!ok);
    assert!(stdout.is_empty());
    assert!(stderr.contains("HeaderBytes"), "{stderr}");
}
