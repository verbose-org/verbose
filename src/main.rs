use std::env;
use std::fs;
use std::path::Path;
use std::process;

mod ast;
mod interpreter;
mod lexer;
mod parser;
mod verifier;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("usage: verbosec <file.verbose> [--run <rule> --input <data.json>]");
        process::exit(2);
    }
    let path = &args[1];
    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading {}: {}", path, e);
            process::exit(1);
        }
    };

    let tokens = match lexer::Lexer::new(&source).tokenize() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{}", e);
            process::exit(1);
        }
    };

    let program = match parser::Parser::new(tokens).parse_program() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", e);
            process::exit(1);
        }
    };

    let base_dir = Path::new(path).parent().unwrap_or_else(|| Path::new("."));
    let errors = verifier::verify_program(&program, base_dir);
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("verify error {}", e);
        }
        eprintln!();
        eprintln!("verification failed: {} error(s)", errors.len());
        process::exit(1);
    }

    let n_concepts = program
        .items
        .iter()
        .filter(|i| matches!(i, ast::Item::Concept(_)))
        .count();
    let n_rules = program
        .items
        .iter()
        .filter(|i| matches!(i, ast::Item::Rule(_)))
        .count();
    println!(
        "verified: {} concept(s), {} rule(s); all proofs check out",
        n_concepts, n_rules
    );

    let run_rule = find_flag(&args, "--run");
    let input_path = find_flag(&args, "--input");

    if let (Some(rule_name), Some(json_path)) = (run_rule, input_path) {
        let rule = program
            .items
            .iter()
            .find_map(|it| match it {
                ast::Item::Rule(r) if r.name == rule_name => Some(r),
                _ => None,
            })
            .unwrap_or_else(|| {
                eprintln!("no rule named '{}'", rule_name);
                process::exit(1);
            });

        let records =
            match interpreter::load_json_input(Path::new(&json_path)) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            };

        println!();
        println!(
            "executing rule '{}' on {} record(s):",
            rule_name,
            records.len()
        );
        for (idx, record) in records.iter().enumerate() {
            match interpreter::eval_rule(rule, record) {
                Ok(val) => {
                    println!("  [{}] {} = {}", idx, rule.output_name, val);
                }
                Err(e) => {
                    eprintln!("  [{}] {}", idx, e);
                    process::exit(1);
                }
            }
        }
    }
}

fn find_flag(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
