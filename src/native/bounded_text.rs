//! Invocation-owned storage for the checked, acyclic text subset.
//!
//! All offsets are assigned before execution. A let binds a value descriptor,
//! never a substituted expression. Calls expand once per call site. Text result
//! destinations pass through tail calls and conditionals to the final producer.
//! Buffer placement follows proved last uses, including aliased branch results.
mod slots;
mod storage;
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
    fn slots(&self, out: &mut Vec<i32>) {
        match self {
            Self::Number(s) | Self::Bool(s) => out.push(*s),
            Self::Text { ptr, len, .. } => out.extend([*ptr, *len]),
            Self::Record(_, fields) => {
                for (_, value) in fields {
                    value.slots(out);
                }
            }
            Self::Input => {}
        }
    }
    fn relocate(&mut self, layout: &slots::Layout) -> Result<(), NativeError> {
        match self {
            Self::Number(s) | Self::Bool(s) => *s = layout.offset(*s)?,
            Self::Text { ptr, len, .. } => {
                *ptr = layout.offset(*ptr)?;
                *len = layout.offset(*len)?;
            }
            Self::Record(_, fields) => {
                for (_, value) in fields {
                    value.relocate(layout)?;
                }
            }
            Self::Input => return Err(error("unmaterialized input during slot placement")),
        }
        Ok(())
    }
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
fn load(code: &mut impl slots::Buffer, reg: u8, slot: i32) {
    let bytes = code.bytes();
    bytes.extend_from_slice(&[0x48, 0x8b, 0x85 | (reg << 3)]);
    let site = bytes.len();
    bytes.extend_from_slice(&slot.to_le_bytes());
    code.operand(slot, site);
}
fn store(code: &mut impl slots::Buffer, reg: u8, slot: i32) {
    let bytes = code.bytes();
    bytes.extend_from_slice(&[0x48, 0x89, 0x85 | (reg << 3)]);
    let site = bytes.len();
    bytes.extend_from_slice(&slot.to_le_bytes());
    code.operand(slot, site);
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
    slot_bytes: usize,
    expression_stack_bytes: usize,
    http_status_range: Option<(i64, i64)>,
    calls: Vec<crate::stack_budget::CallStorage>,
}
struct Emit<'a> {
    code: slots::Code,
    rules: HashMap<&'a str, &'a Rule>,
    concept: &'a Concept,
    fields: Vec<(String, Value)>,
    expression_stack_bytes: usize,
    nodes: usize,
    literal_bytes: usize,
    // Envelopes of known literal alternatives, solely for HTTP diagnostics.
    // Unknown alternatives are not represented: this is not an interval proof.
    literal_ranges: HashMap<i32, (i64, i64)>,
    storage: storage::Storage,
}
impl Emit<'_> {
    fn scalar(&mut self, boolean: bool) -> Result<Value, NativeError> {
        let slot = self.code.slot()?;
        store(&mut self.code, 0, slot);
        Ok(if boolean {
            Value::Bool(slot)
        } else {
            Value::Number(slot)
        })
    }
    fn text(&mut self, cap: usize) -> Result<Value, NativeError> {
        let ptr = self.code.slot()?;
        self.storage.pointer(ptr);
        Ok(Value::Text {
            ptr,
            len: self.code.slot()?,
            cap,
        })
    }
    fn destination(
        &mut self,
        cap: Option<usize>,
        context: String,
    ) -> Result<TextDestination, NativeError> {
        let Value::Text { ptr, len, .. } = self.text(0)? else {
            unreachable!()
        };
        self.storage.reserve(ptr, cap, context, &mut self.code)?;
        store(&mut self.code, 0, ptr);
        Ok(TextDestination { ptr, len })
    }
    fn use_value(&mut self, value: &Value) -> Result<(), NativeError> {
        match value {
            Value::Text { ptr, .. } => self.storage.touch(*ptr)?,
            Value::Record(_, fields) => {
                for (_, value) in fields {
                    self.use_value(value)?;
                }
            }
            _ => {}
        }
        Ok(())
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
            Type::Number => Value::Number(self.code.slot()?),
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
        self.use_value(&value)?;
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
        input: Value,
        depth: usize,
        destination: Option<TextDestination>,
    ) -> Result<Value, NativeError> {
        if depth > 256 {
            return Err(error("native call expansion limit exceeded (256 levels)"));
        }
        // Capacities and last uses are known before final buffer placement.
        let owns_destination = rule.output_ty == Type::Text && destination.is_none();
        let destination = if owns_destination {
            Some(self.destination(None, format!("rule '{}' / output", rule.name))?)
        } else {
            destination
        };
        let mut env = HashMap::from([(rule.input_name.clone(), input)]);
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
        let value = self.materialize_input(value)?;
        if let Value::Text { cap, .. } = value {
            let cap = rule.output_text_max.map_or(cap, |n| n as usize);
            let dest = destination.ok_or_else(|| error("missing text destination"))?;
            if owns_destination {
                self.storage.capacity(dest.ptr, cap)?;
            }
            Ok(dest.value(cap))
        } else {
            Ok(value)
        }
    }
    fn materialize_input(&mut self, value: Value) -> Result<Value, NativeError> {
        Ok(if matches!(value, Value::Input) {
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
        })
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
                self.literal_ranges.insert(value.scalar()?, (*n, *n));
                value
            }
            Expr::Neg(n) => {
                let v = self.expr(n, env, depth + 1)?;
                load(&mut self.code, 0, v.scalar()?);
                self.code.extend_from_slice(&[0x48, 0xf7, 0xd8]);
                let value = self.scalar(false)?;
                if let Some(range) = self
                    .literal_ranges
                    .get(&v.scalar()?)
                    .and_then(|(lo, hi)| Some((hi.checked_neg()?, lo.checked_neg()?)))
                {
                    self.literal_ranges.insert(value.scalar()?, range);
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
            Expr::Call(name, args) => {
                let [arg] = args.as_slice() else {
                    return Err(error("call requires exactly one record input"));
                };
                // Evaluate all constructed fields before entering the callee.
                // Descriptors retain the original storage owners, including
                // across callee lets, returned records and later alias uses.
                let input = self.expr(arg, env, depth + 1)?;
                let rule = *self
                    .rules
                    .get(name.as_str())
                    .ok_or_else(|| error("unknown callee"))?;
                let call = self.storage.begin_call(name);
                let value = self.rule(rule, input, depth + 1, destination)?;
                self.storage.end_call(call)?;
                value
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
                    self.destination(Some(cap), "concat".into())?
                };
                load(&mut self.code, 3, dest.ptr); // rbx: write cursor
                for arg in values {
                    // Earlier operands remain live across all later operand
                    // evaluations and the destination's first write.
                    self.use_value(&arg)?;
                    match arg {
                        Value::Number(s) => {
                            load(&mut self.code, 0, s);
                            emit_itoa_to_buffer(&mut self.code);
                            // Operands are evaluated into slots before filling
                            // the buffer. Each conversion restores its scratch.
                            self.expression_stack_bytes = ITOA_STACK_BYTES as usize;
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
            Expr::Binary(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod), a, b) => {
                let a = self.expr(a, env, depth + 1)?;
                let b = self.expr(b, env, depth + 1)?;
                // Both values stay live across RHS evaluation and until their
                // register loads. Their proved domain excludes overflow/traps.
                load(&mut self.code, 0, a.scalar()?);
                load(&mut self.code, 1, b.scalar()?);
                match op {
                    BinOp::Add => self.code.extend_from_slice(&[0x48, 0x01, 0xc8]),
                    BinOp::Sub => self.code.extend_from_slice(&[0x48, 0x29, 0xc8]),
                    BinOp::Mul => self.code.extend_from_slice(&[0x48, 0x0f, 0xaf, 0xc1]),
                    BinOp::Div | BinOp::Mod => {
                        self.code.extend_from_slice(&[0x48, 0x99, 0x48, 0xf7, 0xf9]); // cqo; idiv rcx
                        if *op == BinOp::Mod {
                            self.code.extend_from_slice(&[0x48, 0x89, 0xd0]); // mov rax,rdx
                        }
                    }
                    _ => unreachable!(),
                }
                self.scalar(false)?
            }
            Expr::Abs(e) => {
                let value = self.expr(e, env, depth + 1)?;
                load(&mut self.code, 0, value.scalar()?);
                self.code.extend_from_slice(&[
                    0x48, 0x89, 0xc1, // mov rcx,rax
                    0x48, 0xf7, 0xd9, // neg rcx (MIN excluded by verification)
                    0x48, 0x85, 0xc0, // test rax,rax
                    0x48, 0x0f, 0x48, 0xc1, // cmovs rax,rcx
                ]);
                self.scalar(false)?
            }
            Expr::Min(a, b) | Expr::Max(a, b) => {
                let a = self.expr(a, env, depth + 1)?;
                let b = self.expr(b, env, depth + 1)?;
                load(&mut self.code, 0, a.scalar()?);
                load(&mut self.code, 1, b.scalar()?);
                self.code.extend_from_slice(&[0x48, 0x39, 0xc8]); // cmp rax,rcx
                self.code.extend_from_slice(&[
                    0x48, 0x0f, if matches!(e, Expr::Min(..)) { 0x4f } else { 0x4c }, 0xc1,
                ]); // signed cmovg / cmovl rax,rcx
                self.scalar(false)?
            }
            Expr::Binary(op, a, b) => {
                let a = self.expr(a, env, depth + 1)?;
                let b = self.expr(b, env, depth + 1)?;
                self.use_value(&a)?;
                self.use_value(&b)?;
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
        let value = if let Some(dest) = destination {
            self.copy_text(value, dest)?
        } else {
            value
        };
        self.use_value(&value)?;
        Ok(value)
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
        let branch = self.storage.branch();
        let a = self.expr_into(a, env, depth + 1, destination)?;
        let a = self.materialize_input(a)?;
        let mut dest = match (&a, destination) {
            (Value::Text { cap, .. }, Some(dest)) => dest.value(*cap),
            (_, None) => self.join_slots(&a)?,
            _ => return Err(error("text destination requires a text branch")),
        };
        if destination.is_none() {
            self.move_value(&a, &dest)?;
        }
        let done = jump(&mut self.code, &[0xe9]);
        self.finish_jump(otherwise);
        self.storage.otherwise(branch)?;
        let b = self.expr_into(b, env, depth + 1, destination)?;
        let b = self.materialize_input(b)?;
        if destination.is_none() {
            self.move_value(&b, &dest)?;
        }
        self.join_capacities(&mut dest, &b)?;
        self.finish_jump(done);
        self.storage.end_branch(branch)?;
        Ok(dest)
    }
    fn join_slots(&mut self, value: &Value) -> Result<Value, NativeError> {
        Ok(match value {
            Value::Text { cap, .. } => self.text(*cap)?,
            Value::Bool(_) => Value::Bool(self.code.slot()?),
            Value::Number(source) => {
                let slot = self.code.slot()?;
                self.join_literals(*source, slot);
                Value::Number(slot)
            }
            Value::Record(name, fields) => Value::Record(
                name.clone(),
                fields
                    .iter()
                    .map(|(n, v)| Ok((n.clone(), self.join_slots(v)?)))
                    .collect::<Result<_, NativeError>>()?,
            ),
            Value::Input => return Err(error("conditional input was not materialized")),
        })
    }
    fn join_literals(&mut self, source: i32, dest: i32) {
        if let Some((lo, hi)) = self.literal_ranges.get(&source).copied() {
            self.literal_ranges
                .entry(dest)
                .and_modify(|(a, b)| {
                    *a = (*a).min(lo);
                    *b = (*b).max(hi);
                })
                .or_insert((lo, hi));
        }
    }
    fn join_capacities(&mut self, dest: &mut Value, other: &Value) -> Result<(), NativeError> {
        match (dest, other) {
            (Value::Text { cap, .. }, Value::Text { cap: other, .. }) => *cap = (*cap).max(*other),
            (Value::Record(name, fields), Value::Record(other_name, other))
                if name == other_name =>
            {
                let other: HashMap<_, _> = other.iter().map(|(n, v)| (n.as_str(), v)).collect();
                for (name, value) in fields {
                    let other = other
                        .get(name.as_str())
                        .ok_or_else(|| error("missing conditional record field"))?;
                    self.join_capacities(value, other)?;
                }
            }
            (Value::Number(dest), Value::Number(source)) => self.join_literals(*source, *dest),
            (Value::Bool(_), Value::Bool(_)) => {}
            _ => return Err(error("incompatible conditional storage")),
        }
        Ok(())
    }
    fn move_value(&mut self, from: &Value, to: &Value) -> Result<(), NativeError> {
        if let (Value::Record(name, source), Value::Record(other, fields)) = (from, to) {
            if name != other {
                return Err(error("incompatible conditional record concepts"));
            }
            let source: HashMap<_, _> = source.iter().map(|(n, v)| (n.as_str(), v)).collect();
            for (name, dest) in fields {
                let value = source
                    .get(name.as_str())
                    .ok_or_else(|| error("missing conditional record field"))?;
                // Joining a text field moves its pointer and length only. The
                // existing provenance graph keeps both possible owners alive.
                self.move_value(value, dest)?;
            }
        } else if let Value::Text { ptr, len, .. } = from {
            let Value::Text { ptr: dest, .. } = to else {
                return Err(error("text join requires text"));
            };
            self.storage.alias(*dest, *ptr)?;
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
        code: slots::Code::default(),
        rules,
        concept,
        fields: Vec::new(),
        expression_stack_bytes: 0,
        nodes: 0,
        literal_bytes: 0,
        literal_ranges: HashMap::new(),
        storage: storage::Storage::default(),
    };
    let mut result = emit.rule(rule, Value::Input, 0, None)?;
    // CLI/HTTP consumers run after the entire fragment. Retain every buffer
    // reachable from the returned text or record through that boundary.
    emit.use_value(&result)?;
    // Preserve diagnostics by logical identity, before unrelated words can
    // share an address. A dead literal must never become a result's range.
    let http_status_range = if let Value::Record(name, fields) = &result {
        fields
            .iter()
            .find(|(field, _)| name == "HttpResponse" && field == "status")
            .and_then(|(_, v)| v.scalar().ok())
            .and_then(|slot| emit.literal_ranges.get(&slot).copied())
    } else {
        None
    };
    let mut inputs = Vec::new();
    for (_, value) in &emit.fields {
        value.slots(&mut inputs);
    }
    let mut outputs = Vec::new();
    result.slots(&mut outputs);
    let layout = emit.code.layout(&inputs, &outputs)?;
    let slot_bytes = layout.bytes;
    let frame_bytes = emit.storage.layout(&mut emit.code, slot_bytes)?;
    for (_, value) in &mut emit.fields {
        value.relocate(&layout)?;
    }
    result.relocate(&layout)?;
    Ok(Fragment {
        code: emit.code.into_bytes(),
        fields: emit.fields,
        result,
        frame_bytes,
        slot_bytes,
        expression_stack_bytes: emit.expression_stack_bytes,
        http_status_range,
        calls: emit.storage.call_report()?,
    })
}

impl Fragment {
    pub(super) fn response_capacity(&self) -> Result<usize, NativeError> {
        if let Value::Record(name, fields) = &self.result {
            if name == "HttpResponse" {
                if let Some((_, Value::Text { cap, .. })) = fields.iter().find(|(n, _)| n == "body") {
                    return Ok(*cap);
                }
            }
        }
        Err(error("HTTP stack analysis requires a bounded response body"))
    }
    pub(super) fn stack_layout(&self) -> (crate::stack_budget::TextFrame, usize) {
        (crate::stack_budget::TextFrame {
            slot_bytes: self.slot_bytes,
            buffer_bytes: self.frame_bytes - self.slot_bytes,
            saved_register_bytes: 16,
            calls: self.calls.clone(),
        }, self.expression_stack_bytes)
    }
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
    // Copy while invocation storage is live. State keeps its own pointer;
    // restoring the temporary frame must happen on both success and backstop.
    pub(super) fn persist(
        &self,
        code: &mut Vec<u8>,
        offsets: &HashMap<&str, i32>,
        text: &TextBindings<'_>,
        buffer: i32,
        length: i32,
        capacity: i32,
        abort_patches: &mut Vec<usize>,
    ) -> Result<(), NativeError> {
        let Value::Text { ptr, len, cap } = self.result else {
            return Err(error("state transfer requires a text result"));
        };
        if capacity < 0 || cap > capacity as usize {
            return Err(error("text result exceeds persistent destination capacity"));
        }
        self.begin(code, offsets, text)?;
        load(code, 2, len);
        code.extend_from_slice(&[0x48, 0x81, 0xfa]);
        code.extend_from_slice(&capacity.to_le_bytes());
        let excessive = jump(code, &[0x0f, 0x87]);
        // r10 is the enclosing service frame, saved by begin().
        code.extend_from_slice(&[0x4c, 0x8b, 0x55, 0x08]);
        load(code, 6, ptr);
        code.extend_from_slice(&[0x49, 0x8d, 0xba]); // lea rdi, [r10 + buffer]
        code.extend_from_slice(&buffer.to_le_bytes());
        code.extend_from_slice(&[0x48, 0x89, 0xd1, 0xfc, 0xf3, 0xa4]);
        outer_store(code, 2, length);
        Self::end(code);
        let done = jump(code, &[0xe9]);
        let fail = code.len();
        patch(code, excessive, fail);
        Self::end(code);
        abort_patches.push(jump(code, &[0xe9]));
        let end = code.len();
        patch(code, done, end);
        Ok(())
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
        if let Some((lo, hi)) = self.http_status_range {
            if lo < 100 || hi > 599 {
                let n = if lo < 100 { lo } else { hi };
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
        // Restore caller registers but retain rsp below our buffers through
        // service logs and send. Log scratch is allocated below this region;
        // freeing that scratch must not release the borrowed response. The
        // existing accept-loop reset releases the region on all client paths.
        load(code, 3, 0);
        code.extend_from_slice(&[0x4c, 0x89, 0xd5]);
        Ok(())
    }
}

pub(super) fn compile(p: &Program, rule: &Rule, concept: &Concept) -> Result<Vec<u8>, NativeError> {
    Ok(compile_phase(p, rule, concept, EntryEnd::Exit)?.0)
}

pub(super) fn stack_report(
    p: &Program,
    rule: &Rule,
    concept: &Concept,
) -> Result<crate::stack_budget::Report, NativeError> {
    Ok(compile_phase(p, rule, concept, EntryEnd::Exit)?.1)
}

pub(super) fn compile_phase(
    p: &Program,
    rule: &Rule,
    concept: &Concept,
    end: EntryEnd,
) -> Result<(Vec<u8>, crate::stack_budget::Report), NativeError> {
    compile_entry(p, rule, concept, end, false)
}

pub(super) fn compile_pipeline(
    p: &Program,
    rule: &Rule,
    concept: &Concept,
) -> Result<(Vec<u8>, crate::stack_budget::Report), NativeError> {
    compile_entry(p, rule, concept, EntryEnd::Exit, true)
}

fn compile_entry(
    p: &Program,
    rule: &Rule,
    concept: &Concept,
    end: EntryEnd,
    checked_input: bool,
) -> Result<(Vec<u8>, crate::stack_budget::Report), NativeError> {
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
        // This synthetic input rule has no lets. The numeric-scratch entry
        // option therefore only enables strict i64 and complete-record guards;
        // there are no scratch bindings to reserve or skip.
        checked_input,
    )?;
    fragment.begin(&mut code, &ctx.binding_offsets, &ctx.text_bindings)?;
    let mut output_stack_bytes = 0;
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
            output_stack_bytes = ITOA_STACK_BYTES as usize;
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
                        // emit_record_as_json receives only slot identifiers;
                        // each numeric formatter restores its 24-byte scratch.
                        output_stack_bytes = ITOA_STACK_BYTES as usize;
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
    emit_record_loop_end(&mut code, &ctx, end);
    let report = crate::stack_budget::Report {
        rule: rule.name.clone(),
        declared_bytes: rule.proofs.native_stack,
        input_slot_bytes: concept.fields.len() * 8,
        shared_slot_bytes: 0,
        bookkeeping_bytes: ctx.frame_bytes - concept.fields.len() * 8,
        frame_bytes: ctx.frame_bytes,
        saved_base_pointer_bytes: 8,
        input_stack_bytes: if concept
            .fields
            .iter()
            .any(|f| text_field_declared_max(f).is_some())
        {
            8
        } else {
            0
        },
        expression_stack_bytes: fragment.expression_stack_bytes,
        output_stack_bytes,
        text_frame: Some(crate::stack_budget::TextFrame {
            slot_bytes: fragment.slot_bytes,
            buffer_bytes: fragment.frame_bytes - fragment.slot_bytes,
            saved_register_bytes: 16, // Fragment::begin saves rbp and rbx.
            calls: fragment.calls,
        }),
    };
    Ok((code, report))
}

// Reuse the checked fragment and its placed lifetimes for a concurrent lane.
// Only the final consumer changes: serialization into a fixed owned buffer.
pub(super) fn worker_body(
    p: &Program,
    rule: &Rule,
    concept: &Concept,
) -> Result<super::concurrent::Body, NativeError> {
    use super::concurrent::{literal, output_end, output_start, scalar_output};
    let fragment = prepare(p, &rule.name, concept)?;
    let frame_bytes = (concept.fields.len() + 1) * 8;
    let sticky = -(frame_bytes as i32);
    let offsets = concept
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), -((i as i32 + 1) * 8)))
        .collect();
    let mut code = Vec::new();
    fragment.begin(&mut code, &offsets, &HashMap::new())?;
    let mut scratch = 0;
    fn value(code: &mut Vec<u8>, v: &Value, scratch: &mut usize) -> Result<usize, NativeError> {
        match v {
            Value::Number(slot) => {
                load(code, 0, *slot);
                emit_itoa_to_buffer(code);
                *scratch = 24;
                Ok(20)
            }
            Value::Text { ptr, len, cap } => {
                load(code, 6, *ptr);
                load(code, 1, *len);
                code.extend_from_slice(&[0x48, 0x89, 0xdf, 0xfc, 0xf3, 0xa4, 0x48, 0x89, 0xfb]);
                Ok(*cap)
            }
            _ => Err(error(
                "concurrent record output supports number/text fields",
            )),
        }
    }
    let output_bytes = if let Value::Bool(slot) = fragment.result {
        load(&mut code, 0, slot);
        Fragment::end(&mut code);
        scalar_output(&mut code, &Type::Bool, sticky);
        6
    } else {
        output_start(&mut code);
        let capacity = if let Value::Record(name, fields) = &fragment.result {
            let output = iter_all_concepts(&p.items)
                .find(|c| c.name == *name)
                .ok_or_else(|| error("missing output concept"))?;
            let mut capacity: usize = 2; // closing brace and newline
            for (index, field) in output.fields.iter().enumerate() {
                let v = &fields
                    .iter()
                    .find(|(n, _)| *n == field.name)
                    .ok_or_else(|| error("missing output field"))?
                    .1;
                let prefix = format!(
                    "{}\"{}\":{}",
                    if index == 0 { "{" } else { "," },
                    field.name,
                    if field.ty == Type::Text { "\"" } else { "" }
                );
                literal(&mut code, prefix.as_bytes());
                let n = value(&mut code, v, &mut scratch)?;
                capacity = capacity
                    .checked_add(prefix.len())
                    .and_then(|v| v.checked_add(n))
                    .ok_or_else(|| error("output capacity overflow"))?;
                if field.ty == Type::Text {
                    literal(&mut code, b"\"");
                    capacity = capacity
                        .checked_add(1)
                        .ok_or_else(|| error("output capacity overflow"))?;
                }
            }
            literal(&mut code, b"}\n");
            capacity
        } else {
            let capacity = value(&mut code, &fragment.result, &mut scratch)?
                .checked_add(1)
                .ok_or_else(|| error("output capacity overflow"))?;
            literal(&mut code, b"\n");
            capacity
        };
        output_end(&mut code);
        Fragment::end(&mut code);
        capacity
    };
    Ok(super::concurrent::Body {
        code,
        frame_bytes,
        output_bytes,
        stack_bytes: frame_bytes
            + 8usize.max(16 + fragment.frame_bytes + fragment.expression_stack_bytes.max(scratch)),
    })
}
