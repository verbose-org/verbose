//! Check the real optimized artifact path against source interpretation.
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn signed_division_cli_preserves_signed_values_and_channels() {
    let f = Fixture(std::env::temp_dir().join(format!(
        "verbose-signed-division-cli-{}",
        std::process::id()
    )));
    fs::create_dir_all(&f.0).unwrap();
    fs::write(
        f.0.join("boolean_guards.intent"),
        include_str!("../examples/boolean_guards.intent"),
    )
    .unwrap();
    let source = f.0.join("case.verbose");
    let binary = f.0.join("native");
    let inputs = [
        i64::MIN,
        i64::MIN + 1,
        -9,
        -8,
        -7,
        -1,
        0,
        1,
        7,
        8,
        9,
        i64::MAX,
    ];
    let json = format!(
        "[{}]",
        inputs
            .iter()
            .map(|n| format!(r#"{{"s":"","n":{n}}}"#))
            .collect::<Vec<_>>()
            .join(",")
    );
    for d in [1, 2, 8, 1i64 << 32, 1i64 << 62] {
        let src = include_str!("../examples/boolean_guards.verbose")
            .replace(
                "if i.n >= 0 and i.n < length(i.s) and byte_at(i.s, i.n) > 0 then 1 else 0",
                &format!("i.n / {d}"),
            )
            .replace("reads: [i.n, i.s]", "reads: [i.n]");
        fs::write(&source, src).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_verbosec"))
            .arg(&source)
            .args(["--run", "guarded_byte", "--native"])
            .arg(&binary)
            .output()
            .unwrap();
        assert!(out.status.success(), "d={d}: {out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
        let out = Command::new(&binary)
            .args(inputs.iter().flat_map(|n| [String::new(), n.to_string()]))
            .output()
            .unwrap();
        let expected: String = inputs.iter().map(|n| format!("{}\n", n / d)).collect();
        assert_eq!(
            (out.status.code(), out.stdout, out.stderr),
            (Some(0), expected.into_bytes(), vec![]),
            "d={d}"
        );
        fs::remove_file(&binary).unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_verbosec"))
            .arg(&source)
            .args(["--run", "guarded_byte", "--stdin", "--json"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(json.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let expected = format!(
            "[{}]\n",
            inputs
                .iter()
                .map(|n| format!(r#"{{"out":{}}}"#, n / d))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(
            (out.status.code(), out.stdout, out.stderr),
            (Some(0), expected.into_bytes(), vec![]),
            "d={d}"
        );
    }
}
