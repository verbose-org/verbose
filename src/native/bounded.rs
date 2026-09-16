//! Dedicated two-word Result(number, BoundsError) lowering. Values never use
//! pointers: rax is the tag (0/1), rdx the byte or zero. Every temporary and
//! lexical binding has a frame slot; call expansion is acyclic and eager.
use super::*;

fn jump(code: &mut Vec<u8>, opcode: &[u8]) -> usize {
    code.extend_from_slice(opcode);
    let p = code.len();
    code.extend_from_slice(&[0; 4]);
    p
}
fn patch(code: &mut Vec<u8>, p: usize) {
    let d = code.len() as i32 - p as i32 - 4;
    code[p..p + 4].copy_from_slice(&d.to_le_bytes());
}
fn store(code: &mut Vec<u8>, slot: i32, dx: bool) {
    code.extend_from_slice(&[0x48, 0x89, if dx { 0x95 } else { 0x85 }]);
    code.extend_from_slice(&slot.to_le_bytes());
}
fn load(code: &mut Vec<u8>, slot: i32, dx: bool) {
    code.extend_from_slice(&[0x48, 0x8b, if dx { 0x95 } else { 0x85 }]);
    code.extend_from_slice(&slot.to_le_bytes());
}
#[derive(Clone)]
struct Local {
    slot: i32,
    ty: Type,
}
struct Emit<'a> {
    code: Vec<u8>,
    rules: HashMap<&'a str, &'a Rule>,
    fields: HashMap<&'a str, i32>,
    slots: Vec<i32>,
    next: usize,
}
impl Emit<'_> {
    fn save(&mut self, ty: Type) -> Result<Local, NativeError> {
        let slot = *self.slots.get(self.next + 1).ok_or_else(|| NativeError {
            message: "bounded-result frame estimate exhausted".into(),
        })?;
        self.next += 2;
        store(&mut self.code, slot, false);
        if ty == crate::bounds::result_type() {
            store(&mut self.code, slot + 8, true);
        }
        Ok(Local { slot, ty })
    }
    fn rule(&mut self, r: &Rule) -> Result<Type, NativeError> {
        let mut env = HashMap::new();
        for (name, expr) in &r.logic.bindings {
            let ty = self.expr(expr, &r.input_name, &env)?;
            let local = self.save(ty)?;
            env.insert(name.clone(), local);
        }
        self.expr(&r.logic.value, &r.input_name, &env)
    }
    fn scalar(
        &mut self,
        expr: &Expr,
        input: &str,
        env: &HashMap<String, Local>,
    ) -> Result<(), NativeError> {
        let mut offsets = self.fields.clone();
        for (name, l) in env {
            offsets.insert(name.as_str(), l.slot);
        }
        emit_eval_expr(
            &mut self.code,
            expr,
            input,
            &offsets,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            None,
            None,
        )
    }
    fn expr(
        &mut self,
        e: &Expr,
        input: &str,
        env: &HashMap<String, Local>,
    ) -> Result<Type, NativeError> {
        let result = crate::bounds::result_type();
        match e {
            Expr::Ident(n) if env.contains_key(n) => {
                let l = &env[n];
                load(&mut self.code, l.slot, false);
                if l.ty == result {
                    load(&mut self.code, l.slot + 8, true);
                }
                Ok(l.ty.clone())
            }
            Expr::Number(_) | Expr::Field(_, _) | Expr::Ident(_) => {
                self.scalar(e, input, env)?;
                Ok(
                    if matches!(e, Expr::Ident(n) if n == "true" || n == "false") {
                        Type::Bool
                    } else {
                        Type::Number
                    },
                )
            }
            Expr::Call(n, _) => {
                let r = self.rules[n.as_str()];
                self.rule(r)
            }
            Expr::TryByteAt(t, i) => {
                self.expr(i, input, env)?;
                let bytes = match t.as_ref() {
                    Expr::Text(t) => t.as_bytes(),
                    Expr::Bytes(b) => b,
                    _ => {
                        return Err(NativeError {
                            message: "try_byte_at requires literal table".into(),
                        })
                    }
                };
                // Unsigned comparison rejects negative indices as well as >= len.
                self.code.extend_from_slice(&[0x48, 0xb9]);
                self.code
                    .extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                self.code.extend_from_slice(&[0x48, 0x39, 0xc8]);
                let err = jump(&mut self.code, &[0x0f, 0x83]);
                self.code.extend_from_slice(&[0x48, 0x8d, 0x15]);
                let data_ref = self.code.len();
                self.code.extend_from_slice(&[0; 4]);
                self.code
                    .extend_from_slice(&[0x0f, 0xb6, 0x14, 0x02, 0x31, 0xc0]); // movzx edx,[rdx+rax]; xor eax,eax
                let done = jump(&mut self.code, &[0xe9]);
                patch(&mut self.code, err);
                emit_mov_rax_imm(&mut self.code, 1);
                self.code.extend_from_slice(&[0x31, 0xd2]);
                if bytes.len() <= 127 {
                    self.code.extend_from_slice(&[0xeb, bytes.len() as u8]);
                } else {
                    self.code.push(0xe9);
                    self.code
                        .extend_from_slice(&(bytes.len() as i32).to_le_bytes());
                }
                patch(&mut self.code, data_ref);
                self.code.extend_from_slice(bytes);
                patch(&mut self.code, done);
                Ok(result)
            }
            Expr::Ok(v) => {
                self.expr(v, input, env)?;
                self.code.extend_from_slice(&[0x48, 0x89, 0xc2, 0x31, 0xc0]);
                Ok(result)
            }
            Expr::Err(v) => {
                self.expr(v, input, env)?;
                emit_mov_rax_imm(&mut self.code, 1);
                self.code.extend_from_slice(&[0x31, 0xd2]);
                Ok(result)
            }
            Expr::If(c, a, b) => {
                self.expr(c, input, env)?;
                self.code.extend_from_slice(&[0x48, 0x85, 0xc0]);
                let other = jump(&mut self.code, &[0x0f, 0x84]);
                let ty = self.expr(a, input, env)?;
                let done = jump(&mut self.code, &[0xe9]);
                patch(&mut self.code, other);
                self.expr(b, input, env)?;
                patch(&mut self.code, done);
                Ok(ty)
            }
            Expr::MatchResult(t, ok, a, err, b) => {
                self.expr(t, input, env)?;
                let l = self.save(result)?;
                self.code.extend_from_slice(&[0x48, 0x85, 0xc0]);
                let other = jump(&mut self.code, &[0x0f, 0x85]);
                let mut scope = env.clone();
                scope.insert(
                    ok.clone(),
                    Local {
                        slot: l.slot + 8,
                        ty: Type::Number,
                    },
                );
                let ty = self.expr(a, input, &scope)?;
                let done = jump(&mut self.code, &[0xe9]);
                patch(&mut self.code, other);
                scope = env.clone();
                scope.insert(
                    err.clone(),
                    Local {
                        slot: l.slot + 8,
                        ty: Type::BoundsError,
                    },
                );
                self.expr(b, input, &scope)?;
                patch(&mut self.code, done);
                Ok(ty)
            }
            Expr::Binary(BinOp::And, a, b) => self.expr(
                &Expr::If(a.clone(), b.clone(), Box::new(Expr::Ident("false".into()))),
                input,
                env,
            ),
            Expr::Binary(BinOp::Or, a, b) => self.expr(
                &Expr::If(a.clone(), Box::new(Expr::Ident("true".into())), b.clone()),
                input,
                env,
            ),
            // Evaluate subterms once, then use the established scalar emitter
            // solely on frame loads. Synthetic identifiers cannot collide with
            // source names because this scope contains only the generated ones.
            _ => {
                let mut children = Vec::new();
                crate::verifier::walk_expr_children(e, &mut |c| children.push(c.clone()));
                let mut locals = HashMap::new();
                for (i, c) in children.iter().enumerate() {
                    // byte_at's literal is consumed by its existing emitter.
                    if matches!(e, Expr::ByteAt(_, _)) && i == 0 {
                        continue;
                    }
                    let ty = self.expr(c, input, env)?;
                    let local = self.save(ty)?;
                    locals.insert(format!("__bounds_{i}"), local);
                }
                let a = Box::new(Expr::Ident("__bounds_0".into()));
                let b = Box::new(Expr::Ident("__bounds_1".into()));
                let lowered = match e {
                    Expr::Binary(op, _, _) => Expr::Binary(*op, a, b),
                    Expr::Neg(_) => Expr::Neg(a),
                    Expr::Not(_) => Expr::Not(a),
                    Expr::Abs(_) => Expr::Abs(a),
                    Expr::BitNot(_) => Expr::BitNot(a),
                    Expr::AbortIf(_) => Expr::AbortIf(a),
                    Expr::Min(_, _) => Expr::Min(a, b),
                    Expr::Max(_, _) => Expr::Max(a, b),
                    Expr::BitAnd(_, _) => Expr::BitAnd(a, b),
                    Expr::BitOr(_, _) => Expr::BitOr(a, b),
                    Expr::BitXor(_, _) => Expr::BitXor(a, b),
                    Expr::Shl(_, _) => Expr::Shl(a, b),
                    Expr::Shr(_, _) => Expr::Shr(a, b),
                    Expr::ByteAt(t, _) => Expr::ByteAt(t.clone(), b),
                    _ => {
                        return Err(NativeError {
                            message: format!("unsupported bounded-result lowering: {:?}", e),
                        })
                    }
                };
                self.scalar(&lowered, input, &locals)?;
                Ok(
                    if matches!(
                        e,
                        Expr::Not(_)
                            | Expr::Binary(
                                BinOp::Eq
                                    | BinOp::NotEq
                                    | BinOp::Gt
                                    | BinOp::Lt
                                    | BinOp::GtEq
                                    | BinOp::LtEq,
                                _,
                                _
                            )
                    ) {
                        Type::Bool
                    } else {
                        Type::Number
                    },
                )
            }
        }
    }
}

