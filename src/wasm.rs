/// WebAssembly backend — produces .wasm modules from Verbose rules.
///
/// WASM is a stack-based bytecode that runs in browsers (Chrome, Firefox,
/// Safari) and server-side (Node.js, Deno, Cloudflare Workers).
///
/// A Verbose rule compiles to a single exported WASM function:
///   (func (export "rule_name") (param i64 ...) (result i64 or i32)
///     ... bytecode ...
///   )
///
/// The WASM module is typically 60-200 bytes — even smaller than our
/// x86-64 binaries because WASM has no ELF headers or syscall overhead.

use crate::ast::*;

#[derive(Debug)]
pub struct WasmError {
    pub message: String,
}

impl std::fmt::Display for WasmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wasm error: {}", self.message)
    }
}

pub fn compile_wasm(
    program: &Program,
    rule_name: &str,
    output_path: &str,
) -> Result<(), WasmError> {
    let rules: std::collections::HashMap<&str, &Rule> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some((r.name.as_str(), r)),
            _ => None,
        })
        .collect();
    let concepts: Vec<&Concept> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Concept(c) => Some(c),
            _ => None,
        })
        .collect();

    let rule = rules.get(rule_name).ok_or_else(|| WasmError {
        message: format!("no rule named '{}'", rule_name),
    })?;

    let concept = match &rule.input_ty {
        Type::Named(n) => concepts.iter().find(|c| c.name == *n).ok_or_else(|| WasmError {
            message: format!("unknown concept '{}'", n),
        })?,
        _ => return Err(WasmError { message: "rule input must be a named concept".into() }),
    };

    let nfields = concept.fields.len();
    let is_bool = rule.output_ty == Type::Bool;

    let mut module = Vec::new();

    // === WASM header ===
    module.extend_from_slice(b"\0asm");     // magic
    module.extend_from_slice(&1u32.to_le_bytes()); // version 1

    // === Type section (function signature) ===
    let mut type_section = Vec::new();
    type_section.push(1); // 1 type
    type_section.push(0x60); // func type
    type_section.push(nfields as u8); // N params
    for _ in 0..nfields {
        type_section.push(0x7E); // i64
    }
    type_section.push(1); // 1 result
    type_section.push(if is_bool { 0x7F } else { 0x7E }); // i32 for bool, i64 for number
    emit_section(&mut module, 1, &type_section);

    // === Function section ===
    let func_section = vec![1, 0]; // 1 function, uses type 0
    emit_section(&mut module, 3, &func_section);

    // === Export section ===
    let mut export_section = Vec::new();
    export_section.push(1); // 1 export
    emit_name(&mut export_section, rule_name);
    export_section.push(0x00); // export kind: function
    export_section.push(0x00); // function index 0
    emit_section(&mut module, 7, &export_section);

    // === Code section ===
    let mut body = Vec::new();

    // Let bindings: need locals
    let n_locals = rule.logic.bindings.len();
    if n_locals > 0 {
        // Declare locals: all i64
        emit_leb128(&mut body, n_locals as u64);
        body.push(n_locals as u8);
        body.push(0x7E); // i64
    } else {
        body.push(0); // 0 local declarations
    }

    // Emit let binding computations
    for (i, (_, expr)) in rule.logic.bindings.iter().enumerate() {
        emit_wasm_expr(&mut body, expr, rule, concept, &rules)?;
        // Store in local (params take slots 0..nfields-1, locals start at nfields)
        body.push(0x21); // local.set
        emit_leb128(&mut body, (nfields + i) as u64);
    }

    // Emit main expression
    emit_wasm_expr(&mut body, &rule.logic.value, rule, concept, &rules)?;

    // If rule returns bool but expr produces i64, wrap to i32
    if is_bool {
        body.push(0xA7); // i32.wrap_i64
    }

    body.push(0x0B); // end

    let mut code_section = Vec::new();
    code_section.push(1); // 1 function body
    emit_leb128(&mut code_section, body.len() as u64);
    code_section.extend_from_slice(&body);
    emit_section(&mut module, 10, &code_section);

    std::fs::write(output_path, &module).map_err(|e| WasmError {
        message: format!("cannot write '{}': {}", output_path, e),
    })?;

    Ok(())
}

