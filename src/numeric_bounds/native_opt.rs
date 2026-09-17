//! Private native lowering after strict verification of the original program.
//! Never publish this view as source: its calls/proofs may have been simplified.
//! The emitter must retain the original entry classification and input guards.
use super::*;

#[derive(Clone, Copy, Debug)]
enum Fact {
    Number(i64, i64),
    Bool(Option<bool>),
    Unknown,
}
impl From<Value> for Fact {
    fn from(v: Value) -> Self {
        match v {
            Value::Number(lo, hi) => Self::Number(lo, hi),
            Value::Bool => Self::Bool(None),
        }
    }
}
impl Fact {
    fn constant(self) -> Option<Expr> {
        match self {
            Self::Number(lo, hi) if lo == hi => Some(Expr::Number(lo)),
            // The AST has no bool literal. Keep its type explicit, without
            // inventing an identifier that a user binding could shadow.
            Self::Bool(Some(v)) => Some(Expr::Binary(
                BinOp::Eq,
                Box::new(Expr::Number(0)),
                Box::new(Expr::Number(if v { 0 } else { 1 })),
            )),
            _ => None,
        }
    }
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Number(a, b), Self::Number(c, d)) => Self::Number(a.min(c), b.max(d)),
            (Self::Bool(a), Self::Bool(b)) => Self::Bool(if a == b { a } else { None }),
            _ => Self::Unknown,
        }
    }
}

fn compare(op: BinOp, (a, b): (i64, i64), (c, d): (i64, i64)) -> Option<bool> {
    match op {
        BinOp::Eq | BinOp::NotEq => {
            let eq = if b < c || d < a {
                Some(false)
            } else if a == b && c == d {
                Some(a == c)
            } else {
                None
            };
            eq.map(|v| if op == BinOp::Eq { v } else { !v })
        }
        BinOp::Lt => {
            if b < c {
                Some(true)
            } else if a >= d {
                Some(false)
            } else {
                None
            }
        }
        BinOp::LtEq => {
            if b <= c {
                Some(true)
            } else if a > d {
                Some(false)
            } else {
                None
            }
        }
        BinOp::Gt => compare(BinOp::Lt, (c, d), (a, b)),
        BinOp::GtEq => compare(BinOp::LtEq, (c, d), (a, b)),
        _ => None,
    }
}

fn binary(op: BinOp, left: Fact, right: Fact) -> Fact {
    match (left, right) {
        (Fact::Number(a, b), Fact::Number(c, d)) => match op {
            BinOp::Mod if a == b && c == d => a
                .checked_rem(c)
                .map(|v| Fact::Number(v, v))
                .unwrap_or(Fact::Unknown),
            BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                Fact::Bool(compare(op, (a, b), (c, d)))
            }
            _ => arithmetic(op, (a, b), (c, d))
                .map(Fact::from)
                .unwrap_or(Fact::Unknown),
        },
        (Fact::Bool(a), Fact::Bool(b)) => Fact::Bool(match op {
            BinOp::Eq => a.zip(b).map(|(a, b)| a == b),
            BinOp::NotEq => a.zip(b).map(|(a, b)| a != b),
            BinOp::And => {
                if a == Some(false) || b == Some(false) {
                    Some(false)
                } else {
                    a.zip(b).map(|(a, b)| a && b)
                }
            }
            BinOp::Or => {
                if a == Some(true) || b == Some(true) {
                    Some(true)
                } else {
                    a.zip(b).map(|(a, b)| a || b)
                }
            }
            _ => None,
        }),
        _ => Fact::Unknown,
    }
}

struct Fold<'a> {
    fields: HashMap<&'a str, Fact>,
    locals: HashMap<String, Fact>,
    calls: &'a HashMap<String, Fact>,
}
impl Fold<'_> {
    fn expr(&self, e: &Expr) -> (Expr, Fact) {
        let (lowered, fact) = match e {
            Expr::Number(n) => (e.clone(), Fact::Number(*n, *n)),
            Expr::Ident(n) => (
                e.clone(),
                self.locals.get(n).copied().unwrap_or(Fact::Unknown),
            ),
            Expr::Field(_, n) => (
                e.clone(),
                self.fields
                    .get(n.as_str())
                    .copied()
                    .unwrap_or(Fact::Unknown),
            ),
            Expr::Call(n, _) => (
                e.clone(),
                self.calls.get(n).copied().unwrap_or(Fact::Unknown),
            ),
            Expr::If(c, a, b) => {
                let (c, cf) = self.expr(c);
                // Both source branches were checked before any transformation.
                if let Fact::Bool(Some(v)) = cf {
                    return self.expr(if v { a } else { b });
                }
                let (a, af) = self.expr(a);
                let (b, bf) = self.expr(b);
                (Expr::If(Box::new(c), Box::new(a), Box::new(b)), af.join(bf))
            }
            Expr::Binary(op, a, b) => {
                let (a, af) = self.expr(a);
                let (b, bf) = self.expr(b);
                (
                    Expr::Binary(*op, Box::new(a), Box::new(b)),
                    binary(*op, af, bf),
                )
            }
            Expr::Not(a) | Expr::Neg(a) | Expr::Abs(a) => {
                let (a, af) = self.expr(a);
                match e {
                    Expr::Not(_) => (
                        Expr::Not(Box::new(a)),
                        match af {
                            Fact::Bool(v) => Fact::Bool(v.map(|v| !v)),
                            _ => Fact::Unknown,
                        },
                    ),
                    Expr::Neg(_) => (
                        Expr::Neg(Box::new(a)),
                        match af {
                            Fact::Number(lo, hi) => hi
                                .checked_neg()
                                .zip(lo.checked_neg())
                                .map(|(a, b)| Fact::Number(a, b))
                                .unwrap_or(Fact::Unknown),
                            _ => Fact::Unknown,
                        },
                    ),
                    _ => (
                        Expr::Abs(Box::new(a)),
                        match af {
                            Fact::Number(lo, hi) => lo
                                .checked_abs()
                                .zip(hi.checked_abs())
                                .map(|(a, b)| {
                                    Fact::Number(
                                        if lo <= 0 && hi >= 0 { 0 } else { a.min(b) },
                                        a.max(b),
                                    )
                                })
                                .unwrap_or(Fact::Unknown),
                            _ => Fact::Unknown,
                        },
                    ),
                }
            }
            Expr::Min(a, b) | Expr::Max(a, b) => {
                let (a, af) = self.expr(a);
                let (b, bf) = self.expr(b);
                let is_min = matches!(e, Expr::Min(..));
                let fact = match (af, bf) {
                    (Fact::Number(a, b), Fact::Number(c, d)) => {
                        if is_min {
                            Fact::Number(a.min(c), b.min(d))
                        } else {
                            Fact::Number(a.max(c), b.max(d))
                        }
                    }
                    _ => Fact::Unknown,
                };
                (
                    if is_min {
                        Expr::Min(Box::new(a), Box::new(b))
                    } else {
                        Expr::Max(Box::new(a), Box::new(b))
                    },
                    fact,
                )
            }
            _ => (e.clone(), Fact::Unknown),
        };
        (fact.constant().unwrap_or(lowered), fact)
    }
}

