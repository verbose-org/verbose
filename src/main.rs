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
mod wasm;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("verbosec 0.1.0");
        println!("A language designed for AI, verified by compiler, pushed by humans.");
        println!();
        println!("Backends: interpreter, Rust transpiler, native x86-64, WebAssembly");
        println!("Features: 15 language features, 10+ proof checks, 11 optimizations");
        println!("License:  Apache 2.0");
        println!("Repo:     https://github.com/verbose-org/verbose");
        return;
    }

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
        eprintln!("  --wasm <output>                    Compile to WebAssembly module (.wasm)");
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
    let json_output = args.iter().any(|a| a == "--json");
    if !json_output {
        println!(
            "verified: {} concept(s), {} rule(s); all proofs check out",
            n_concepts, n_rules
        );
    }
    if show_stats {
        println!("optimizations:\n{}", opt_stats);
    }

    let emit_rust = args.iter().any(|a| a == "--emit-rust");
    let compile_output = find_flag(&args, "--compile");
    let native_output = find_flag(&args, "--native");
    let run_rule = find_flag(&args, "--run");
    let input_path = find_flag(&args, "--input");

    if args.iter().any(|a| a == "--benchmark") {
        let bench_rule = find_flag(&args, "--run").unwrap_or_else(|| {
            program.items.iter().find_map(|i| match i {
                ast::Item::Rule(r) => Some(r.name.clone()),
                _ => None,
            }).unwrap_or_default()
        });

        println!();
        println!("=== Verbose Benchmark: {} ===", bench_rule);
        println!();

        let wasm_path = "/tmp/verbose_bench.wasm";
        let wasm_size = wasm::compile_wasm(&program, &bench_rule, wasm_path).ok().and_then(|()| {
            let s = std::fs::metadata(wasm_path).map(|m| m.len()).ok();
            let _ = std::fs::remove_file(wasm_path);
            s
        });

        let native_path = "/tmp/verbose_bench_native";
        let native_size = native::compile_native(&program, &bench_rule, native_path).ok().and_then(|()| {
            let s = std::fs::metadata(native_path).map(|m| m.len()).ok();
            let _ = std::fs::remove_file(native_path);
            s
        });

        let rust_path = "/tmp/verbose_bench_rust";
        let rust_source = codegen::emit_rust(&program);
        let rust_tmp = format!("{}.rs", rust_path);
        let rust_size = fs::write(&rust_tmp, &rust_source).ok().and_then(|()| {
            let status = process::Command::new("rustc")
                .args([&rust_tmp, "-o", rust_path])
                .stdout(process::Stdio::null())
                .stderr(process::Stdio::null())
                .status().ok()?;
            let _ = fs::remove_file(&rust_tmp);
            if status.success() {
                let size = fs::metadata(rust_path).map(|m| m.len()).ok();
                let _ = fs::remove_file(rust_path);
                size
            } else { None }
        });

        let rule = program.items.iter().find_map(|i| match i {
            ast::Item::Rule(r) if r.name == bench_rule => Some(r),
            _ => None,
        });
        let mut hints_list = Vec::new();
        if let Some(h) = rule.and_then(|r| r.hints.as_ref()) {
            if h.vectorizable.is_some() { hints_list.push("SIMD"); }
            if h.parallel.is_some() { hints_list.push("parallel"); }
            if h.overflow.is_some() { hints_list.push("overflow-safe"); }
        }

        println!("  Backend           Size         Dependencies");
        println!("  -------           ----         ------------");
        if let Some(s) = wasm_size {
            println!("  WASM              {:>6} B     browser / Node.js", s);
        }
        if let Some(s) = native_size {
            println!("  Native x86-64     {:>6} B     none (zero)", s);
        }
        if let Some(s) = rust_size {
            println!("  Rust transpiled  {:>6} KB    libc", s / 1024);
        }
        println!();
        if !hints_list.is_empty() {
            println!("  Hints exploited:  {}", hints_list.join(", "));
        }
        let elim = opt_stats.nodes_before.saturating_sub(opt_stats.nodes_after);
        if elim > 0 {
            println!("  Optimizations:    {} AST nodes eliminated", elim);
        }
        println!("  Proofs verified:  purity, termination, determinism");
    } else if let Some(output) = native_output {
        let native_rule_str = find_flag(&args, "--run").unwrap_or_else(|| {
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
        // Multi-rule: "rule1,rule2,..." compiles all into one binary.
        let rule_names: Vec<&str> = native_rule_str.split(',').collect();
        let compile_result = if rule_names.len() > 1 {
            native::compile_native_multi(&program, &rule_names, &output)
        } else {
            native::compile_native(&program, &rule_names[0], &output)
        };
        let native_rule = &native_rule_str; // for display
        match compile_result {
            Ok(()) => {
                let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
                println!("native: {} -> {} ({} bytes, rule '{}')", path, output, size, native_rule);
                // Report exploited hints
                if let Some(rule) = program.items.iter().find_map(|i| match i {
                    ast::Item::Rule(r) if r.name == *native_rule => Some(r),
                    _ => None,
                }) {
                    if let Some(hints) = &rule.hints {
                        if let Some(reason) = &hints.vectorizable {
                            println!("  hint: vectorizable — SIMD-eligible (SSE4.2 pcmpgtq)");
                            println!("        reason: {}", reason);
                        }
                        if let Some(reason) = &hints.parallel {
                            println!("  hint: parallel — multi-thread eligible");
                            println!("        reason: {}", reason);
                        }
                        if let Some(reason) = &hints.cache_result {
                            println!("  hint: cache_result — memoization eligible");
                            println!("        reason: {}", reason);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
    } else if args.iter().any(|a| a == "--disasm") {
        let disasm_rule = find_flag(&args, "--run").unwrap_or_else(|| {
            program.items.iter().find_map(|i| match i {
                ast::Item::Rule(r) => Some(r.name.clone()),
                _ => None,
            }).unwrap_or_default()
        });
        let tmp = "/tmp/verbose_disasm_tmp";
        match native::compile_native(&program, &disasm_rule, tmp) {
            Ok(()) => {
                let size = fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
                println!("native: {} bytes, rule '{}'\n", size, disasm_rule);
                // Extract just the code section (skip 120-byte ELF header)
                // and disassemble with objdump in raw mode starting at the
                // right offset, or fall back to hex dump.
                let code_tmp = format!("{}.code", tmp);
                let disasm_ok = (|| -> Option<()> {
                    let bytes = fs::read(tmp).ok()?;
                    if bytes.len() <= 120 { return None; }
                    fs::write(&code_tmp, &bytes[120..]).ok()?;
                    let out = process::Command::new("objdump")
                        .args(["-D", "-b", "binary", "-m", "i386:x86-64", "-M", "intel", &code_tmp])
                        .output().ok()?;
                    if !out.status.success() { return None; }
                    let text = String::from_utf8_lossy(&out.stdout);
                    for line in text.lines().skip(7) {
                        if !line.is_empty() {
                            println!("{}", line);
                        }
                    }
                    Some(())
                })();
                let _ = fs::remove_file(&code_tmp);
                if disasm_ok.is_none() {
                    // Fallback: hex dump of code section
                    eprintln!("(objdump unavailable, showing hex dump)");
                    if let Ok(bytes) = fs::read(tmp) {
                        for (i, chunk) in bytes[120..].chunks(16).enumerate() {
                            print!("  {:04x}: ", i * 16);
                            for b in chunk { print!("{:02x} ", b); }
                            println!();
                        }
                    }
                }
                let _ = fs::remove_file(tmp);
            }
            Err(e) => {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
    } else if let Some(output) = find_flag(&args, "--wasm") {
        let wasm_rule = find_flag(&args, "--run").unwrap_or_else(|| {
            program.items.iter().rev().find_map(|i| match i {
                ast::Item::Rule(r) => Some(r.name.clone()),
                _ => None,
            }).unwrap_or_default()
        });
        match wasm::compile_wasm(&program, &wasm_rule, &output) {
            Ok(()) => {
                let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
                println!("wasm: {} -> {} ({} bytes, rule '{}')", path, output, size, wasm_rule);
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
    } else if let (Some(rule_name), json_path) = (run_rule, input_path) {
        // Support --stdin as alternative to --input file
        let stdin_mode = args.iter().any(|a| a == "--stdin");
        let json_path = if stdin_mode {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).unwrap_or_else(|e| {
                eprintln!("cannot read stdin: {}", e);
                process::exit(1);
            });
            let tmp = "/tmp/verbose_stdin.json";
            fs::write(tmp, &buf).unwrap();
            Some(tmp.to_string())
        } else {
            json_path
        };
        let json_path = match json_path {
            Some(p) => p,
            None => {
                eprintln!("--run requires --input <file> or --stdin");
                process::exit(2);
            }
        };
        let all_rules: Vec<&ast::Rule> = program
            .items
            .iter()
            .filter_map(|i| match i {
                ast::Item::Rule(r) => Some(r),
                _ => None,
            })
            .collect();

        // Check if it's a reaction
        let reaction = program.items.iter().find_map(|i| match i {
            ast::Item::Reaction(rx) if rx.name == rule_name => Some(rx),
            _ => None,
        });

        if let Some(rx) = reaction {
            let trigger_rule = all_rules
                .iter()
                .find(|r| r.name == rx.trigger)
                .unwrap_or_else(|| {
                    eprintln!("trigger rule '{}' not found", rx.trigger);
                    process::exit(1);
                });

            let records = match interpreter::load_json_input(Path::new(&json_path)) {
                Ok(r) => r,
                Err(e) => { eprintln!("{}", e); process::exit(1); }
            };

            println!();
            println!("executing reaction '{}' (trigger: {}) on {} record(s):",
                rule_name, rx.trigger, records.len());

            for (idx, record) in records.iter().enumerate() {
                match interpreter::eval_rule(trigger_rule, &all_rules, record) {
                    Ok(val) => {
                        let should_fire = match &val {
                            interpreter::Value::Bool(true) => true,
                            interpreter::Value::Number(n) if *n != 0 => true,
                            _ => false,
                        };
                        if should_fire {
                            for effect in &rx.effects {
                                match effect {
                                    ast::Effect::Print(args) => {
                                        let parts: Vec<String> = args.iter().map(|arg| {
                                            match interpreter::eval_rule_expr(arg, trigger_rule, &all_rules, record) {
                                                Ok(v) => format!("{}", v),
                                                Err(_) => format!("{:?}", arg),
                                            }
                                        }).collect();
                                        println!("  [{}] EFFECT print: {}", idx, parts.join(" "));
                                    }
                                    ast::Effect::AppendFile { path, content } => {
                                        // Evaluate the content expression, coerce to text,
                                        // append to the declared path. The path is a
                                        // literal at parse time, so this is the only
                                        // file this effect can ever touch.
                                        let text = match interpreter::eval_rule_expr(content, trigger_rule, &all_rules, record) {
                                            Ok(interpreter::Value::Text(s)) => s,
                                            Ok(interpreter::Value::Number(n)) => n.to_string(),
                                            Ok(interpreter::Value::Bool(b)) => b.to_string(),
                                            Ok(other) => {
                                                eprintln!("  [{}] EFFECT append_file: content must be scalar, got {}", idx, other);
                                                process::exit(1);
                                            }
                                            Err(e) => {
                                                eprintln!("  [{}] EFFECT append_file: {}", idx, e);
                                                process::exit(1);
                                            }
                                        };
                                        use std::fs::OpenOptions;
                                        use std::io::Write;
                                        let res = OpenOptions::new()
                                            .create(true)
                                            .append(true)
                                            .open(path)
                                            .and_then(|mut f| f.write_all(text.as_bytes()));
                                        if let Err(e) = res {
                                            eprintln!("  [{}] EFFECT append_file '{}': {}", idx, path, e);
                                            process::exit(1);
                                        }
                                        println!("  [{}] EFFECT append_file '{}': {} bytes", idx, path, text.len());
                                    }
                                }
                            }
                        } else {
                            println!("  [{}] (trigger = {} — no effects)", idx, val);
                        }
                    }
                    Err(e) => { eprintln!("  [{}] {}", idx, e); process::exit(1); }
                }
            }
            return;
        }

        let rule = all_rules
            .iter()
            .find(|r| r.name == rule_name)
            .unwrap_or_else(|| {
                eprintln!("no rule or reaction named '{}'", rule_name);
                process::exit(1);
            });

        let records = match interpreter::load_json_input(Path::new(&json_path)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{}", e);
                process::exit(1);
            }
        };

        let json_output = args.iter().any(|a| a == "--json");

        if json_output {
            // Machine-readable JSON output
            let mut results = Vec::new();
            for record in records.iter() {
                match interpreter::eval_rule(rule, &all_rules, record) {
                    Ok(val) => {
                        let json_val = value_to_json(&val);
                        results.push(format!(
                            "{{\"{}\":{}}}",
                            rule.output_name, json_val
                        ));
                    }
                    Err(e) => {
                        eprintln!("{}", e);
                        process::exit(1);
                    }
                }
            }
            println!("[{}]", results.join(","));
        } else {
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
}

/// Serialize a runtime value as a JSON literal.
/// Collections (Value::List) emit as real JSON arrays so map/filter outputs
/// round-trip correctly; records emit as JSON objects. Text strings escape " and \.
fn value_to_json(val: &interpreter::Value) -> String {
    match val {
        interpreter::Value::Number(n) => n.to_string(),
        interpreter::Value::Bool(b) => b.to_string(),
        interpreter::Value::Text(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{}\"", escaped)
        }
        interpreter::Value::List(items) => {
            let parts: Vec<String> = items.iter().map(value_to_json).collect();
            format!("[{}]", parts.join(","))
        }
        interpreter::Value::Record(fields) => {
            let mut keys: Vec<&String> = fields.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("\"{}\":{}", k, value_to_json(&fields[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        interpreter::Value::Ok(inner) => format!("{{\"ok\":{}}}", value_to_json(inner)),
        interpreter::Value::Err(inner) => format!("{{\"err\":{}}}", value_to_json(inner)),
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

        // Rewrite @source paths of imported items so they resolve relative
        // to the importing file's base_dir (not the imported module's dir).
        // E.g., stdlib/finance.verbose has @source: finance.intent:1. After
        // import into app.verbose (base_dir = examples/), the path becomes
        // stdlib/finance.intent so the verifier finds examples/stdlib/finance.intent.
        let import_dir = Path::new(&use_path).parent().unwrap_or(Path::new(""));
        let mut rewritten_items = use_program.items;
        for item in &mut rewritten_items {
            let rewrite = |sref: &mut ast::SourceRef| {
                let prefixed = import_dir.join(&sref.file);
                sref.file = prefixed.to_string_lossy().to_string();
            };
            match item {
                ast::Item::Concept(c) => rewrite(&mut c.source),
                ast::Item::Rule(r) => rewrite(&mut r.source),
                ast::Item::Reaction(rx) => rewrite(&mut rx.source),
            }
        }

        // Merge items (concepts + rules) into the main program
        // Imported items go BEFORE existing items so they're available
        rewritten_items.append(&mut program.items);
        program.items = rewritten_items;
    }

    program.uses.clear(); // imports resolved
    program
}
