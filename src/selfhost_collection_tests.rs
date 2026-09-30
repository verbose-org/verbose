//! Collection typing and native capability are independent contracts.
use crate::{
    ast::{iter_all_concepts, Item},
    interpreter::{self, Value},
    native,
    selfhost_constructor_tests::*,
    selfhost_constructor_type_tests::TAKE,
    verifier,
};
use std::{collections::HashMap, fs, path::Path, process::Command};

fn program(body: &str, output: &str, extra: &str, calls: &str) -> String {
    source(body, extra, calls)
        .replace(
            "    n : number\nconcept Pair",
            "    items : collection(number)\nconcept Pair",
        )
        .replacen(
            "    out : number\n  logic:",
            &format!("    out : {output}\n  logic:"),
            1,
        )
        .replacen(
            "reads: []",
            if body.contains("produce(i)") {
                "reads: [i]"
            } else if body.contains("i.items") {
                "reads: [i.items]"
            } else {
                "reads: []"
            },
            1,
        )
}

fn eval(src: &str, values: &[i64]) -> Result<Value, interpreter::RuntimeError> {
    let p = parse(src);
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    interpreter::eval_rule(
        rules[0],
        &rules,
        &concepts,
        &[],
        &HashMap::from([(
            "items".into(),
            Value::List(values.iter().copied().map(Value::Number).collect()),
        )]),
    )
}

fn supported() -> Vec<String> {
    [
        ("sum(i.items, x => x + 1)", "number"),
        ("count(i.items, x => x > 2)", "number"),
        ("fold(i.items, 7, acc, x => acc + x)", "number"),
        ("all(i.items, x => x > 2)", "bool"),
        ("any(i.items, x => x > 2)", "bool"),
        ("min(i.items, x => x)", "number"),
        ("max(i.items, x => x)", "number"),
        ("map(i.items, x => x + 1)", "collection(number)"),
        ("filter(i.items, x => x > 2)", "collection(number)"),
        ("sum(i.items, x => byte_at(\"a\", x))", "number"),
    ]
    .into_iter()
    .map(|(expr, ty)| program(&format!("    out = {expr}"), ty, "", ""))
    .chain([
        program(
            "    let x = 7\n    out = sum(i.items, x => x) + x",
            "number",
            "",
            "",
        ),
        program(
            "    out = sum(i.items, x => take(Pair { first: x, second: 2 }))",
            "number",
            TAKE,
            "take",
        ),
    ])
    .collect()
}

fn refused() -> Vec<String> {
    let mut out = vec![];
    for producer in ["map(i.items, x => x + 1)", "filter(i.items, x => x > 2)"] {
        for (expr, ty) in [
            (format!("sum({producer}, x => x)"), "number"),
            (format!("count({producer}, x => x > 2)"), "number"),
            (format!("fold({producer}, 0, acc, x => acc + x)"), "number"),
            (format!("all({producer}, x => x > 2)"), "bool"),
            (format!("any({producer}, x => x > 2)"), "bool"),
            (format!("min({producer}, x => x)"), "number"),
            (format!("max({producer}, x => x)"), "number"),
            (format!("map({producer}, x => x + 1)"), "collection(number)"),
            (
                format!("filter({producer}, x => x > 2)"),
                "collection(number)",
            ),
        ] {
            out.push(program(&format!("    out = {expr}"), ty, "", ""));
        }
        let reduce = format!("sum({producer}, x => x)");
        for body in [
            format!("    let unused = {producer}\n    out = 7"),
            format!("    let xs = {producer}\n    let alias = xs\n    let xs = 0\n    out = sum(alias, x => x)"),
            format!("    out = if 1 < 2 then 7 else {reduce}"),
            format!("    out = if {reduce} > 0 then 7 else 0"),
            format!("    out = max(7, {reduce})"),
            format!("    out = match_result(Ok({reduce}), value => value, error => 0)"),
            format!("    out = match_result(Ok(7), value => value, error => {reduce})"),
            format!("    out = fold(i.items, {reduce}, acc, x => acc + x)"),
            format!("    out = sum(i.items, x => {reduce})"),
            format!("    out = take(Pair {{ first: {reduce}, second: 2 }})"),
            format!("    out = match Choice::Empty:\n      Empty => 7\n      Label(n, text) => {reduce}\n      Trio(a, b, c) => 0\n      Link(value, next) => 0"),
        ] {
            let take = body.contains("take(");
            out.push(program(&body, "number", if take { TAKE } else { "" }, if take { "take" } else { "" }));
        }
        out.push(program(
            &format!("    out = if 1 < 2 then {producer} else {producer}"),
            "collection(number)",
            "",
            "",
        ));
        out.push(program(&format!("    out = {producer}"), "number", "", ""));
    }
    // Collection-returning calls cannot return the streamed collection as a value.
    let producer = program(
        "    out = map(i.items, x => x + 1)",
        "collection(number)",
        "",
        "",
    );
    let helper = producer[producer.find("rule probe\n").unwrap()..]
        .replace("rule probe\n", "rule produce\n");
    for (body, ty) in [
        ("    out = sum(produce(i), x => x)", "number"),
        ("    let unused = produce(i)\n    out = 7", "number"),
        ("    out = produce(i)", "collection(number)"),
    ] {
        out.push(program(body, ty, &helper, "produce"));
    }
    out.push(program("    out = i.items", "collection(number)", "", ""));
    out.push(program(
        "    let xs = i.items\n    out = xs",
        "collection(number)",
        "",
        "",
    ));
    // An uncalled rule must still be checked, even when it precedes the ELF entry.
    let bad = program(
        "    out = sum(map(i.items, x => x + 1), x => x)",
        "number",
        "",
        "",
    );
    let unused = bad[bad.find("rule probe\n").unwrap()..].replace("rule probe\n", "rule unused\n");
    out.push(program("    out = 7", "number", &unused, ""));
    out.push(format!(
        "{bad}\n{}",
        unused
            .replace("rule unused\n", "rule selected\n")
            .replace("sum(map(i.items, x => x + 1), x => x)", "7")
            .replace("reads: [i.items]", "reads: []")
    ));
    out
}

