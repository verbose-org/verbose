//! Private native lowering after strict verification of the original program.
//! Never publish this view as source: its calls/proofs may have been simplified.
//! The emitter must retain the original entry classification and input guards.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fact {
    Number(Ranges),
    Bool(Option<bool>),
    Unknown,
}
impl From<Value> for Fact {
    fn from(v: Value) -> Self {
        match v {
            Value::Number(ranges) => Self::Number(ranges),
            Value::Bool => Self::Bool(None),
        }
    }
}
impl Fact {
    fn constant(self) -> Option<Expr> {
        match self {
            Self::Number(ranges) => {
                let (lo, hi) = ranges.hull();
                (lo == hi).then_some(Expr::Number(lo))
            }
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
            (Self::Number(a), Self::Number(b)) => Self::Number(a.join(b)),
            (Self::Bool(a), Self::Bool(b)) => Self::Bool(if a == b { a } else { None }),
            _ => Self::Unknown,
        }
    }
}

fn compare_interval(op: BinOp, (a, b): (i64, i64), (c, d): (i64, i64)) -> Option<bool> {
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
        BinOp::Gt => compare_interval(BinOp::Lt, (c, d), (a, b)),
        BinOp::GtEq => compare_interval(BinOp::LtEq, (c, d), (a, b)),
        _ => None,
    }
}

fn compare(op: BinOp, left: Ranges, right: Ranges) -> Option<bool> {
    // A decision must hold for EVERY pair, including pieces of opposite signs.
    // The verifier's fixed domain capacity bounds this to four comparisons.
    let mut result = None;
    for a in left.pieces() {
        for b in right.pieces() {
            let decision = compare_interval(op, a, b)?;
            if result.is_some_and(|previous| previous != decision) {
                return None;
            }
            result = Some(decision);
        }
    }
    result
}

fn binary(op: BinOp, left: Fact, right: Fact) -> Fact {
    match (left, right) {
        (Fact::Number(left), Fact::Number(right)) => {
            let (a, b) = left.hull();
            let (c, d) = right.hull();
            match op {
                // Preserve exact singleton remainder folding: the general
                // remainder range deliberately includes additional values.
                BinOp::Mod if a == b && c == d => a
                    .checked_rem(c)
                    .map(|v| Fact::Number(Ranges::interval(v, v)))
                    .unwrap_or(Fact::Unknown),
                BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                    Fact::Bool(compare(op, left, right))
                }
                _ => left
                    .arithmetic(op, right)
                    .map(Fact::Number)
                    .unwrap_or(Fact::Unknown),
            }
        }
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
            Fact::Number(ranges) => {
                self.values.insert(name.into(), Value::Number(ranges));
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
            Expr::Number(n) => (e.clone(), Fact::Number(Ranges::interval(*n, *n))),
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
                            Fact::Number(ranges) => ranges
                                .map(|(lo, hi)| {
                                    hi.checked_neg()
                                        .zip(lo.checked_neg())
                                        .ok_or_else(|| "negation may overflow".into())
                                })
                                .map(Fact::Number)
                                .unwrap_or(Fact::Unknown),
                            _ => Fact::Unknown,
                        },
                    ),
                    _ => (
                        Expr::Abs(Box::new(a)),
                        match af {
                            Fact::Number(ranges) => ranges
                                .map(|(lo, hi)| {
                                    lo.checked_abs()
                                        .zip(hi.checked_abs())
                                        .map(|(a, b)| {
                                            (
                                                if lo <= 0 && hi >= 0 { 0 } else { a.min(b) },
                                                a.max(b),
                                            )
                                        })
                                        .ok_or_else(|| "absolute value may overflow".into())
                                })
                                .map(Fact::Number)
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
                    (Fact::Number(left), Fact::Number(right)) => left
                        .pairwise(right, |(a, b), (c, d)| {
                            Ok(if is_min {
                                (a.min(c), b.min(d))
                            } else {
                                (a.max(c), b.max(d))
                            })
                        })
                        .map(Fact::Number)
                        .unwrap_or(Fact::Unknown),
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
                            if let Some(proved) =
                                compare(op, Ranges::interval(a, b), Ranges::interval(c, d))
                            {
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

    #[test]
    fn numeric_native_disjoint_comparisons_require_unanimous_pairs() {
        // Every nonempty subset of [-3, 3] representable by <= 2 intervals.
        let mut domains = Vec::new();
        for mask in 1u32..128 {
            let mut pieces = Vec::new();
            let mut bit = 0;
            while bit < 7 {
                if mask & (1 << bit) == 0 {
                    bit += 1;
                    continue;
                }
                let lo = bit;
                while bit < 7 && mask & (1 << bit) != 0 {
                    bit += 1;
                }
                pieces.push(Ranges::interval(lo - 3, bit - 4));
            }
            if pieces.len() <= 2 {
                domains.push(pieces.into_iter().reduce(Ranges::join).unwrap());
            }
        }
        for &left in &domains {
            for &right in &domains {
                for op in [
                    BinOp::Eq,
                    BinOp::NotEq,
                    BinOp::Lt,
                    BinOp::LtEq,
                    BinOp::Gt,
                    BinOp::GtEq,
                ] {
                    let actual: Vec<_> = left
                        .pieces()
                        .flat_map(|(lo, hi)| lo..=hi)
                        .flat_map(|x| {
                            right
                                .pieces()
                                .flat_map(|(lo, hi)| lo..=hi)
                                .map(move |y| match op {
                                    BinOp::Eq => x == y,
                                    BinOp::NotEq => x != y,
                                    BinOp::Lt => x < y,
                                    BinOp::LtEq => x <= y,
                                    BinOp::Gt => x > y,
                                    BinOp::GtEq => x >= y,
                                    _ => unreachable!(),
                                })
                        })
                        .collect();
                    let expected = actual.iter().all(|v| *v == actual[0]).then_some(actual[0]);
                    assert_eq!(
                        compare(op, left, right),
                        expected,
                        "{left:?} {op:?} {right:?}"
                    );
                }
            }
        }
        let extremes =
            Ranges::interval(i64::MIN, i64::MIN).join(Ranges::interval(i64::MAX, i64::MAX));
        assert_eq!(
            compare(
                BinOp::Eq,
                extremes,
                Ranges::interval(i64::MIN + 1, i64::MAX - 1)
            ),
            Some(false)
        );
        assert_eq!(compare(BinOp::Lt, extremes, Ranges::interval(0, 0)), None);
        assert_eq!(
            binary(
                BinOp::Div,
                Fact::Number(extremes),
                Fact::Number(Ranges::interval(-1, -1))
            ),
            Fact::Unknown
        );
        assert_eq!(
            binary(
                BinOp::Mod,
                Fact::Number(extremes),
                Fact::Number(Ranges::interval(-1, -1))
            ),
            Fact::Unknown
        );
    }
}
