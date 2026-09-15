//! Invocation-owned storage for the checked, acyclic text subset.
//!
//! All offsets are assigned before execution. A let binds a value descriptor,
//! never a substituted expression. Calls expand once per call site. Text result
//! destinations pass through tail calls and conditionals to the final producer.
//! Other temporaries remain distinct and live for the entire invocation.
use super::*;

const FRAME_LIMIT: usize = 2 * 1024 * 1024;
// Saved rbp/rbx and the largest temporary used by numeric output/formatting.
const FIXED_SCRATCH: usize = 16 + 32;

#[derive(Clone, Debug)]
enum Value {
    Number(i32),
    Bool(i32),
    Text { ptr: i32, len: i32, cap: usize },
    Input,
    Record(String, Vec<(String, Value)>),
}
// A fresh result destination is never put in a lexical environment while it is
// being filled. Only output-position calls/branches may receive it; lets and
// concat operands keep their own immutable storage. Its pointer slot is set
// before evaluating the rule and never overwritten by a producer.
#[derive(Clone, Copy)]
struct TextDestination {
    ptr: i32,
    len: i32,
}
impl TextDestination {
    fn value(self, cap: usize) -> Value {
        Value::Text {
            ptr: self.ptr,
            len: self.len,
            cap,
        }
    }
}
impl Value {
    fn scalar(&self) -> Result<i32, NativeError> {
        match self {
            Self::Number(s) | Self::Bool(s) => Ok(*s),
            _ => Err(error("expected a scalar storage slot")),
        }
    }
    fn capacity(&self) -> Result<usize, NativeError> {
        match self {
            Self::Text { cap, .. } => Ok(*cap),
            Self::Number(_) => Ok(20),
            _ => Err(error("expected text or number storage")),
        }
    }
}
fn error(message: impl Into<String>) -> NativeError {
    NativeError {
        message: format!("bounded text storage: {}", message.into()),
    }
}
fn jump(code: &mut Vec<u8>, opcode: &[u8]) -> usize {
    code.extend_from_slice(opcode);
    let site = code.len();
    code.extend_from_slice(&[0; 4]);
    site
}
fn patch(code: &mut [u8], site: usize, target: usize) {
    code[site..site + 4].copy_from_slice(&((target as i32) - (site as i32) - 4).to_le_bytes());
}
// Only low registers are used here; all accesses use disp32 for one encoding.
fn load(code: &mut Vec<u8>, reg: u8, slot: i32) {
    code.extend_from_slice(&[0x48, 0x8b, 0x85 | (reg << 3)]);
    code.extend_from_slice(&slot.to_le_bytes());
}
fn store(code: &mut Vec<u8>, reg: u8, slot: i32) {
    code.extend_from_slice(&[0x48, 0x89, 0x85 | (reg << 3)]);
    code.extend_from_slice(&slot.to_le_bytes());
}
fn address(code: &mut Vec<u8>, reg: u8, slot: i32) {
    code.extend_from_slice(&[0x48, 0x8d, 0x85 | (reg << 3)]);
    code.extend_from_slice(&slot.to_le_bytes());
}
fn outer_load(code: &mut Vec<u8>, reg: u8, slot: i32) {
    // mov reg, [r10 + disp32]
    code.extend_from_slice(&[0x49, 0x8b, 0x82 | (reg << 3)]);
    code.extend_from_slice(&slot.to_le_bytes());
}
fn outer_store(code: &mut Vec<u8>, reg: u8, slot: i32) {
    code.extend_from_slice(&[0x49, 0x89, 0x82 | (reg << 3)]);
    code.extend_from_slice(&slot.to_le_bytes());
}