pub(super) fn compile(p: &Program, name: &str) -> Result<Vec<u8>, NativeError> {
    if let Some(e) = crate::bounds::verify(p).first() {
        return Err(NativeError {
            message: e.to_string(),
        });
    }
    let rules: HashMap<_, _> = p
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some((r.name.as_str(), r)),
            _ => None,
        })
        .collect();
    let r = rules[name];
    let concept = p
        .items
        .iter()
        .find_map(|i| match i {
            Item::Concept(c) if r.input_ty == Type::Named(c.name.clone()) => Some(c),
            _ => None,
        })
        .ok_or_else(|| NativeError {
            message: "bounded result input concept missing".into(),
        })?;
    fn count(
        e: &Expr,
        rules: &HashMap<&str, &Rule>,
        budget: &mut usize,
    ) -> Result<(), NativeError> {
        if *budget >= 100_000 {
            return Err(NativeError {
                message: "bounded-result call expansion exceeds 100000 nodes".into(),
            });
        }
        *budget += 1;
        if let Expr::Call(n, _) = e {
            let r = rules[n.as_str()];
            for (_, b) in &r.logic.bindings {
                count(b, rules, budget)?;
            }
            count(&r.logic.value, rules, budget)?;
        }
        let mut err = None;
        crate::verifier::walk_expr_children(e, &mut |c| {
            if err.is_none() {
                err = count(c, rules, budget).err();
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        Ok(())
    }
    let mut nodes = 0;
    for (_, b) in &r.logic.bindings {
        count(b, &rules, &mut nodes)?;
    }
    count(&r.logic.value, &rules, &mut nodes)?;
    let nslots = 4 * (nodes + r.logic.bindings.len() + 1);
    let mut frame = r.clone();
    frame.output_ty = Type::Number;
    frame.logic.value = Expr::Number(0);
    frame.logic.bindings = (0..nslots)
        .map(|i| (format!("__bounds_slot_{i}"), Expr::Number(0)))
        .collect();
    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(
        &mut code,
        &frame,
        concept,
        None,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        None,
        false,
    )?;
    let slots = frame
        .logic
        .bindings
        .iter()
        .map(|(n, _)| ctx.binding_offsets[n.as_str()])
        .collect();
    let fields = concept
        .fields
        .iter()
        .map(|f| (f.name.as_str(), ctx.binding_offsets[f.name.as_str()]))
        .collect();
    let mut emit = Emit {
        code,
        rules,
        fields,
        slots,
        next: 0,
    };
    emit.rule(r)?;
    if r.output_ty == crate::bounds::result_type() {
        emit.code.extend_from_slice(&[0x48, 0x85, 0xc0]);
        let err = jump(&mut emit.code, &[0x0f, 0x85]);
        emit.code.extend_from_slice(&[0x48, 0x89, 0xd0]);
        emit_itoa_inline(&mut emit.code);
        let done = jump(&mut emit.code, &[0xe9]);
        patch(&mut emit.code, err);
        emit_write_static_to_fd(&mut emit.code, b"Bounds\n", 2);
        emit_mov_rax_imm(&mut emit.code, 1);
        store(&mut emit.code, ctx.exit_flag_slot, false);
        patch(&mut emit.code, done);
    } else if r.output_ty == Type::Bool {
        emit.code.extend_from_slice(&[0x84, 0xc0]);
        emit_bool_arms(&mut emit.code, BoolExitFlag::Slot(ctx.exit_flag_slot));
    } else {
        emit_itoa_inline(&mut emit.code);
    }
    let back = jump(&mut emit.code, &[0xe9]);
    let d = ctx.loop_top as i32 - back as i32 - 4;
    emit.code[back..back + 4].copy_from_slice(&d.to_le_bytes());
    emit_record_loop_epilogue(&mut emit.code, &ctx);
    Ok(emit.code)
}
