//! Exercise the real command's AST selection, JSON channels and failure status.
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("verbose-boolean-cli-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("boolean_guards.intent"),
            include_str!("../examples/boolean_guards.intent"),
        )
        .unwrap();
        let fixture = Self(dir);
        fs::write(
            fixture.0.join("case.verbose"),
            include_str!("../examples/boolean_guards.verbose"),
        )
        .unwrap();
        fixture
    }

    fn expression(&self, expr: &str, kind: &str, lets: &str) {
        let original = include_str!("../examples/boolean_guards.verbose");
        let body = format!("{lets}    out = {expr}");
        let reads = ["i.n", "i.s"]
            .into_iter()
            .filter(|r| body.contains(r))
            .collect::<Vec<_>>()
            .join(", ");
        let src = original.replace("out : number", &format!("out : {kind}"))
            .replace("    out = if i.n >= 0 and i.n < length(i.s) and byte_at(i.s, i.n) > 0 then 1 else 0", &body)
            .replace("reads: [i.n, i.s]", &format!("reads: [{reads}]"));
        fs::write(self.0.join("case.verbose"), src).unwrap();
    }

    fn run(&self, flags: &[&str], input: Option<&str>) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_verbosec"))
            .arg(self.0.join("case.verbose"))
            .args(["--run", "guarded_byte"])
            .args(flags)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn boolean_cli_guards_empty_text_and_extreme_indices() {
    let f = Fixture::new("guards");
    let input = r#"[{"s":"","n":0},{"s":"é🦀","n":0},{"s":"x","n":1},{"s":"x","n":-1},{"s":"x","n":9223372036854775807},{"s":"x","n":-9223372036854775808}]"#;
    let expected = b"[{\"out\":0},{\"out\":1},{\"out\":0},{\"out\":0},{\"out\":0},{\"out\":0}]\n";
    let out = f.run(&["--stdin", "--json"], Some(input));
    assert_eq!(
        (out.status.code(), out.stdout, out.stderr),
        (Some(0), expected.to_vec(), vec![])
    );
    let path = f.0.join("input.json");
    fs::write(&path, input).unwrap();
    let out = f.run(&["--input", path.to_str().unwrap(), "--json"], None);
    assert_eq!(
        (out.status.code(), out.stdout, out.stderr),
        (Some(0), expected.to_vec(), vec![])
    );
}