pub(super) struct Fragment {
    code: Vec<u8>,
    fields: Vec<(String, Value)>,
    result: Value,
    frame_bytes: usize,
    constants: HashMap<i32, i64>,
}
struct Emit<'a> {
    code: Vec<u8>,
    rules: HashMap<&'a str, &'a Rule>,
    concept: &'a Concept,
    fields: Vec<(String, Value)>,
    frame_bytes: usize,
    nodes: usize,
    literal_bytes: usize,
    constants: HashMap<i32, i64>,
}
impl Emit<'_> {
    fn allocate(&mut self, bytes: usize) -> Result<i32, NativeError> {
        let bytes = bytes
            .checked_add(7)
            .map(|n| n & !7)
            .ok_or_else(|| error("frame size overflow"))?;
        self.frame_bytes = self
            .frame_bytes
            .checked_add(bytes)
            .ok_or_else(|| error("frame size overflow"))?;
        if self.frame_bytes + FIXED_SCRATCH > FRAME_LIMIT {
            return Err(error(format!("invocation frame exceeds {FRAME_LIMIT} bytes (needs {} including slots, buffers and fixed scratch)", self.frame_bytes + FIXED_SCRATCH)));
        }
        Ok(-(self.frame_bytes as i32))
    }
    fn scalar(&mut self, boolean: bool) -> Result<Value, NativeError> {
        let slot = self.allocate(8)?;
        store(&mut self.code, 0, slot);
        Ok(if boolean {
            Value::Bool(slot)
        } else {
            Value::Number(slot)
        })
    }
    fn text(&mut self, cap: usize) -> Result<Value, NativeError> {
        Ok(Value::Text {
            ptr: self.allocate(8)?,
            len: self.allocate(8)?,
            cap,
        })
    }
    fn save_pair(&mut self, value: &Value) {
        if let Value::Text { ptr, len, .. } = value {
            store(&mut self.code, 0, *ptr);
            store(&mut self.code, 2, *len);
        }
    }
    fn finish_jump(&mut self, site: usize) {
        let end = self.code.len();
        patch(&mut self.code, site, end);
    }
    fn field(&mut self, name: &str) -> Result<Value, NativeError> {
        if let Some((_, v)) = self.fields.iter().find(|(n, _)| n == name) {
            return Ok(v.clone());
        }
        let f = self
            .concept
            .fields
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| error("unknown input field"))?;
        let value = match f.ty {
            Type::Number => Value::Number(self.allocate(8)?),
            Type::Text => self.text(
                f.range
                    .and_then(|(_, n)| usize::try_from(n).ok())
                    .ok_or_else(|| error("unknown input text capacity"))?,
            )?,
            _ => return Err(error("unsupported input field")),
        };
        self.fields.push((name.into(), value.clone()));
        Ok(value)
    }
    fn copy_text(&mut self, value: Value, dest: TextDestination) -> Result<Value, NativeError> {
        let Value::Text { ptr, len, cap } = value else {
            return Err(error("expected text result"));
        };
        if ptr == dest.ptr && len == dest.len {
            return Ok(value);
        }
        load(&mut self.code, 7, dest.ptr);
        load(&mut self.code, 6, ptr);
        load(&mut self.code, 1, len);
        self.code.extend_from_slice(&[0xfc, 0xf3, 0xa4]); // cld; rep movsb
        load(&mut self.code, 2, len);
        store(&mut self.code, 2, dest.len);
        Ok(dest.value(cap))
    }
    fn rule(
        &mut self,
        rule: &Rule,
        depth: usize,
        destination: Option<TextDestination>,
    ) -> Result<Value, NativeError> {
        if depth > 256 {
            return Err(error("native call expansion limit exceeded (256 levels)"));
        }
        // Inferred capacities become known while emitting the body. Reserve
        // the pair now and patch the buffer's displacement after that analysis;
        // all frame allocation still happens before runtime evaluation begins.
        let mut buffer_site = None;
        let destination = if rule.output_ty == Type::Text && destination.is_none() {
            let dest = TextDestination {
                ptr: self.allocate(8)?,
                len: self.allocate(8)?,
            };
            address(&mut self.code, 0, 0);
            buffer_site = Some(self.code.len() - 4);
            store(&mut self.code, 0, dest.ptr);
            Some(dest)
        } else {
            destination
        };
        let mut env = HashMap::from([(rule.input_name.clone(), Value::Input)]);
        for (name, rhs) in &rule.logic.bindings {
            let v = self.expr(rhs, &env, depth + 1).map_err(|e| {
                error(format!(
                    "rule '{}' / let '{name}': {}",
                    rule.name, e.message
                ))
            })?;
            env.insert(name.clone(), v);
        }
        let value = self
            .expr_into(&rule.logic.value, &env, depth + 1, destination)
            .map_err(|e| error(format!("rule '{}' / output: {}", rule.name, e.message)))?;
        let value = if matches!(value, Value::Input) {
            let fields: Vec<_> = self.concept.fields.iter().map(|f| f.name.clone()).collect();
            Value::Record(
                self.concept.name.clone(),
                fields
                    .into_iter()
                    .map(|n| Ok((n.clone(), self.field(&n)?)))
                    .collect::<Result<_, NativeError>>()?,
            )
        } else {
            value
        };
        if let Value::Text { cap, .. } = value {
            let cap = rule.output_text_max.map_or(cap, |n| n as usize);
            if let Some(site) = buffer_site {
                let buffer = self
                    .allocate(cap)
                    .map_err(|e| error(format!("rule '{}' / output: {}", rule.name, e.message)))?;
                self.code[site..site + 4].copy_from_slice(&buffer.to_le_bytes());
            }
            Ok(destination
                .ok_or_else(|| error("missing text destination"))?
                .value(cap))
        } else {
            Ok(value)
        }
    }
    fn expr(
        &mut self,
        e: &Expr,
        env: &HashMap<String, Value>,
        depth: usize,
    ) -> Result<Value, NativeError> {
        self.expr_into(e, env, depth, None)
    }
    fn expr_into(
        &mut self,
        e: &Expr,
        env: &HashMap<String, Value>,
        depth: usize,
        destination: Option<TextDestination>,
    ) -> Result<Value, NativeError> {
        self.nodes += 1;
        if self.nodes > 100_000 || depth > 256 {
            return Err(error(
                "native call expansion limit exceeded (100000 nodes / 256 levels)",
            ));
        }
        let value = match e {
            Expr::Ident(n) => env
                .get(n)
                .cloned()
                .ok_or_else(|| error(format!("unknown binding '{n}'")))?,
            Expr::Number(n) => {
                emit_mov_rax_imm(&mut self.code, *n);
                let value = self.scalar(false)?;
                self.constants.insert(value.scalar()?, *n);
                value
            }
            Expr::Neg(n) => {
                let v = self.expr(n, env, depth + 1)?;
                load(&mut self.code, 0, v.scalar()?);
                self.code.extend_from_slice(&[0x48, 0xf7, 0xd8]);
                let value = self.scalar(false)?;
                if let Some(n) = self
                    .constants
                    .get(&v.scalar()?)
                    .and_then(|n| n.checked_neg())
                {
                    self.constants.insert(value.scalar()?, n);
                }
                value
            }
            Expr::Text(s) => {
                self.literal_bytes += s.len();
                if self.literal_bytes > 16 * 1024 * 1024 {
                    return Err(error("native literal expansion exceeds 16 MiB"));
                }
                let value = self.text(s.len())?;
                self.code.push(0xe9);
                self.code.extend_from_slice(&(s.len() as i32).to_le_bytes());
                let data = self.code.len();
                self.code.extend_from_slice(s.as_bytes());
                let displacement = data as i32 - self.code.len() as i32 - 7;
                self.code.extend_from_slice(&[0x48, 0x8d, 0x05]);
                self.code.extend_from_slice(&displacement.to_le_bytes());
                self.code.extend_from_slice(&[0x48, 0xc7, 0xc2]);
                self.code.extend_from_slice(&(s.len() as i32).to_le_bytes());
                self.save_pair(&value);
                value
            }
            Expr::Field(base, name) => match self.expr(base, env, depth + 1)? {
                Value::Input => self.field(name)?,
                Value::Record(_, fields) => {
                    fields
                        .into_iter()
                        .find(|(n, _)| n == name)
                        .ok_or_else(|| error("unknown record field"))?
                        .1
                }
                _ => return Err(error("field access requires a record")),
            },
            Expr::Call(name, _) => {
                let rule = *self
                    .rules
                    .get(name.as_str())
                    .ok_or_else(|| error("unknown callee"))?;
                self.rule(rule, depth + 1, destination)?
            }
            Expr::Concat(args) => {
                // Evaluate every argument exactly once, in source order, before
                // filling the destination. A nested concat has its own buffer.
                let values: Vec<_> = args
                    .iter()
                    .map(|e| self.expr(e, env, depth + 1))
                    .collect::<Result<_, _>>()?;
                let cap = values.iter().try_fold(0usize, |n, v| {
                    n.checked_add(v.capacity()?)
                        .ok_or_else(|| error("capacity overflow"))
                })?;
                let dest = if let Some(dest) = destination {
                    dest
                } else {
                    let dest = TextDestination {
                        ptr: self.allocate(8)?,
                        len: self.allocate(8)?,
                    };
                    let buffer = self.allocate(cap)?;
                    address(&mut self.code, 0, buffer);
                    store(&mut self.code, 0, dest.ptr);
                    dest
                };
                load(&mut self.code, 3, dest.ptr); // rbx: write cursor
                for arg in values {
                    match arg {
                        Value::Number(s) => {
                            load(&mut self.code, 0, s);
                            emit_itoa_to_buffer(&mut self.code);
                        }
                        Value::Text { ptr, len, .. } => {
                            load(&mut self.code, 6, ptr);
                            load(&mut self.code, 1, len);
                            self.code.extend_from_slice(&[
                                0x48, 0x89, 0xdf, 0xfc, 0xf3, 0xa4, 0x48, 0x89, 0xfb,
                            ]);
                        }
                        _ => return Err(error("unsupported concat argument")),
                    }
                }
                load(&mut self.code, 0, dest.ptr);
                self.code
                    .extend_from_slice(&[0x48, 0x89, 0xda, 0x48, 0x29, 0xc2]); // len = rbx - rax
                store(&mut self.code, 2, dest.len);
                dest.value(cap)
            }
            Expr::If(c, a, b) => self.conditional(c, a, b, env, depth, destination)?,
            Expr::Binary(op @ (BinOp::And | BinOp::Or), a, b) => {
                let a = self.expr(a, env, depth + 1)?;
                load(&mut self.code, 0, a.scalar()?);
                let dest = self.scalar(true)?;
                self.code.extend_from_slice(&[0x48, 0x85, 0xc0]);
                let done = jump(
                    &mut self.code,
                    &[0x0f, if *op == BinOp::And { 0x84 } else { 0x85 }],
                );
                let b = self.expr(b, env, depth + 1)?;
                self.move_value(&b, &dest)?;
                self.finish_jump(done);
                dest
            }
            Expr::Binary(op, a, b) => {
                let a = self.expr(a, env, depth + 1)?;
                let b = self.expr(b, env, depth + 1)?;
                if let (
                    Value::Text {
                        ptr: ap, len: al, ..
                    },
                    Value::Text {
                        ptr: bp, len: bl, ..
                    },
                ) = (&a, &b)
                {
                    load(&mut self.code, 0, *al);
                    load(&mut self.code, 1, *bl);
                    self.code.extend_from_slice(&[0x48, 0x39, 0xc8]);
                    let unequal = jump(&mut self.code, &[0x0f, 0x85]);
                    // Equal lengths, including zero: ZF from the length compare
                    // is preserved when repe cmpsb executes zero iterations.
                    load(&mut self.code, 6, *ap);
                    load(&mut self.code, 7, *bp);
                    self.code.extend_from_slice(&[0xfc, 0xf3, 0xa6]);
                    self.finish_jump(unequal);
                } else {
                    load(&mut self.code, 0, a.scalar()?);
                    load(&mut self.code, 1, b.scalar()?);
                    self.code.extend_from_slice(&[0x48, 0x39, 0xc8]);
                }
                let condition = match op {
                    BinOp::Eq => 0x94,
                    BinOp::NotEq => 0x95,
                    BinOp::Lt => 0x9c,
                    BinOp::GtEq => 0x9d,
                    BinOp::LtEq => 0x9e,
                    BinOp::Gt => 0x9f,
                    _ => return Err(error("unsupported comparison")),
                };
                self.code
                    .extend_from_slice(&[0x0f, condition, 0xc0, 0x48, 0x0f, 0xb6, 0xc0]);
                self.scalar(true)?
            }
            Expr::Not(e) => {
                let v = self.expr(e, env, depth + 1)?;
                load(&mut self.code, 0, v.scalar()?);
                self.code.extend_from_slice(&[0x48, 0x83, 0xf0, 0x01]);
                self.scalar(true)?
            }
            Expr::Length(e) => {
                let Value::Text { len, .. } = self.expr(e, env, depth + 1)? else {
                    return Err(error("length requires text"));
                };
                load(&mut self.code, 0, len);
                self.scalar(false)?
            }
            Expr::Record(n, fields) => Value::Record(
                n.clone(),
                fields
                    .iter()
                    .map(|(n, e)| Ok((n.clone(), self.expr(e, env, depth + 1)?)))
                    .collect::<Result<_, NativeError>>()?,
            ),
            _ => return Err(error("expression outside the checked subset")),
        };
        if let Some(dest) = destination {
            self.copy_text(value, dest)
        } else {
            Ok(value)
        }
    }
    fn conditional(
        &mut self,
        c: &Expr,
        a: &Expr,
        b: &Expr,
        env: &HashMap<String, Value>,
        depth: usize,
        destination: Option<TextDestination>,
    ) -> Result<Value, NativeError> {
        let c = self.expr(c, env, depth + 1)?;
        load(&mut self.code, 0, c.scalar()?);
        self.code.extend_from_slice(&[0x48, 0x85, 0xc0]);
        let otherwise = jump(&mut self.code, &[0x0f, 0x84]);
        let a = self.expr_into(a, env, depth + 1, destination)?;
        let mut dest = match (&a, destination) {
            (Value::Text { cap, .. }, Some(dest)) => dest.value(*cap),
            (Value::Text { cap, .. }, None) => self.text(*cap)?,
            (Value::Bool(_), None) => Value::Bool(self.allocate(8)?),
            (Value::Number(_), None) => Value::Number(self.allocate(8)?),
            _ => return Err(error("conditional record storage is unsupported")),
        };
        if destination.is_none() {
            self.move_value(&a, &dest)?;
        }
        let done = jump(&mut self.code, &[0xe9]);
        self.finish_jump(otherwise);
        let b = self.expr_into(b, env, depth + 1, destination)?;
        if destination.is_none() {
            self.move_value(&b, &dest)?;
        }
        if let (Value::Text { cap, .. }, Value::Text { cap: other, .. }) = (&mut dest, b) {
            *cap = (*cap).max(other);
        }
        self.finish_jump(done);
        Ok(dest)
    }
    fn move_value(&mut self, from: &Value, to: &Value) -> Result<(), NativeError> {
        if let Value::Text { ptr, len, .. } = from {
            load(&mut self.code, 0, *ptr);
            load(&mut self.code, 2, *len);
            self.save_pair(to);
        } else {
            load(&mut self.code, 0, from.scalar()?);
            store(&mut self.code, 0, to.scalar()?);
        }
        Ok(())
    }
}

