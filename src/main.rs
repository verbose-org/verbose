use std::env;
use std::fs;
use std::path::Path;
use std::process;

mod ast;
mod codegen;
mod interpreter;
mod lexer;
mod native;
mod optimizer;
mod parser;
mod validate_x86;
mod verifier;

fn main() {
    let args: Vec<String> = env::args().collect();

    // Special demo mode: HTTP server
    if args.iter().any(|a| a == "--demo-http") {
        let output = find_flag(&args, "--demo-http").unwrap_or_else(|| "/tmp/verbose-http".into());
        match native::emit_http_demo(&output) {
            Ok(()) => {
                let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
                println!("HTTP demo server: {} ({} bytes)", output, size);
                println!("Run it: ./{}", output);
                println!("Test:   curl http://localhost:9999");
            }
            Err(e) => {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
        return;
    }

    if args.len() < 2 {
        eprintln!("usage: verbosec <file.verbose> [options]");
        eprintln!();
        eprintln!("options:");
        eprintln!("  --run <rule> --input <data.json>   Interpret a rule on JSON data");
        eprintln!("  --emit-rust                        Print generated Rust source to stdout");
        eprintln!("  --compile <output>                 Compile to a standalone binary via rustc");
        eprintln!("  --native <output>                  Compile to native x86-64 ELF (no dependencies)");
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

    // Resolve imports: load and merge items from 'use' declarations
    let program = resolve_imports(program, base_dir);

    let errors = verifier::verify_program(&program, base_dir);
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("verify error {}", e);
        }
        eprintln!();
        eprintln!("verification failed: {} error(s)", errors.len());
        process::exit(1);
    }

    // Optimize AST (platform-independent transformations)
    let show_stats = args.iter().any(|a| a == "--stats");
    let (program, opt_stats) = optimizer::optimize_program(&program);

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
    if show_stats {
        println!("optimizations:\n{}", opt_stats);
    }

    let emit_rust = args.iter().any(|a| a == "--emit-rust");
    let compile_output = find_flag(&args, "--compile");
    let native_output = find_flag(&args, "--native");
    let run_rule = find_flag(&args, "--run");
    let input_path = find_flag(&args, "--input");

    if let Some(output) = native_output {
        let native_rule = find_flag(&args, "--run").unwrap_or_else(|| {
            program
                .items
                .iter()
                .rev()
                .find_map(|i| match i {
                    ast::Item::Rule(r) => Some(r.name.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        });
        match native::compile_native(&program, &native_rule, &output) {
            Ok(()) => {
                let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
                println!("native: {} -> {} ({} bytes, rule '{}')", path, output, size, native_rule);
                // Report exploited hints
                if let Some(rule) = program.items.iter().find_map(|i| match i {
                    ast::Item::Rule(r) if r.name == native_rule => Some(r),
                    _ => None,
                }) {
                    if let Some(hints) = &rule.hints {
                        if hints.vectorizable == Some(true) {
                            println!("  hint: vectorizable — SIMD-eligible (SSE4.2 pcmpgtq)");
                        }
                        if hints.parallel == Some(true) {
                            println!("  hint: parallel — multi-thread eligible");
                        }
                        if hints.cache_result == Some(true) {
                            println!("  hint: cache_result — memoization eligible");
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
    } else if emit_rust {
        println!();
        print!("{}", codegen::emit_rust(&program));
    } else if let Some(output) = compile_output {
        let rust_source = codegen::emit_rust(&program);
        let tmp = format!("{}.rs", output);
        fs::write(&tmp, &rust_source).unwrap_or_else(|e| {
            eprintln!("cannot write temp file '{}': {}", tmp, e);
            process::exit(1);
        });
        let status = process::Command::new("rustc")
            .args([&tmp, "-o", &output])
            .status()
            .unwrap_or_else(|e| {
                eprintln!("failed to run rustc: {}", e);
                process::exit(1);
            });
        let _ = fs::remove_file(&tmp);
        if !status.success() {
            eprintln!("rustc compilation failed");
            process::exit(1);
        }
        println!("compiled: {} -> {}", path, output);
    } else if let (Some(rule_name), Some(json_path)) = (run_rule, input_path) {
        let all_rules: Vec<&ast::Rule> = program
            .items
            .iter()
            .filter_map(|i| match i {
                ast::Item::Rule(r) => Some(r),
                _ => None,
            })
            .collect();
        let rule = all_rules
            .iter()
            .find(|r| r.name == rule_name)
            .unwrap_or_else(|| {
                eprintln!("no rule named '{}'", rule_name);
                process::exit(1);
            });

        let records = match interpreter::load_json_input(Path::new(&json_path)) {
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
            match interpreter::eval_rule(rule, &all_rules, record) {
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

fn find_first_concept_rule(program: &ast::Program) -> (&ast::Concept, &ast::Rule) {
    let concept = program
        .items
        .iter()
        .find_map(|i| match i {
            ast::Item::Concept(c) => Some(c),
            _ => None,
        })
        .unwrap_or_else(|| {
            eprintln!("no concept found in program");
            std::process::exit(1);
        });
    let rule = program
        .items
        .iter()
        .find_map(|i| match i {
            ast::Item::Rule(r) => Some(r),
            _ => None,
        })
        .unwrap_or_else(|| {
            eprintln!("no rule found in program");
            std::process::exit(1);
        });
    (concept, rule)
}

/// Resolve `use "file.verbose"` imports: load, parse, and merge items.
/// Handles circular imports via a "seen files" set.
fn resolve_imports(mut program: ast::Program, base_dir: &Path) -> ast::Program {
    let mut seen = std::collections::HashSet::new();
    let mut pending = program.uses.clone();

    while let Some(use_path) = pending.pop() {
        let resolved = base_dir.join(&use_path);
        let canonical = resolved.to_string_lossy().to_string();
        if seen.contains(&canonical) {
            continue; // already loaded — skip circular import
        }
        seen.insert(canonical);

        let use_source = match fs::read_to_string(&resolved) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error loading '{}': {}", use_path, e);
                process::exit(1);
            }
        };
        let use_tokens = match lexer::Lexer::new(&use_source).tokenize() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("in '{}': {}", use_path, e);
                process::exit(1);
            }
        };
        let use_program = match parser::Parser::new(use_tokens).parse_program() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("in '{}': {}", use_path, e);
                process::exit(1);
            }
        };

        // Queue any nested imports from the loaded module
        for nested_use in &use_program.uses {
            pending.push(nested_use.clone());
        }

        // Merge items (concepts + rules) into the main program
        // Imported items go BEFORE existing items so they're available
        let mut merged = use_program.items;
        merged.append(&mut program.items);
        program.items = merged;
    }

    program.uses.clear(); // imports resolved
    program
}