#[test]
fn boolean_cli_keeps_constant_booleans_typed() {
    let f = Fixture::new("constants");
    for (expr, expected) in [
        ("1 == 1 or byte_at(i.s, 0) > 0", true),
        ("1 == 2 and byte_at(i.s, 0) > 0", false),
        ("starts_with(\"abc\", \"a\") or byte_at(i.s, 0) > 0", true),
        ("not (1 == 1 and 1 != 2)", false),
    ] {
        f.expression(expr, "bool", "");
        let out = f.run(&["--stdin", "--json"], Some(r#"[{"s":"","n":0}]"#));
        assert_eq!(
            (out.status.code(), out.stdout, out.stderr),
            (
                Some(0),
                format!("[{{\"out\":{expected}}}]\n").into_bytes(),
                vec![]
            ),
            "{expr}"
        );
    }
    let out = f.run(&["--stdin", "--stats"], Some(r#"[{"s":"","n":0}]"#));
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("optimizations: skipped for source interpretation"));
}

#[test]
fn boolean_cli_preserves_required_errors_and_eager_lets() {
    let f = Fixture::new("errors");
    for (expr, lets) in [
        ("i.n == 0 and byte_at(i.s, 0) > 0", ""),
        ("i.n != 0 or byte_at(i.s, 0) > 0", ""),
        ("byte_at(i.s, 0) > 0 or i.n == 0", ""),
        ("1 == 1 or first > 0", "    let first = byte_at(i.s, 0)\n"),
    ] {
        f.expression(expr, "bool", lets);
        let out = f.run(&["--stdin", "--json"], Some(r#"[{"s":"","n":0}]"#));
        assert_eq!((out.status.code(), out.stdout), (Some(1), vec![]), "{expr}");
        assert!(String::from_utf8(out.stderr)
            .unwrap()
            .contains("byte_at index out of range"));
    }
    // Optimization must not inspect an unselected arithmetic trap either.
    f.expression("1 == 1 or (-9223372036854775807 - 1) / -1 > 0", "bool", "");
    let out = f.run(&["--stdin", "--json"], Some(r#"[{"s":"","n":0}]"#));
    assert_eq!(
        (out.status.code(), out.stdout, out.stderr),
        (Some(0), b"[{\"out\":true}]\n".to_vec(), vec![])
    );
}

#[test]
fn boolean_cli_still_verifies_skipped_operands() {
    let f = Fixture::new("verification");
    f.expression("1 == 1 or byte_at(i.s, 0) > 0", "bool", "");
    let path = f.0.join("case.verbose");
    let src = fs::read_to_string(&path)
        .unwrap()
        .replace("reads: [i.s]", "reads: []");
    fs::write(path, src).unwrap();
    let out = f.run(&["--stdin", "--json"], Some(r#"[{"s":"","n":0}]"#));
    assert!(!out.status.success());
    assert!(!out.stderr.is_empty());
}

#[test]
fn boolean_cli_reaction_uses_the_same_source_evaluation() {
    let f = Fixture::new("reaction");
    f.expression("1 == 1 or byte_at(i.s, 0) > 0", "bool", "");
    let path = f.0.join("case.verbose");
    let mut src = fs::read_to_string(&path).unwrap();
    src.push_str("\nreaction notify\n  @intention: \"Publish a guarded trigger\"\n  @source: boolean_guards.intent:3\n  trigger: guarded_byte\n  effects:\n    print \"guard passed\"\n");
    fs::write(&path, src).unwrap();
    let input = f.0.join("input.json");
    fs::write(&input, r#"[{"s":"","n":0}]"#).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_verbosec"))
        .arg(path)
        .args(["--run", "notify", "--input"])
        .arg(input)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("EFFECT print: guard passed"));
}

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn boolean_cli_artifact_modes_keep_optimization() {
    let f = Fixture::new("artifacts");
    f.expression("if 2 > 1 then 7 else 9", "number", "");
    let native = f.0.join("native");
    let wasm = f.0.join("module.wasm");
    for flags in [
        vec!["--native", native.to_str().unwrap(), "--stats"],
        vec!["--wasm", wasm.to_str().unwrap(), "--stats"],
        vec!["--benchmark", "--stats"],
        vec!["--disasm", "--stats"],
    ] {
        let out = f.run(&flags, None);
        assert_eq!(out.status.code(), Some(0), "{flags:?}: {:?}", out.stderr);
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(stdout.contains("optimizations:\n"), "{stdout}");
        assert!(!stdout.contains("skipped for source interpretation"));
    }
    let out = Command::new(native).args(["", "0"]).output().unwrap();
    assert_eq!(
        (out.status.code(), out.stdout, out.stderr),
        (Some(0), b"7\n".to_vec(), vec![])
    );
    assert_eq!(&fs::read(wasm).unwrap()[..4], b"\0asm");
}

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn constant_folding_cli_compiles_guarded_traps_in_artifact_modes() {
    let f = Fixture::new("folding-guards");
    let native = f.0.join("native");
    let wasm = f.0.join("module.wasm");
    for failure in [
        "1 / 0",
        "1 % 0",
        "(-9223372036854775807 - 1) / -1",
        "(-9223372036854775807 - 1) % -1",
    ] {
        f.expression(
            &format!("if 1 == 1 or ({failure}) > 0 then 7 else 9"),
            "number",
            "",
        );
        for flags in [
            vec!["--native", native.to_str().unwrap()],
            vec!["--wasm", wasm.to_str().unwrap()],
            vec!["--disasm"],
        ] {
            let out = f.run(&flags, None);
            assert!(out.status.success(), "{failure}, {flags:?}: {out:?}");
            assert!(out.stderr.is_empty(), "{out:?}");
        }
        let out = Command::new(&native).args(["", "0"]).output().unwrap();
        assert_eq!(
            (out.status.code(), out.stdout, out.stderr),
            (Some(0), b"7\n".to_vec(), vec![])
        );
        // Compilation only: WASM logical evaluation remains eager in this slice.
        assert_eq!(&fs::read(&wasm).unwrap()[..4], b"\0asm");
        let out = f.run(&["--stdin", "--json"], Some(r#"[{"s":"","n":0}]"#));
        assert_eq!(
            (out.status.code(), out.stdout, out.stderr),
            (Some(0), b"[{\"out\":7}]\n".to_vec(), vec![])
        );
    }
}

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn constant_folding_cli_keeps_required_evaluation_and_wrapping_negation() {
    use std::os::unix::process::ExitStatusExt;
    let f = Fixture::new("folding-required");
    let native = f.0.join("native");
    for (expr, lets) in [
        ("0 * (1 / 0)", ""),
        ("(1 % 0) * 0", ""),
        ("(-9223372036854775807 - 1) / -1", ""),
        ("(-9223372036854775807 - 1) % -1", ""),
        ("if (if 1 / i.n > 0 then 1 else 2) > 0 then 7 else 9", ""),
        ("7", "    let unused = (1 / i.n) * 0\n"),
    ] {
        f.expression(expr, "number", lets);
        let out = f.run(&["--native", native.to_str().unwrap()], None);
        assert!(out.status.success(), "{expr}: {out:?}");
        assert!(out.stderr.is_empty());
        let out = Command::new(&native).args(["", "0"]).output().unwrap();
        assert_eq!(
            (out.status.signal(), out.stdout, out.stderr),
            (Some(8), vec![], vec![]),
            "{expr}"
        );
        // Do not rewrite an executable inode retained after the trap.
        fs::remove_file(&native).unwrap();
    }
    f.expression("-(-9223372036854775807 - 1)", "number", "");
    let out = f.run(&["--native", native.to_str().unwrap()], None);
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty());
    let out = Command::new(&native).args(["", "0"]).output().unwrap();
    assert_eq!(
        (out.status.code(), out.stdout, out.stderr),
        (Some(0), b"-9223372036854775808\n".to_vec(), vec![])
    );
}
