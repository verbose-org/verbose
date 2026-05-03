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

    // Locals layout (all i64):
    //   - params 0..nfields           = concept fields
    //   - locals nfields..+n_bindings = let bindings (in source order)
    //   - locals +0, +1               = scratch_a, scratch_b
    //
    // Scratch locals are reserved when the rule body uses one of the binary/
    // unary scalar primitives (`abs`, `min(a,b)`, `max(a,b)`). WASM has no
    // DUP — to evaluate a sub-expression once and reuse it, the value must
    // be parked in a local. Scratches are SHARED across all such call sites
    // because each emission writes both before reading either, so nesting
    // (e.g. `min(min(a,b), min(c,d))`) is safe: the outer min only reads its
    // scratches AFTER both sub-expressions have finished writing theirs.
    let n_bindings = rule.logic.bindings.len();
    let needs_scratch = expr_uses_scratch(&rule.logic.value)
        || rule.logic.bindings.iter().any(|(_, e)| expr_uses_scratch(e));
    let n_scratch = if needs_scratch { 2 } else { 0 };
    let n_locals = n_bindings + n_scratch;

    // WASM local declarations: vec(localdecl), each localdecl = (count, valtype).
    // We pack everything into a single i64 group when n_locals > 0; otherwise
    // emit zero groups.
    //
    // The previous encoding ("n_locals groups, first one has n_locals items")
    // worked by accident only for n=0 and n=1 — for n>=2 it produced an invalid
    // module that no test exercised. The single-group form below is the spec-
    // correct shape and degenerates to the prior bytes for n in {0, 1}.
    if n_locals > 0 {
        emit_leb128(&mut body, 1);                  // 1 declaration group
        emit_leb128(&mut body, n_locals as u64);    // group has n_locals items
        body.push(0x7E);                             // all i64
    } else {
        body.push(0);                                // 0 declaration groups
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
        Expr::Abs(inner) => {
            // Branchless abs via select. Evaluate `inner` exactly ONCE,
            // park it in scratch_a, then build:
            //   stack: [x, -x, (x>=0)i32]   →   select   →   [abs(x)]
            // i64.ge_s already returns i32 so no extend is needed for the
            // select condition (unlike the rule-output path that uniformly
            // widens comparisons via 0xAD for i64 chaining).
            let scratch_a = (nfields + rule.logic.bindings.len()) as u64;
            emit_wasm_expr(code, inner, rule, concept, all_rules)?;
            code.push(0x22); emit_leb128(code, scratch_a);   // local.tee scratch_a
            code.push(0x42); code.push(0x00);                // i64.const 0
            code.push(0x20); emit_leb128(code, scratch_a);   // local.get scratch_a
            code.push(0x7D);                                  // i64.sub  -> -x
            code.push(0x20); emit_leb128(code, scratch_a);   // local.get scratch_a
            code.push(0x42); code.push(0x00);                // i64.const 0
            code.push(0x59);                                  // i64.ge_s -> i32 cond
            code.push(0x1B);                                  // select
            Ok(())
        }
        Expr::Min(a, b) | Expr::Max(a, b) => {
            // Branchless binary scalar reduction via select.
            //
            // Evaluation discipline: a NESTED min/max/abs in `b` would
            // clobber scratch_a/scratch_b mid-computation, so we MUST
            // finish evaluating BOTH sub-expressions onto the stack
            // before parking either into a scratch local. The pop order
            // is then RIGHT-then-LEFT (LIFO):
            //   stack after evals : [..., a, b]
            //   set scratch_b     : pops b, $sb = b
            //   set scratch_a     : pops a, $sa = a
            // From here, only `local.get`s — no sub-evals — so $sa and
            // $sb remain stable through the four reads + select.
            //
            //   stack: [a, b, (a OP b)i32]  →  select  →  [a if cond else b]
            // For min: cond = (a < b)  (i64.lt_s = 0x53)
            // For max: cond = (a > b)  (i64.gt_s = 0x55)
            let is_max = matches!(expr, Expr::Max(_, _));
            let scratch_a = (nfields + rule.logic.bindings.len()) as u64;
            let scratch_b = scratch_a + 1;

            emit_wasm_expr(code, a, rule, concept, all_rules)?;     // [a]
            emit_wasm_expr(code, b, rule, concept, all_rules)?;     // [a, b]
            code.push(0x21); emit_leb128(code, scratch_b);           // pop b: $sb = b
            code.push(0x21); emit_leb128(code, scratch_a);           // pop a: $sa = a
            code.push(0x20); emit_leb128(code, scratch_a);           // [a]      val_1
            code.push(0x20); emit_leb128(code, scratch_b);           // [a, b]   val_2
            code.push(0x20); emit_leb128(code, scratch_a);           // [a, b, a]
            code.push(0x20); emit_leb128(code, scratch_b);           // [a, b, a, b]
            code.push(if is_max { 0x55 } else { 0x53 });             // i64.gt_s | i64.lt_s
            code.push(0x1B);                                          // select
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

/// True iff `expr` (or any sub-expression) needs the WASM scratch locals.
/// Today that means `Abs`, `Min`, or `Max` — primitives that must reuse a
/// sub-expression value but cannot DUP the stack.
///
/// Walked once before the locals declaration so the function header
/// reserves scratches up-front; missed cases would surface as
/// "unknown local" validation errors at module load.
fn expr_uses_scratch(expr: &Expr) -> bool {
    match expr {
        Expr::Abs(_) | Expr::Min(_, _) | Expr::Max(_, _) => true,
        // Recurse into compound shapes the WASM backend currently supports.
        Expr::Binary(_, l, r) => expr_uses_scratch(l) || expr_uses_scratch(r),
        Expr::If(c, t, e) => expr_uses_scratch(c) || expr_uses_scratch(t) || expr_uses_scratch(e),
        Expr::Not(inner) | Expr::Neg(inner) => expr_uses_scratch(inner),
        Expr::Call(_, args) => args.iter().any(expr_uses_scratch),
        // Leaves and shapes the backend doesn't yet emit.
        _ => false,
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
    termination:
      bound: 1
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
    termination:
      bound: 2
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

    /// Compile `src` (a Verbose source string) for `rule_name` and return
    /// the resulting WASM bytes. Centralised here so the W1 tests stay
    /// focused on bytecode shape.
    fn compile_to_bytes(src: &str, rule_name: &str, file_stem: &str) -> Vec<u8> {
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");
        let path = format!("/tmp/wasm_w1_{}.wasm", file_stem);
        compile_wasm(&program, rule_name, &path).expect("compile_wasm");
        let bytes = std::fs::read(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        bytes
    }

    /// W1: `abs(<number>)` is a single-eval branchless emission. The
    /// distinctive opcodes are `i64.ge_s` (0x59) immediately followed by
    /// `select` (0x1B) — together they pick `x` when non-negative and
    /// `-x` otherwise. Pinning that pair guards against silent regression
    /// to a re-eval-twice or branching shape.
    #[test]
    fn wasm_w1_abs_emits_select_with_ge_s_cond() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number
rule pick_abs
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    n = abs(t.x)
  proofs:
    purity:
      reads: [t.x]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "pick_abs", "abs");
        assert_eq!(&bytes[0..4], b"\0asm");
        // i64.ge_s (0x59) immediately before select (0x1B) is the abs signature.
        assert!(
            bytes.windows(2).any(|w| w == [0x59, 0x1B]),
            "abs should emit `i64.ge_s; select`"
        );
        // Negative regressions: a min/max emission would have a different cond.
        assert!(!bytes.windows(2).any(|w| w == [0x53, 0x1B]), "abs must not emit min cond");
        assert!(!bytes.windows(2).any(|w| w == [0x55, 0x1B]), "abs must not emit max cond");
    }

    /// W1: binary `min(a, b)` emits `i64.lt_s; select`. Asymmetry pinned
    /// here — if min and max ever silently swapped we'd notice.
    #[test]
    fn wasm_w1_min_emits_select_with_lt_s_cond() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule pick_min
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    n = min(t.a, t.b)
  proofs:
    purity:
      reads: [t.a, t.b]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "pick_min", "min");
        assert_eq!(&bytes[0..4], b"\0asm");
        assert!(
            bytes.windows(2).any(|w| w == [0x53, 0x1B]),
            "min should emit `i64.lt_s; select`"
        );
        assert!(!bytes.windows(2).any(|w| w == [0x55, 0x1B]), "min must not emit max cond");
    }

    /// W1: binary `max(a, b)` emits `i64.gt_s; select`.
    #[test]
    fn wasm_w1_max_emits_select_with_gt_s_cond() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule pick_max
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    n = max(t.a, t.b)
  proofs:
    purity:
      reads: [t.a, t.b]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "pick_max", "max");
        assert_eq!(&bytes[0..4], b"\0asm");
        assert!(
            bytes.windows(2).any(|w| w == [0x55, 0x1B]),
            "max should emit `i64.gt_s; select`"
        );
        assert!(!bytes.windows(2).any(|w| w == [0x53, 0x1B]), "max must not emit min cond");
    }

    /// W1: a rule with only let-bindings (no scratch primitives) keeps
    /// the previous locals-section bytes byte-for-byte. Two bindings
    /// would have triggered the latent encoding bug fixed in this slice
    /// (the prior code emitted "n groups, group 1 has n items" then ran
    /// out of bytes), so this test pins the corrected shape.
    #[test]
    fn wasm_w1_two_let_bindings_encode_locals_correctly() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number