fn emit_wasm_expr(
    code: &mut Vec<u8>,
    expr: &Expr,
    rule: &Rule,
    concept: &Concept,
    all_rules: &std::collections::HashMap<&str, &Rule>,
) -> Result<(), WasmError> {
    let nfields = concept.fields.len();

    match expr {
        Expr::Number(n) => {
            code.push(0x42); // i64.const
            emit_sleb128(code, *n);
            Ok(())
        }
        Expr::Field(base, field_name) => {
            if !matches!(base.as_ref(), Expr::Ident(n) if n == &rule.input_name) {
                return Err(WasmError { message: "nested field access not supported".into() });
            }
            let idx = concept.fields.iter().position(|f| f.name == *field_name)
                .ok_or_else(|| WasmError { message: format!("unknown field '{}'", field_name) })?;
            code.push(0x20); // local.get
            emit_leb128(code, idx as u64);
            Ok(())
        }
        Expr::Ident(name) => {
            // Check let bindings
            if let Some(idx) = rule.logic.bindings.iter().position(|(n, _)| n == name) {
                code.push(0x20); // local.get
                emit_leb128(code, (nfields + idx) as u64);
                Ok(())
            } else {
                Err(WasmError { message: format!("unresolved ident '{}'", name) })
            }
        }
        Expr::Binary(op, left, right) => {
            emit_wasm_expr(code, left, rule, concept, all_rules)?;
            emit_wasm_expr(code, right, rule, concept, all_rules)?;
            match op {
                BinOp::Add => code.push(0x7C),    // i64.add
                BinOp::Sub => code.push(0x7D),    // i64.sub
                BinOp::Mul => code.push(0x7E),    // i64.mul
                BinOp::Div => code.push(0x7F),    // i64.div_s
                BinOp::Mod => code.push(0x81),    // i64.rem_s
                // Comparisons return i32 in WASM — extend to i64 for consistency
                BinOp::Gt => { code.push(0x55); code.push(0xAD); }     // i64.gt_s → i64.extend_i32_u
                BinOp::Lt => { code.push(0x53); code.push(0xAD); }     // i64.lt_s → i64.extend_i32_u
                BinOp::GtEq => { code.push(0x57); code.push(0xAD); }   // i64.ge_s → i64.extend_i32_u
                BinOp::LtEq => { code.push(0x55); code.push(0xAD); }   // i64.le_s → i64.extend_i32_u
                BinOp::Eq => { code.push(0x51); code.push(0xAD); }     // i64.eq → i64.extend_i32_u
                BinOp::NotEq => { code.push(0x52); code.push(0xAD); }  // i64.ne → i64.extend_i32_u
                BinOp::And => code.push(0x83),     // i64.and
                BinOp::Or => code.push(0x84),      // i64.or
            }
            Ok(())
        }
        Expr::If(cond, then_e, else_e) => {
            emit_wasm_expr(code, cond, rule, concept, all_rules)?;
            code.push(0xA7); // i32.wrap_i64 (condition must be i32)
            code.push(0x04); // if
            code.push(0x7E); // result type: i64
            emit_wasm_expr(code, then_e, rule, concept, all_rules)?;
            code.push(0x05); // else
            emit_wasm_expr(code, else_e, rule, concept, all_rules)?;
            code.push(0x0B); // end
            Ok(())
        }
        Expr::Not(inner) => {
            emit_wasm_expr(code, inner, rule, concept, all_rules)?;
            code.push(0x50); // i64.eqz
            Ok(())
        }
        Expr::Neg(inner) => {
            code.push(0x42); code.push(0x00); // i64.const 0
            emit_wasm_expr(code, inner, rule, concept, all_rules)?;
            code.push(0x7D); // i64.sub (0 - x = -x)
            Ok(())
        }
        Expr::Call(name, args) => {
            if args.len() != 1 {
                return Err(WasmError { message: "call requires 1 argument".into() });
            }
            let called = all_rules.get(name.as_str()).ok_or_else(|| WasmError {
                message: format!("unknown rule '{}'", name),
            })?;
            // Inline the called rule's logic with the same field layout
            emit_wasm_expr(code, &called.logic.value, called, concept, all_rules)
        }
        _ => Err(WasmError {
            message: format!("unsupported expression in WASM backend"),
        }),
    }
}

