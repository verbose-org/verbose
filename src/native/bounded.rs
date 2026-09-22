//! Dedicated two-word Result(number, BoundsError) lowering. Values never use
//! pointers: rax is the tag (0/1), rdx the byte or zero. Every temporary and
//! lexical binding has a frame slot; call expansion is acyclic and eager.
//! Also used for strict numeric contracts: scalar slots are one word and reused
//! after their last enclosing expression, including by subsequent scratch and
//! expanded calls. Scratch is not zero-initialized; entry checks complete i64s.
use super::*;

mod liveness;

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
    peak: usize,
    numeric: bool,
    numeric_slots: liveness::Slots,
    temporaries: Vec<usize>,
    expression_stack_bytes: usize,
}
impl Emit<'_> {
    fn numeric_local(&mut self, ty: Type) -> Result<(usize, Local), NativeError> {
        let index = self.numeric_slots.allocate();
        let slot = *self.slots.get(index).ok_or_else(|| NativeError {
            message: "strict overflow frame estimate exhausted".into(),
        })?;
        self.peak = self.peak.max(index + 1);
        store(&mut self.code, slot, false);
        Ok((index, Local { slot, ty }))
    }

    fn save(&mut self, ty: Type) -> Result<Local, NativeError> {
        if self.numeric {
            let (index, local) = self.numeric_local(ty)?;
            self.temporaries.push(index);
            return Ok(local);
        }
        let width = 2;
        let slot = *self
            .slots
            .get(self.next + width - 1)
            .ok_or_else(|| NativeError {
                message: "bounded-result frame estimate exhausted".into(),
            })?;
        self.next += width;
        self.peak = self.peak.max(self.next);
        store(&mut self.code, slot, false);
        if ty == crate::bounds::result_type() {
            store(&mut self.code, slot + 8, true);
        }
        Ok(Local { slot, ty })
    }
    fn rule(&mut self, r: &Rule) -> Result<Type, NativeError> {
        if self.numeric {
            return self.numeric_rule(r);
        }
        let mut env = HashMap::new();
        for (name, expr) in &r.logic.bindings {
            let ty = self.expr(expr, &r.input_name, &env)?;
            let local = self.save(ty)?;
            env.insert(name.clone(), local);
        }
        self.expr(&r.logic.value, &r.input_name, &env)
    }
    fn numeric_rule(&mut self, r: &Rule) -> Result<Type, NativeError> {
        let plan = liveness::Plan::for_rule(r);
        let mut slots = vec![None; r.logic.bindings.len()];
        let mut env = HashMap::new();
        for (i, (name, expr)) in r.logic.bindings.iter().enumerate() {
            // Keep eager source order, including unused nonconstant lets.
            let ty = self.expr(expr, &r.input_name, &env)?;
            for &definition in &plan.releases[i] {
                self.numeric_slots
                    .release(slots[definition].take().unwrap());
            }
            if plan.used[i] {
                // The initializer finished; its last-use operands can now be
                // overwritten by the result without moving another live value.
                let (index, local) = self.numeric_local(ty)?;
                slots[i] = Some(index);
                env.insert(name.clone(), local);
            } else {
                env.remove(name);
            }
        }
        let ty = self.expr(&r.logic.value, &r.input_name, &env)?;
        for &definition in &plan.releases[r.logic.bindings.len()] {
            self.numeric_slots
                .release(slots[definition].take().unwrap());
        }
        debug_assert!(slots.iter().all(Option::is_none));
        Ok(ty)
    }
    fn scalar(
        &mut self,
        expr: &Expr,
        input: &str,
        env: &HashMap<String, Local>,
    ) -> Result<(), NativeError> {
        if self.numeric {
            // Children of scalar operators have already been evaluated into
            // stable slots. Generic binary/min/max emission spills just one
            // word; nested expressions/calls never run underneath that spill.
            // Keep this closed: extending numeric analysis must also account
            // for any newly admitted scalar emitter's transient stack use.
            let bytes = match expr {
                Expr::Number(_) | Expr::Ident(_) | Expr::Field(_, _) => 0,
                Expr::Neg(a) | Expr::Abs(a) | Expr::Not(a)
                    if matches!(a.as_ref(), Expr::Ident(_)) =>
                {
                    0
                }
                Expr::Binary(_, a, b) | Expr::Min(a, b) | Expr::Max(a, b)
                    if matches!((a.as_ref(), b.as_ref()), (Expr::Ident(_), Expr::Ident(_))) =>
                {
                    8
                }
                _ => {
                    return Err(NativeError {
                        message: "unknown native stack analysis for scalar lowering".into(),
                    })
                }
            };
            self.expression_stack_bytes = self.expression_stack_bytes.max(bytes);
        }
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
        let mark = self.temporaries.len();
        let value = self.expr_inner(e, input, env);
        // The result is in registers. All slots created by this expression
        // are dead. Caller operands and lexical locals remain allocated, even
        // when their physical slots are interleaved with this scratch. Expanded
        // callees release their own locals after producing their result.
        if self.numeric {
            for slot in self.temporaries.drain(mark..) {
                self.numeric_slots.release(slot);
            }
        }
        value
    }
    fn expr_inner(
        &mut self,
        e: &Expr,
        input: &str,
        env: &HashMap<String, Local>,
    ) -> Result<Type, NativeError> {
        if self.numeric {
            if let Expr::Binary(BinOp::Eq, a, b) = e {
                if let (Expr::Number(a), Expr::Number(b)) = (a.as_ref(), b.as_ref()) {
                    emit_mov_rax_imm(&mut self.code, i64::from(a == b));
                    return Ok(Type::Bool);
                }
            }
        }
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
            Expr::Field(_, name) => {
                // Input fields and lexical scalars occupy separate namespaces.
                // A let named `x` must not change a later read of `input.x`.
                let slot = *self.fields.get(name.as_str()).ok_or_else(|| NativeError {
                    message: format!("unknown checked input field '{name}'"),
                })?;
                load(&mut self.code, slot, false);
                Ok(Type::Number)
            }
            Expr::Number(_) | Expr::Ident(_) => {
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
            Expr::Binary(op @ (BinOp::And | BinOp::Or), a, b) => {
                self.expr(a, input, env)?;
                self.code.extend_from_slice(&[0x48, 0x85, 0xc0]);
                let done = jump(
                    &mut self.code,
                    if *op == BinOp::And {
                        &[0x0f, 0x84]
                    } else {
                        &[0x0f, 0x85]
                    },
                );
                self.expr(b, input, env)?;
                patch(&mut self.code, done);
                Ok(Type::Bool)
            }
            // Use the established scalar emitter solely on frame loads. Direct
            // numeric field/local reads can borrow their existing slots: inputs
            // are immutable, and lexical locals live through this complete
            // expression, including its expanded calls. Computed subterms still
            // evaluate once in source order and receive independent scratch.
            // Synthetic names isolate operand slots from source namespaces.
            _ => {
                let mut children = Vec::new();
                crate::verifier::walk_expr_children(e, &mut |c| children.push(c.clone()));
                let mut locals = HashMap::new();
                for (i, c) in children.iter().enumerate() {
                    // byte_at's literal is consumed by its existing emitter.
                    if matches!(e, Expr::ByteAt(_, _)) && i == 0 {
                        continue;
                    }
                    let borrowed = if self.numeric {
                        match c {
                            Expr::Ident(name) => env.get(name).cloned(),
                            Expr::Field(_, name) => {
                                self.fields.get(name.as_str()).map(|&slot| Local {
                                    slot,
                                    ty: Type::Number,
                                })
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    let local = if let Some(local) = borrowed {
                        local
                    } else {
                        // Keep literals behind slots too: exposing a constant
                        // divisor would activate legacy unsigned reductions.
                        let ty = self.expr(c, input, env)?;
                        self.save(ty)?
                    };
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
    Ok(compile_with_layout(p, name, EntryEnd::Exit)?.0)
}

pub(super) fn numeric_stack_report(
    p: &Program,
    name: &str,
) -> Result<crate::stack_budget::Report, NativeError> {
    Ok(compile_phase(p, name, EntryEnd::Exit)?.1)
}

pub(super) fn compile_phase(
    p: &Program,
    name: &str,
    end: EntryEnd,
) -> Result<(Vec<u8>, crate::stack_budget::Report), NativeError> {
    if !p
        .items
        .iter()
        .any(|i| matches!(i, Item::Rule(r) if r.name == name))
    {
        return Err(NativeError {
            message: format!("no rule named '{name}' for native stack analysis"),
        });
    }
    if !crate::numeric_bounds::active_rules(p).contains(name) {
        return Err(NativeError {
            message: format!("rule '{name}': native stack analysis requires the strict numeric contract (hints.overflow) or the bounded text contract"),
        });
    }
    let (code, report) = compile_with_layout(p, name, end)?;
    let report = report.ok_or_else(|| NativeError {
        message: format!("rule '{name}': unknown native stack layout"),
    })?;
    Ok((code, report))
}

fn compile_with_layout(
    p: &Program,
    name: &str,
    end: EntryEnd,
) -> Result<(Vec<u8>, Option<crate::stack_budget::Report>), NativeError> {
    if let Some(e) = crate::bounds::verify(p).first() {
        return Err(NativeError {
            message: e.to_string(),
        });
    }
    let numeric = crate::numeric_bounds::active_rules(p).contains(name);
    if let Some(e) = crate::numeric_bounds::verify(p).first() {
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
    // Bound expansion on the original source, including branches later erased.
    // Keep `numeric` from that source: lowering can erase its last checked call.
    let lowered;
    let (rules, r) = if numeric {
        lowered = crate::numeric_bounds::native_opt::lower(p).map_err(|e| NativeError {
            message: e.to_string(),
        })?;
        let rules: HashMap<_, _> = lowered
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Rule(r) => Some((r.name.as_str(), r)),
                _ => None,
            })
            .collect();
        let r = rules[name];
        (rules, r)
    } else {
        (rules, r)
    };
    let estimated = (if numeric { 2 } else { 4 }) * (nodes + r.logic.bindings.len() + 1);
    if numeric && (concept.fields.len() + 32) * 8 > 2 * 1024 * 1024 {
        return Err(NativeError {
            message: "strict overflow frame exceeds 2 MiB".into(),
        });
    }
    // With no context, the prologue puts one word per input at rbp-8 onward,
    // followed by scalar scratch slots. Emit the body first to measure its
    // peak live scratch; relative branches stay valid when the prefix is added.
    let fields = concept
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), -((i as i32 + 1) * 8)))
        .collect();
    let slots: Vec<_> = (0..estimated)
        .map(|i| -(((concept.fields.len() + i) as i32 + 1) * 8))
        .collect();
    let mut emit = Emit {
        code: Vec::new(),
        rules,
        fields,
        slots,
        next: 0,
        peak: 0,
        numeric,
        numeric_slots: liveness::Slots::default(),
        temporaries: Vec::new(),
        expression_stack_bytes: 0,
    };
    emit.rule(r)?;
    let nslots = if numeric { emit.peak } else { estimated };
    if numeric && (nslots + concept.fields.len() + 32) * 8 > 2 * 1024 * 1024 {
        return Err(NativeError {
            message: "strict overflow frame exceeds 2 MiB".into(),
        });
    }
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
        numeric,
    )?;
    debug_assert!(frame
        .logic
        .bindings
        .iter()
        .enumerate()
        .all(|(i, (name, _))| ctx.binding_offsets[name.as_str()] == emit.slots[i]));
    code.append(&mut emit.code);
    emit.code = code;
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
    emit_record_loop_end(&mut emit.code, &ctx, end);
    let report = numeric.then(|| crate::stack_budget::Report {
        text_frame: None,
        rule: name.into(),
        declared_bytes: r.proofs.native_stack,
        input_slot_bytes: concept.fields.len() * 8,
        shared_slot_bytes: nslots * 8,
        bookkeeping_bytes: ctx.frame_bytes - (concept.fields.len() + nslots) * 8,
        frame_bytes: ctx.frame_bytes,
        saved_base_pointer_bytes: 8,
        // emit_text_bound_check saves/restores rdi while scanning each bounded
        // carried text field. Numeric parsing uses registers only. All guards
        // finish before body spills or output formatting begin.
        input_stack_bytes: if concept
            .fields
            .iter()
            .any(|f| text_field_declared_max(f).is_some())
        {
            8
        } else {
            0
        },
        expression_stack_bytes: emit.expression_stack_bytes,
        output_stack_bytes: if r.output_ty == Type::Number {
            ITOA_STACK_BYTES as usize
        } else {
            0
        },
    });
    Ok((emit.code, report))
}