rule double_then_inc
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    let doubled = t.x * 2
    let plus_one = doubled + 1
    n = plus_one
  proofs:
    purity:
      reads: [t.x]
      calls: []
    termination:
      bound: 3
"#;
        let bytes = compile_to_bytes(src, "double_then_inc", "two_lets");
        assert_eq!(&bytes[0..4], b"\0asm");
        // Look for the locals declaration "01 02 7E" = 1 group of 2 i64.
        // It must appear inside the code section; a bare scan of the
        // module bytes is sufficient because no other section embeds
        // that exact triple by coincidence in this minimal program.
        assert!(
            bytes.windows(3).any(|w| w == [0x01, 0x02, 0x7E]),
            "expected single-group locals decl `01 02 7E` (1 group, 2 i64) in module bytes:\n{:02X?}",
            bytes
        );
    }

    /// W1: a rule that mixes a let-binding AND a scratch-needing
    /// primitive should reserve `n_bindings + 2` locals. With one let
    /// and `min(...)`, that's `01 03 7E` (1 group of 3 i64). This pins
    /// the layout invariant that scratches sit AFTER bindings in the
    /// local-index space.
    #[test]
    fn wasm_w1_lets_plus_scratch_reserve_total_locals() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule clamp_low
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    let floor = 10
    n = max(floor, min(t.a, t.b))
  proofs:
    purity:
      reads: [t.a, t.b]
      calls: []
    termination:
      bound: 3