pub(crate) fn assert_collections(
    compiler: &Path,
    raw: &Path,
    checker: &Path,
    gate: &Path,
    base: &Path,
) {
    let executable = base.join("collection-probe");
    for src in supported() {
        assert!(
            verifier::verify_program(&parse(&src), Path::new("examples")).is_empty(),
            "{src}"
        );
        let r = send(gate, &src, 0, false);
        assert_eq!(
            (r.status.code(), r.stdout, r.stderr),
            (Some(0), b"0\n".to_vec(), vec![]),
            "{src}"
        );
        install(send(compiler, &src, 0, false), &executable);
        let r = send(raw, &src, 0, false);
        assert_eq!(r.status.code(), Some(0), "{src}");
        assert!(!r.stdout.is_empty() && r.stderr.is_empty());
        for values in [vec![], vec![2, 5, 9], vec![-4, 0, 3], vec![0]] {
            let mut args = vec![values.len().to_string()];
            args.extend(values.iter().map(i64::to_string));
            let r = Command::new(&executable).args(args).output().unwrap();
            let (status, stdout) = match eval(&src, &values) {
                Ok(Value::List(v)) => (0, v.iter().map(|v| format!("{v}\n")).collect::<String>()),
                Ok(Value::Bool(v)) => (if v { 0 } else { 1 }, format!("{v}\n")),
                Ok(value) => (0, format!("{value}\n")),
                Err(_) => (1, String::new()),
            };
            assert_eq!(
                (r.status.code(), r.stdout, r.stderr),
                (Some(status), stdout.into_bytes(), vec![]),
                "{src}\n{values:?}"
            );
        }
    }
    for src in refused() {
        let r = send(gate, &src, 0, false);
        assert_eq!(r.status.code(), Some(0), "{src}");
        assert!(r.stderr.is_empty());
        assert!(
            String::from_utf8(r.stdout)
                .unwrap()
                .trim()
                .parse::<i64>()
                .unwrap()
                > 0,
            "{src}"
        );
        for backend in [compiler, raw] {
            let r = send(backend, &src, 0, false);
            assert_eq!(
                (r.status.code(), r.stdout, r.stderr),
                (Some(1), vec![], vec![]),
                "{src}"
            );
        }
        if src.contains("rule selected") {
            let r = send(compiler, &src, 1, false);
            assert_eq!(
                (r.status.code(), r.stdout, r.stderr),
                (Some(1), vec![], vec![])
            );
        }
    }
    // Capability refusal must not be mistaken for an invalid language expression.
    for (expr, expected) in [
        (
            "sum(map(i.items, x => x + 1), x => take(Pair { first: x, second: 2 }))",
            19,
        ),
        (
            "sum(filter(i.items, x => x > 2), x => take(Pair { first: x, second: 2 }))",
            14,
        ),
    ] {
        let src = program(&format!("    out = {expr}"), "number", TAKE, "take");
        assert!(verifier::verify_program(&parse(&src), Path::new("examples")).is_empty());
        assert_eq!(eval(&src, &[2, 5, 9]).unwrap(), Value::Number(expected));
        let r = send(checker, &src, 0, false);
        assert_eq!(
            (r.status.code(), r.stdout, r.stderr),
            (Some(0), b"0\n".to_vec(), vec![])
        );
        for backend in [compiler, raw] {
            let r = send(backend, &src, 0, false);
            assert_eq!(
                (r.status.code(), r.stdout, r.stderr),
                (Some(1), vec![], vec![])
            );
        }
    }
}

#[test]
fn selfhost_collections_refuse_intermediate_values_before_output() {
    let p = parse(include_str!("../examples/vexprparse.verbose"));
    let base =
        std::env::temp_dir().join(format!("verbose-collection-guard-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let drivers: Vec<_> = [
        "elf_program_src",
        "x86_program_src",
        "type_check",
        "collection_lowering_check",
    ]
    .into_iter()
    .map(|name| {
        let path = base.join(name);
        native::compile_native_stdin_raw(&p, name, path.to_str().unwrap()).unwrap();
        path
    })
    .collect();
    assert_collections(&drivers[0], &drivers[1], &drivers[2], &drivers[3], &base);
    fs::remove_dir_all(base).unwrap();
}