pub(super) fn prepare(p: &Program, name: &str, concept: &Concept) -> Result<Fragment, NativeError> {
    if let Some(e) = crate::text_bounds::verify_mode(p, true).first() {
        return Err(error(e.to_string()));
    }
    let rules: HashMap<_, _> = p
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some((r.name.as_str(), r)),
            _ => None,
        })
        .collect();
    let rule = *rules.get(name).ok_or_else(|| error("entry rule missing"))?;
    let mut emit = Emit {
        code: Vec::new(),
        rules,
        concept,
        fields: Vec::new(),
        frame_bytes: 0,
        nodes: 0,
        literal_bytes: 0,
        constants: HashMap::new(),
    };
    let result = emit.rule(rule, 0, None)?;
    Ok(Fragment {
        code: emit.code,
        fields: emit.fields,
        result,
        frame_bytes: emit.frame_bytes,
        constants: emit.constants,
    })
}

impl Fragment {
    pub(super) fn uses_field(&self, name: &str) -> bool {
        self.fields.iter().any(|(n, _)| n == name)
    }
    // The fragment runs below the existing input/transport frame. Its buffers
    // remain live until the consumer completes; only then may rsp be reset.
    fn begin(
        &self,
        code: &mut Vec<u8>,
        offsets: &HashMap<&str, i32>,
        text: &TextBindings<'_>,
    ) -> Result<(), NativeError> {
        code.extend_from_slice(&[
            0x55, 0x53, 0x49, 0x89, 0xea, 0x48, 0x89, 0xe5, 0x48, 0x81, 0xec,
        ]);
        code.extend_from_slice(&(self.frame_bytes as i32).to_le_bytes());
        for (name, value) in &self.fields {
            match value {
                Value::Number(slot) => {
                    outer_load(
                        code,
                        0,
                        *offsets.get(name.as_str()).ok_or_else(|| {
                            error(format!("input field '{name}' has no native slot"))
                        })?,
                    );
                    store(code, 0, *slot);
                }
                Value::Text { ptr, len, .. } => {
                    // HTTP body supplies a counted pair (NUL is data). Other
                    // input modes retain their existing NUL-terminated contract.
                    let pair = text
                        .get(format!("__req_{name}").as_str())
                        .or_else(|| text.get(name.as_str()));
                    if let Some((p, n)) = pair {
                        outer_load(code, 0, *p);
                        store(code, 0, *ptr);
                        outer_load(code, 0, *n);
                        store(code, 0, *len);
                    } else {
                        outer_load(
                            code,
                            6,
                            *offsets.get(name.as_str()).ok_or_else(|| {
                                error(format!("input field '{name}' has no native slot"))
                            })?,
                        );
                        store(code, 6, *ptr);
                        emit_strlen(code);
                        store(code, 2, *len);
                    }
                }
                _ => return Err(error("unsupported input storage")),
            }
        }
        code.extend_from_slice(&self.code);
        Ok(())
    }
    fn end(code: &mut Vec<u8>) {
        code.extend_from_slice(&[0x48, 0x89, 0xec, 0x5b, 0x5d]);
    }
    pub(super) fn http(
        &self,
        code: &mut Vec<u8>,
        offsets: &HashMap<&str, i32>,
        text: &TextBindings<'_>,
    ) -> Result<(), NativeError> {
        let Value::Record(name, fields) = &self.result else {
            return Err(error("HTTP handler must return HttpResponse"));
        };
        if name != "HttpResponse" {
            return Err(error("HTTP handler must return HttpResponse"));
        }
        let status = fields
            .iter()
            .find(|(n, _)| n == "status")
            .ok_or_else(|| error("missing response status"))?
            .1
            .scalar()?;
        if let Some(n) = self.constants.get(&status) {
            if !(100..=599).contains(n) {
                return Err(error(format!(
                    "status {n} outside HTTP valid range [100, 599]"
                )));
            }
        }
        let Value::Text { ptr, len, .. } = &fields
            .iter()
            .find(|(n, _)| n == "body")
            .ok_or_else(|| error("missing response body"))?
            .1
        else {
            return Err(error("response body must be text"));
        };
        self.begin(code, offsets, text)?;
        // Reload the transport frame and publish only while our storage is live.
        code.extend_from_slice(&[0x4c, 0x8b, 0x55, 0x08]);
        for (source, dest) in [(status, -24), (*ptr, -32), (*len, -40)] {
            load(code, 0, source);
            outer_store(code, 0, dest);
        }
        // Restore caller registers but retain rsp below our buffers until send
        // completes. The existing accept-loop reset releases them on all paths.
        load(code, 3, 0);
        code.extend_from_slice(&[0x4c, 0x89, 0xd5]);
        Ok(())
    }
}