"#;
        let bytes = compile_to_bytes(src, "clamp_low", "lets_scratch");
        assert_eq!(&bytes[0..4], b"\0asm");
        assert!(
            bytes.windows(3).any(|w| w == [0x01, 0x03, 0x7E]),
            "expected locals decl `01 03 7E` (1 group, 3 i64 = 1 let + 2 scratch)"
        );
        // Both min and max signature pairs must appear (nested clamp).
        assert!(bytes.windows(2).any(|w| w == [0x53, 0x1B]), "missing min select");
        assert!(bytes.windows(2).any(|w| w == [0x55, 0x1B]), "missing max select");
    }

    /// W1: a rule with NO bindings and NO scratch primitives must still
    /// emit the zero-locals byte (`00`), not skip the field. The CLI
    /// has been relying on this implicit invariant — pinned for clarity.
    #[test]
    fn wasm_w1_no_locals_emits_zero_byte() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number
rule trivial
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    n = t.x + 1
  proofs:
    purity:
      reads: [t.x]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "trivial", "no_locals");
        assert_eq!(&bytes[0..4], b"\0asm");
        // The locals byte sits at the start of the function body inside
        // the code section. We don't decode the section here; instead we
        // pin that the abs/min/max scratch markers are absent (so the
        // detector wouldn't have inflated the count) and that none of
        // the multi-group locals patterns we'd expect for >0 locals
        // (`01 0N 7E` for small N) sneak in.
        for n in 1u8..=3 {
            assert!(
                !bytes.windows(3).any(|w| w == [0x01, n, 0x7E]),
                "trivial rule should not allocate {} locals",
                n
            );
        }
    }

    /// W1 end-to-end: run the compiled WASM in Node and check actual
    /// numeric outputs across nested scratch-using primitives. The
    /// bytecode-shape tests above pin the *opcodes*, but a real engine
    /// is the only thing that catches semantic timing bugs (e.g. a
    /// nested min/max overwriting an outer's scratch local
    /// mid-computation — exactly the bug fixed when this slice landed).
    ///
    /// Skipped silently if `node` is absent so contributors without it
    /// can still run `cargo test`. CI environments are expected to
    /// have node available.
    #[test]
    fn wasm_w1_runtime_nested_clamp_and_abs() {
        use std::process::Command;

        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("note: `node` not found, skipping WASM runtime test");
            return;
        }

        // Two rules over the same Pair concept, exercising:
        //   - max(literal, min(field, field))   — outer max + nested min
        //   - abs(field)                         — single-eval branchless
        // Both share scratch_a/scratch_b, so any cross-contamination
        // between outer and nested levels surfaces here.
        let src = r#"@verbose 0.1.0
