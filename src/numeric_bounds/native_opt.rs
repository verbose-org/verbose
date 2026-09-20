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
            Value::Number(ranges) => {
                // Native folding deliberately uses only the conservative hull.
                let (lo, hi) = ranges.hull();
                Self::Number(lo, hi)
            }
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
    // Numeric domains share the verifier's sparse branch scopes. Only known
    // boolean constants need a separate map; guards never refine booleans.
    values: HashMap<String, Value>,
    bools: HashMap<String, bool>,
    calls: &'a HashMap<String, Fact>,
}
impl Fold<'_> {
    fn bind(&mut self, name: &str, fact: Fact) {
        // Every let introduces a new definition, including unknown results.
        // Do not leave the previous definition's facts behind on shadowing.
        self.values.remove(name);
        self.bools.remove(name);
        match fact {
            Fact::Number(lo, hi) => {
                self.values
                    .insert(name.into(), Value::Number(Ranges::interval(lo, hi)));
            }
            Fact::Bool(value) => {
                self.values.insert(name.into(), Value::Bool);
                if let Some(value) = value {
                    self.bools.insert(name.into(), value);
                }
            }
            // A loss of precision can make valid arithmetic unknown to this
            // pass. Leave its binding and uses intact; invent no domain for it.
            Fact::Unknown => {}
        }
    }

    fn expr(&self, e: &Expr, scope: &Scope<'_>) -> (Expr, Fact) {
        let (lowered, fact) = match e {
            Expr::Number(n) => (e.clone(), Fact::Number(*n, *n)),
            Expr::Ident(n) => (
                e.clone(),
                self.bools
                    .get(n)
                    .copied()
                    .map(|v| Fact::Bool(Some(v)))
                    .or_else(|| scope.local(n).map(Fact::from))
                    .unwrap_or(Fact::Unknown),
            ),
            Expr::Field(_, n) => (
                e.clone(),
                scope
                    .field(n)
                    .map(|ranges| Fact::from(Value::Number(ranges)))
                    .unwrap_or(Fact::Unknown),
            ),
            Expr::Call(n, _) => (
                e.clone(),
                self.calls.get(n).copied().unwrap_or(Fact::Unknown),
            ),
            Expr::If(c, a, b) => {
                let (condition, cf) = self.expr(c, scope);
                // Both source branches were checked before any transformation.
                // Refine from the ORIGINAL condition: folding its scalars first
                // must not change which lexical definitions the guard describes.
                if let Fact::Bool(Some(v)) = cf {
                    return self.expr(if v { a } else { b }, &scope.branch(c, v));
                }
                let (a, af) = self.expr(a, &scope.branch(c, true));
                let (b, bf) = self.expr(b, &scope.branch(c, false));
                (
                    Expr::If(Box::new(condition), Box::new(a), Box::new(b)),
                    af.join(bf),
                )
            }
            Expr::Binary(op, a, b) => {
                let (a, af) = self.expr(a, scope);
                let (b, bf) = self.expr(b, scope);
                (
                    Expr::Binary(*op, Box::new(a), Box::new(b)),
                    binary(*op, af, bf),
                )
            }
            Expr::Not(a) | Expr::Neg(a) | Expr::Abs(a) => {
                let (a, af) = self.expr(a, scope);
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
                let (a, af) = self.expr(a, scope);
                let (b, bf) = self.expr(b, scope);
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
        let concept = check.concepts[name.as_str()];
        let mut fold = Fold {
            values: HashMap::new(),
            bools: HashMap::new(),
            calls: &calls,
        };
        let mut bindings = Vec::new();
        for (name, expr) in &r.logic.bindings {
            let (expr, fact) = fold.expr(expr, &Scope::new(r, concept, &fold.values));
            fold.bind(name, fact);
            // Pure, proved constants cannot fail; all their uses are substituted.
            if fact.constant().is_none() {
                bindings.push((name.clone(), expr));
            }
        }
        r.logic.value = fold
            .expr(&r.logic.value, &Scope::new(r, concept, &fold.values))
            .0;
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