pub(super) fn compile(p: &Program, rule: &Rule, concept: &Concept) -> Result<Vec<u8>, NativeError> {
    let fragment = prepare(p, &rule.name, concept)?;
    let mut input = rule.clone();
    input.logic.bindings.clear();
    input.logic.value = Expr::Number(0);
    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(
        &mut code,
        &input,
        concept,
        None,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        None,
        false,
    )?;
    fragment.begin(&mut code, &ctx.binding_offsets, &ctx.text_bindings)?;
    match &fragment.result {
        Value::Text { ptr, len, .. } => {
            load(&mut code, 6, *ptr);
            load(&mut code, 2, *len);
            emit_mov_rax_imm(&mut code, 1);
            code.extend_from_slice(&[0xbf, 1, 0, 0, 0, 0x0f, 0x05]);
            emit_write_newline(&mut code, 1);
        }
        Value::Number(slot) => {
            load(&mut code, 0, *slot);
            emit_itoa_inline(&mut code);
        }
        Value::Bool(slot) => {
            // Bool exit status belongs to the outer record loop's frame.
            load(&mut code, 0, *slot);
            Fragment::end(&mut code);
            code.extend_from_slice(&[0x84, 0xc0]);
            emit_bool_arms(&mut code, BoolExitFlag::Slot(ctx.exit_flag_slot));
        }
        Value::Record(name, fields) => {
            let output = iter_all_concepts(&p.items)
                .find(|c| c.name == *name)
                .ok_or_else(|| error("output concept missing"))?;
            let mut offsets = HashMap::new();
            let mut text = HashMap::new();
            let mut exprs = Vec::new();
            for (name, value) in fields {
                match value {
                    Value::Number(s) => {
                        offsets.insert(name.as_str(), *s);
                    }
                    Value::Text { ptr, len, .. } => {
                        text.insert(name.as_str(), (*ptr, *len));
                    }
                    _ => return Err(error("native record output supports number/text fields")),
                }
                exprs.push((name.clone(), Expr::Ident(name.clone())));
            }
            emit_record_as_json(
                &mut code,
                &exprs,
                output,
                &rule.input_name,
                concept,
                &HashMap::new(),
                &offsets,
                &HashMap::new(),
                &text,
            )?;
        }
        Value::Input => return Err(error("returning the whole input is unsupported natively")),
    }
    if !matches!(fragment.result, Value::Bool(_)) {
        Fragment::end(&mut code);
    }
    let back = jump(&mut code, &[0xe9]);
    patch(&mut code, back, ctx.loop_top);
    emit_record_loop_epilogue(&mut code, &ctx);
    Ok(code)
}