/// Verify first, then produce a private emission view. Public source and the
/// general optimizer retain every obligation, including calls in dead branches.
pub(crate) fn lower(p: &Program) -> Result<Program, VerifyError> {
    if let Some(error) = verify(p).into_iter().next() {
        return Err(error);
    }
    let active = active_rules(p);
    let mut check = Check::new(p);
    let mut calls = HashMap::new();
    for name in &active {
        let fact = check.rule(name).map_err(|message| VerifyError {
            context: format!("rule '{name}' / numeric lowering"),
            message,
        })?;
        calls.insert(name.clone(), Fact::from(fact));
    }
    let mut lowered = p.clone();
    for item in &mut lowered.items {
        let Item::Rule(r) = item else {
            continue;
        };
        if !active.contains(&r.name) {
            continue;
        }
        let Type::Named(name) = &r.input_ty else {
            unreachable!("verified flat input");
        };
        let mut fold = Fold {
            fields: check.concepts[name.as_str()]
                .fields
                .iter()
                .filter_map(|f| {
                    if f.ty == Type::Number {
                        let (lo, hi) = f.range.unwrap_or((i64::MIN, i64::MAX));
                        Some((f.name.as_str(), Fact::Number(lo, hi)))
                    } else {
                        None
                    }
                })
                .collect(),
            locals: HashMap::new(),
            calls: &calls,
        };
        let mut bindings = Vec::new();
        for (name, expr) in &r.logic.bindings {
            let (expr, fact) = fold.expr(expr);
            fold.locals.insert(name.clone(), fact);
            // Pure, proved constants cannot fail; all their uses are substituted.
            if fact.constant().is_none() {
                bindings.push((name.clone(), expr));
            }
        }
        r.logic.value = fold.expr(&r.logic.value).0;
        r.logic.bindings = bindings;
    }
    Ok(lowered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_native_decided_comparisons_hold_for_every_concrete_pair() {
        for a in -4..=4 {
            for b in a..=4 {
                for c in -4..=4 {
                    for d in c..=4 {
                        for op in [
                            BinOp::Eq,
                            BinOp::NotEq,
                            BinOp::Lt,
                            BinOp::LtEq,
                            BinOp::Gt,
                            BinOp::GtEq,
                        ] {
                            if let Some(proved) = compare(op, (a, b), (c, d)) {
                                for x in a..=b {
                                    for y in c..=d {
                                        let actual = match op {
                                            BinOp::Eq => x == y,
                                            BinOp::NotEq => x != y,
                                            BinOp::Lt => x < y,
                                            BinOp::LtEq => x <= y,
                                            BinOp::Gt => x > y,
                                            BinOp::GtEq => x >= y,
                                            _ => unreachable!(),
                                        };
                                        assert_eq!(
                                            proved, actual,
                                            "{op:?}: {a}..{b}, {c}..{d}, {x}, {y}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        for a in [None, Some(false), Some(true)] {
            for b in [None, Some(false), Some(true)] {
                for op in [BinOp::Eq, BinOp::NotEq, BinOp::And, BinOp::Or] {
                    if let Fact::Bool(Some(proved)) = binary(op, Fact::Bool(a), Fact::Bool(b)) {
                        for x in [false, true]
                            .into_iter()
                            .filter(|x| a.is_none_or(|a| a == *x))
                        {
                            for y in [false, true]
                                .into_iter()
                                .filter(|y| b.is_none_or(|b| b == *y))
                            {
                                assert_eq!(
                                    proved,
                                    match op {
                                        BinOp::Eq => x == y,
                                        BinOp::NotEq => x != y,
                                        BinOp::And => x && y,
                                        BinOp::Or => x || y,
                                        _ => unreachable!(),
                                    }
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