concept Pair
  @intention: "two numbers"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule clamp_to_zero
  @intention: "max(0, min(a, b))"
  @source: invoices.intent:1
  input:
    p : Pair
  output:
    n : number
  logic:
    n = max(0, min(p.a, p.b))
  proofs:
    purity:
      reads : [p.a, p.b]
      calls : []
    termination:
      bound : 2
rule abs_of_a
  @intention: "abs(a)"
  @source: invoices.intent:1
  input:
    p : Pair
  output:
    n : number
  logic:
    n = abs(p.a)
  proofs:
    purity:
      reads : [p.a]
      calls : []
    termination:
      bound : 1
"#;
        let clamp_path = "/tmp/wasm_w1_runtime_clamp.wasm";
        let abs_path = "/tmp/wasm_w1_runtime_abs.wasm";

        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");
        compile_wasm(&program, "clamp_to_zero", clamp_path).expect("compile clamp");
        compile_wasm(&program, "abs_of_a", abs_path).expect("compile abs");

        // One Node run, one assertion: a JS harness loads both modules
        // and prints OK or FAIL lines for each case. We then assert no
        // FAIL appears in the output.
        let script = format!(
            r#"
const fs = require("fs");
async function check(path, fn, args, expected) {{
  const buf = fs.readFileSync(path);
  if (!WebAssembly.validate(buf)) {{ console.log("FAIL: invalid module " + path); return; }}
  const m = await WebAssembly.instantiate(buf);
  const got = m.instance.exports[fn](...args);
  const gotStr = typeof got === "bigint" ? got.toString() : String(got);
  if (gotStr === String(expected)) {{
    console.log("OK " + fn + "(" + args.join(",") + ") = " + gotStr);
  }} else {{
    console.log("FAIL " + fn + "(" + args.join(",") + ") = " + gotStr + ", expected " + expected);
  }}
}}
(async () => {{
  // clamp_to_zero(a, b) = max(0, min(a, b))
  await check("{clamp_path}", "clamp_to_zero", [5n, 7n], 5);          // both positive, in-range
  await check("{clamp_path}", "clamp_to_zero", [-3n, 7n], 0);          // negative inner, clamped up
  await check("{clamp_path}", "clamp_to_zero", [-3n, -10n], 0);        // both negative
  await check("{clamp_path}", "clamp_to_zero", [100n, 50n], 50);       // pins the inner min — this case caught the bug
  await check("{clamp_path}", "clamp_to_zero", [0n, 100n], 0);
  // abs_of_a(a, _)
  await check("{abs_path}", "abs_of_a", [42n, 0n], 42);
  await check("{abs_path}", "abs_of_a", [-42n, 0n], 42);
  await check("{abs_path}", "abs_of_a", [0n, 0n], 0);
  await check("{abs_path}", "abs_of_a", [-9223372036854775807n, 0n], 9223372036854775807n);
}})();
"#
        );

        let out = Command::new("node")
            .args(["-e", &script])
            .output()
            .expect("spawn node");
        let stdout = String::from_utf8_lossy(&out.stdout);

        let _ = std::fs::remove_file(clamp_path);
        let _ = std::fs::remove_file(abs_path);

        assert!(
            !stdout.contains("FAIL"),
            "WASM runtime check failed; node stdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&out.stderr)
        );
        // Sanity: at least one OK line so we didn't silently no-op.
        assert!(stdout.contains("OK"), "no OK lines from node; stdout:\n{}", stdout);
    }
}
