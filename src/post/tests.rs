use super::*;
use crate::{Limits as WireLimits, Parser};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

struct Spool(PathBuf);
impl Spool {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "php-post-test-{}-{}",
            std::process::id(),
            crate::NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Spool {
    fn drop(&mut self) {
        assert_eq!(fs::read_dir(&self.0).unwrap().count(), 0);
        fs::remove_dir(&self.0).unwrap();
    }
}

fn request(spool: &Spool, fields: &[(&[u8], &[u8])], file: bool) -> Request {
    let mut bytes = Vec::new();
    if file {
        bytes.extend_from_slice(
            b"--b\r\nContent-Disposition: form-data; name=f; filename=x\r\n\r\nfile\r\n",
        );
    }
    for (name, value) in fields {
        bytes.extend_from_slice(b"--b\r\nContent-Disposition: form-data; name=\"");
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(b"\"\r\n\r\n");
        bytes.extend_from_slice(value);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"--b--\r\n");
    let mut parser = Parser::new(b"b", WireLimits::default(), &spool.0).unwrap();
    parser.feed(&bytes).unwrap();
    parser.finish().unwrap()
}

fn json(request: &PostRequest) -> String {
    let mut output = Vec::new();
    request.write_json(&mut output).unwrap();
    String::from_utf8(output).unwrap()
}

#[test]
fn ordered_tree_normalizes_keys_preserves_binary_and_replaces_in_place() {
    let spool = Spool::new();
    let post = request(
        &spool,
        &[
            (b" a.b[x.y ]", b"\0\xff"),
            (b"other", b"a"),
            (b"a_b[x.y ]", b"last"),
        ],
        false,
    )
    .into_post(Limits::default())
    .unwrap();
    assert_eq!(
        json(&post),
        r#"["a",[[["s","615f62"],["a",[[["s","782e7920"],["s","6c617374"]]]]],[["s","6f74686572"],["s","61"]]]]"#
    );
    assert_eq!(post.stats().nodes, 4);
    // The field bytes refer to their original ranges, including replaced data.
    assert_eq!(
        post.wire().parts().next().unwrap().value,
        Some(b"\0\xff".as_slice())
    );
}

#[test]
fn bracket_index_whitespace_matches_php_84() {
    let cases: &[(&[u8], Option<&[u8]>)] = &[
        (b"a[ x]", Some(b" x")),
        (b"a[\tx]", Some(b"\tx")),
        (b"a[\nx]", Some(b"\nx")),
        (b"a[  ]", Some(b"  ")),
        (b"a[ \t]", Some(b" \t")),
        (b"a[]", None),
        (b"a[ ]", None),
        (b"a[\t]", None),
        (b"a[\n]", None),
        (b"a[\r]", None),
        (b"a[\x0b]", None),
        (b"a[\x0c]", None),
    ];
    for &(raw, expected) in cases {
        let mut scratch = Vec::new();
        let mut path = [Segment::Append; MAX_DEPTH + 1];
        let count = normalize(raw, &mut scratch, &mut path, Limits::default(), false).unwrap();
        assert_eq!(count, 2, "name={raw:?}");
        match (path[1], expected) {
            (Segment::Append, None) => {}
            (Segment::Key(span), Some(key)) => assert_eq!(span.bytes(&scratch), key),
            _ => panic!("incorrect PHP index for {raw:?}"),
        }
    }

    // Check the complete ordered tagged tree, not just the index lexer.
    let spool = Spool::new();
    let post = request(
        &spool,
        &[(b"a[ x]", b"v"), (b"a[  ]", b"w"), (b"a[ ]", b"z")],
        false,
    )
    .into_post(Limits::default())
    .unwrap();
    assert_eq!(
        json(&post),
        r#"["a",[[["s","61"],["a",[[["s","2078"],["s","76"]],[["s","2020"],["s","77"]],[["i","0"],["s","7a"]]]]]]]"#
    );
}

#[test]
fn php_nesting_limit_skips_overdeep_fields_and_counts_them_toward_input_vars() {
    let spool = Spool::new();
    let fields: &[(&[u8], &[u8])] = &[
        (b"a[x][y]", b"too-deep"),
        (b"ok", b"1"),
        (b"a[x]", b"2"),
        (b"tail", b"3"),
    ];
    let post = request(&spool, fields, false)
        .into_post_with_php_limits(Limits::default(), 3, 1)
        .unwrap();
    assert_eq!(post.stats().skipped_fields, 2);
    assert_eq!(post.stats().fields, 2);
    assert_eq!(
        json(&post),
        r#"["a",[[["s","6f6b"],["s","31"]],[["s","61"],["a",[[["s","78"],["s","32"]]]]]]]"#
    );

    // A zero nesting limit drops bracketed names but keeps plain fields.
    let zero = request(&spool, &[(b"a[x]", b"1"), (b"ok", b"2")], false)
        .into_post_with_php_limits(Limits::default(), usize::MAX, 0)
        .unwrap();
    assert_eq!(json(&zero), r#"["a",[[["s","6f6b"],["s","32"]]]]"#);
    assert_eq!(zero.stats().skipped_fields, 1);

    // Strict callers still reject over-depth names atomically.
    let mut strict = Limits::default();
    strict.depth = 1;
    assert_eq!(
        request(&spool, fields, false)
            .into_post(strict)
            .unwrap_err(),
        Error::Depth
    );
}

#[test]
fn numeric_keys_follow_signed_64_bit_php_rules() {
    for (input, expected) in [
        ("0", Some(0)),
        ("-1", Some(-1)),
        ("00", None),
        ("-0", None),
        ("+1", None),
        ("01", None),
        ("1.0", None),
        ("1e2", None),
        ("9223372036854775807", Some(i64::MAX)),
        ("-9223372036854775808", Some(i64::MIN)),
        ("9223372036854775808", None),
        ("-9223372036854775809", None),
        ("", None),
    ] {
        assert_eq!(numeric(input.as_bytes()), expected, "{input}");
    }
    let spool = Spool::new();
    let post = request(&spool, &[(b"a[-5]", b"x"), (b"a[]", b"y")], false)
        .into_post(Limits::default())
        .unwrap();
    assert_eq!(
        json(&post),
        r#"["a",[[["s","61"],["a",[[["i","-5"],["s","78"]],[["i","-4"],["s","79"]]]]]]]"#
    );
}

#[test]
fn detached_nodes_remain_charged_and_scalar_array_resets_append() {
    let spool = Spool::new();
    let parts: &[(&[u8], &[u8])] = &[(b"a[x]", b"old"), (b"a", b"scalar"), (b"a[]", b"new")];
    let post = request(&spool, parts, false)
        .into_post(Limits::default())
        .unwrap();
    assert_eq!(post.stats().nodes, 4); // root, a, abandoned x, new integer child
    assert_eq!(
        json(&post),
        r#"["a",[[["s","61"],["a",[[["i","0"],["s","6e6577"]]]]]]]"#
    );
    assert!(matches!(
        request(&spool, parts, false).into_post(Limits {
            nodes: 3,
            ..Limits::default()
        }),
        Err(Error::Nodes)
    ));
}

#[test]
fn exact_and_plus_one_adapter_budgets() {
    let spool = Spool::new();
    let fields: &[(&[u8], &[u8])] = &[(b"a[x]", b"hello"), (b"b", b"world")];
    let baseline = request(&spool, fields, false)
        .into_post(Limits::default())
        .unwrap()
        .stats();
    type Case = (Error, fn(&mut Limits, usize), usize);
    let cases: &[Case] = &[
        (Error::Depth, |l, n| l.depth = n, 1),
        (Error::NameBytes, |l, n| l.name_bytes = n, 4),
        (Error::Nodes, |l, n| l.nodes = n, baseline.nodes),
        (Error::KeyBytes, |l, n| l.key_bytes = n, baseline.key_bytes),
        (
            Error::Work,
            |l, n| l.work = n as u64,
            baseline.work as usize,
        ),
        (
            Error::MemoryCapacity,
            |l, n| l.memory_capacity = n,
            baseline.memory_capacity,
        ),
    ];
    for &(error, set, exact) in cases {
        let mut limits = Limits::default();
        set(&mut limits, exact);
        assert!(
            request(&spool, fields, false).into_post(limits).is_ok(),
            "exact {error:?}"
        );
        set(&mut limits, exact - 1);
        assert!(
            matches!(request(&spool, fields, false).into_post(limits), Err(found) if found == error),
            "plus one {error:?}"
        );
    }
    let limits = Limits {
        protected_names: 2,
        ..Limits::default()
    };
    assert!(request(&spool, &[], true).into_post(limits).is_ok());
    assert!(matches!(
        request(&spool, &[], true).into_post(Limits {
            protected_names: 1,
            ..limits
        }),
        Err(Error::ProtectedNames)
    ));
}

#[test]
fn unterminated_brackets_follow_php_84_field_registration() {
    // These are byte-for-byte tagged trees from PHP 8.4 parse_str().
    // A missing first closing bracket produces a flat key; a missing
    // later closing bracket keeps the last complete nested path.
    let spool = Spool::new();
    let cases: &[(&[u8], &str)] = &[
        (b"a[", r#"["a",[[["s","615f"],["s","76"]]]]"#),
        (b"a[x", r#"["a",[[["s","615f78"],["s","76"]]]]"#),
        (b"a[ x", r#"["a",[[["s","615f5f78"],["s","76"]]]]"#),
        (
            b"a[foo.bar[",
            r#"["a",[[["s","615f666f6f5f6261725f"],["s","76"]]]]"#,
        ),
        (b"a[foo[[", r#"["a",[[["s","615f666f6f5f5f"],["s","76"]]]]"#),
        (
            b"a[x][",
            r#"["a",[[["s","61"],["a",[[["s","78"],["s","76"]]]]]]]"#,
        ),
        (
            b"a[x][y",
            r#"["a",[[["s","61"],["a",[[["s","78"],["s","76"]]]]]]]"#,
        ),
        (
            b"a[x][y][",
            r#"["a",[[["s","61"],["a",[[["s","78"],["a",[[["s","79"],["s","76"]]]]]]]]]]"#,
        ),
    ];
    for &(name, expected) in cases {
        let post = request(&spool, &[(name, b"v")], false)
            .into_post(Limits::default())
            .unwrap();
        assert_eq!(json(&post), expected, "name={name:?}");
    }
}

#[test]
fn depth_append_and_configuration_failures_are_explicit() {
    let spool = Spool::new();
    let fields: &[(&[u8], &[u8])] = &[(b"a[9223372036854775807]", b"x"), (b"a[]", b"y")];
    assert!(matches!(
        request(&spool, fields, false).into_post(Limits::default()),
        Err(Error::AppendExhausted)
    ));
    for limits in [
        Limits {
            depth: 65,
            ..Limits::default()
        },
        Limits {
            name_bytes: MAX_NAME + 1,
            ..Limits::default()
        },
        Limits {
            nodes: usize::MAX,
            ..Limits::default()
        },
    ] {
        assert!(matches!(
            request(&spool, &[], false).into_post(limits),
            Err(Error::InvalidLimits)
        ));
    }
    let limits = Limits {
        nodes: 1,
        key_bytes: isize::MAX as usize + 1,
        memory_capacity: usize::MAX,
        ..Limits::default()
    };
    assert!(matches!(
        request(&spool, &[], false).into_post(limits),
        Err(Error::Allocation)
    ));
}

#[test]
fn deepest_tree_serializes_and_drops_without_recursive_ownership() {
    let spool = Spool::new();
    let name = [b"a".as_slice(), b"[x]".repeat(64).as_slice()].concat();
    let post = request(&spool, &[(&name, b"\0\xff")], false)
        .into_post(Limits::default())
        .unwrap();
    assert_eq!(post.stats().nodes, 66);
    assert!(json(&post).contains("00ff"));
    drop(post);
    let too_deep = [name.as_slice(), b"[x]"].concat();
    assert!(matches!(
        request(&spool, &[(&too_deep, b"v")], false).into_post(Limits::default()),
        Err(Error::Depth)
    ));
}

#[test]
fn upload_protection_and_ownership_are_separate_from_files_materialization() {
    let spool = Spool::new();
    let mut post = request(
        &spool,
        &[
            (b"f", b"blocked"),
            (b"f[tmp_name]", b"blocked"),
            (b"g", b"ok"),
        ],
        true,
    )
    .into_post(Limits::default())
    .unwrap();
    assert_eq!(post.stats().skipped_fields, 2);
    assert_eq!(post.stats().protected_names, 2);
    assert_eq!(json(&post), r#"["a",[[["s","67"],["s","6f6b"]]]]"#);
    let mut data = Vec::new();
    assert_eq!(post.upload_mut(0).unwrap().copy_to(&mut data).unwrap(), 4);
    assert_eq!(data, b"file");
}

#[test]
fn adapter_failure_releases_owned_uploads_in_subprocess() {
    const CHILD: &str = "PHP_POST_FD_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "post::tests::adapter_failure_releases_owned_uploads_in_subprocess",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let spool = Spool::new();
    let directory = if Path::new("/proc/self/fd").is_dir() {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let count = || fs::read_dir(directory).unwrap().count();
    let baseline = count();
    for _ in 0..64 {
        for limits in [
            Limits {
                nodes: 0,
                ..Limits::default()
            },
            Limits {
                nodes: 1,
                ..Limits::default()
            },
            Limits {
                key_bytes: 0,
                ..Limits::default()
            },
            Limits {
                protected_names: 1,
                ..Limits::default()
            },
            Limits {
                work: 0,
                ..Limits::default()
            },
            Limits {
                memory_capacity: 0,
                ..Limits::default()
            },
            Limits {
                nodes: 1,
                key_bytes: isize::MAX as usize + 1,
                memory_capacity: usize::MAX,
                ..Limits::default()
            },
        ] {
            let wire = request(&spool, &[(b"g[x]", b"v")], true);
            assert_eq!(count(), baseline + 1);
            assert!(wire.into_post(limits).is_err());
            assert_eq!(count(), baseline);
        }
        let wire = request(&spool, &[(b"g[", b"v")], true);
        let post = wire.into_post(Limits::default()).unwrap();
        assert_eq!(count(), baseline + 1);
        assert_eq!(json(&post), r#"["a",[[["s","675f"],["s","76"]]]]"#);
        drop(post);
        assert_eq!(count(), baseline);
        let post = request(&spool, &[], true)
            .into_post(Limits::default())
            .unwrap();
        assert_eq!(count(), baseline + 1);
        drop(post);
        assert_eq!(count(), baseline);
    }
}

#[test]
fn php_max_input_vars_skips_excess_fields_but_keeps_uploads() {
    let spool = Spool::new();
    let fields: &[(&[u8], &[u8])] = &[(b"a", b"1"), (b"b", b"2"), (b"c", b"3")];
    let expected = [
        r#"["a",[]]"#,
        r#"["a",[[["s","61"],["s","31"]]]]"#,
        r#"["a",[[["s","61"],["s","31"]],[["s","62"],["s","32"]]]]"#,
        r#"["a",[[["s","61"],["s","31"]],[["s","62"],["s","32"]],[["s","63"],["s","33"]]]]"#,
    ];
    for max in 0..=4 {
        let mut post = request(&spool, fields, true)
            .into_post_with_max_input_vars(Limits::default(), max)
            .unwrap();
        assert_eq!(json(&post), expected[max.min(3)], "max_input_vars={max}");
        assert_eq!(post.stats().fields, max.min(3));
        assert_eq!(post.stats().skipped_fields, 3 - max.min(3));
        let mut upload = Vec::new();
        assert_eq!(post.upload_mut(0).unwrap().copy_to(&mut upload).unwrap(), 4);
        assert_eq!(upload, b"file");
    }
    // Strict mode is unchanged and does not silently discard variables.
    let post = request(&spool, fields, false)
        .into_post(Limits::default())
        .unwrap();
    assert_eq!(json(&post), expected[3]);
}

#[test]
fn php_max_input_vars_discards_overflow_before_adapter_depth_checks() {
    let spool = Spool::new();
    let fields: &[(&[u8], &[u8])] = &[(b"ok", b"1"), (b"deep[x][y]", b"2")];
    let limits = Limits {
        depth: 1,
        ..Limits::default()
    };
    assert!(matches!(
        request(&spool, fields, false).into_post(limits),
        Err(Error::Depth)
    ));
    let post = request(&spool, fields, false)
        .into_post_with_max_input_vars(limits, 1)
        .unwrap();
    assert_eq!(json(&post), r#"["a",[[["s","6f6b"],["s","31"]]]]"#);
    assert_eq!(post.stats().skipped_fields, 1);
}