fn emit_section(module: &mut Vec<u8>, id: u8, content: &[u8]) {
    module.push(id);
    emit_leb128(module, content.len() as u64);
    module.extend_from_slice(content);
}

fn emit_name(buf: &mut Vec<u8>, name: &str) {
    emit_leb128(buf, name.len() as u64);
    buf.extend_from_slice(name.as_bytes());
}

fn emit_leb128(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn emit_sleb128(buf: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        let more = !(value == 0 && byte & 0x40 == 0) && !(value == -1 && byte & 0x40 != 0);
        if more {
            buf.push(byte | 0x80);
        } else {
            buf.push(byte);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128_encoding() {
        let mut buf = Vec::new();
        emit_leb128(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);

        buf.clear();
        emit_leb128(&mut buf, 127);
        assert_eq!(buf, vec![0x7F]);

        buf.clear();
        emit_leb128(&mut buf, 128);
        assert_eq!(buf, vec![0x80, 0x01]);
    }

    #[test]
    fn sleb128_encoding() {
        let mut buf = Vec::new();
        emit_sleb128(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);

        buf.clear();
        emit_sleb128(&mut buf, -1);
        assert_eq!(buf, vec![0x7F]);

        buf.clear();
        emit_sleb128(&mut buf, 10000);
        // 10000 = 0x2710 → LEB128: 0x90 0xCE 0x00
        assert_eq!(buf, vec![0x90, 0xCE, 0x00]);
    }

    #[test]
    fn wasm_module_has_valid_header() {
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number
rule test_rule
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : bool
  logic:
    r = t.x > 10
  proofs:
    purity:
      reads: [t.x]
      calls: []
      verdict: pure
    termination:
      form: constant_bound
      bound: 1
    determinism:
      form: total
"#;
        let tokens = Lexer::new(src).tokenize().unwrap();
        let program = Parser::new(tokens).parse_program().unwrap();
        let path = "/tmp/test_wasm_header.wasm";
        compile_wasm(&program, "test_rule", path).unwrap();
        let bytes = std::fs::read(path).unwrap();
        std::fs::remove_file(path).ok();
        // WASM magic: \0asm
        assert_eq!(&bytes[0..4], b"\0asm");
        // Version 1
        assert_eq!(&bytes[4..8], &[1, 0, 0, 0]);
        // Module should be small (< 200 bytes)
        assert!(bytes.len() < 200, "module too large: {} bytes", bytes.len());
    }

    #[test]
    fn wasm_arithmetic_module() {
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule add_them
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : number
  logic:
    r = t.a + t.b * 2
  proofs:
    purity:
      reads: [t.a, t.b]
      calls: []
      verdict: pure
    termination:
      form: constant_bound
      bound: 2
    determinism:
      form: total
"#;
        let tokens = Lexer::new(src).tokenize().unwrap();
        let program = Parser::new(tokens).parse_program().unwrap();
        let path = "/tmp/test_wasm_arith.wasm";
        compile_wasm(&program, "add_them", path).unwrap();
        let bytes = std::fs::read(path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(&bytes[0..4], b"\0asm");
        // Should export "add_them"
        assert!(bytes.windows(8).any(|w| w == b"add_them"));
    }
}
