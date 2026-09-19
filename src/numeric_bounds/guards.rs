//! Branch-local numeric facts. Only direct comparisons with signed literals
//! refine a scalar; no alias relation, boolean binding or callee premise is
//! invented. The complete condition has already passed strict verification.
use super::*;

#[derive(Clone)]
enum Place {
    Local(String),
    Field(String),
}

#[derive(Clone)]
pub(super) struct Scope<'a> {
    pub locals: &'a HashMap<String, Value>,
    pub concept: &'a Concept,
    input: &'a str,
    // Sparse: cloning a nested branch copies only its established facts, not
    // every enclosing let. Names denote fixed definitions within an expression.
    local_facts: HashMap<String, (i64, i64)>,
    field_facts: HashMap<String, (i64, i64)>,
}

impl<'a> Scope<'a> {
    pub fn new(r: &'a Rule, concept: &'a Concept, locals: &'a HashMap<String, Value>) -> Self {
        Self {
            locals,
            concept,
            input: &r.input_name,
            local_facts: HashMap::new(),
            field_facts: HashMap::new(),
        }
    }

    pub fn local(&self, name: &str) -> Option<Value> {
        let v = *self.locals.get(name)?;
        Some(match v {
            Value::Number(lo, hi) => {
                let (lo, hi) = self.local_facts.get(name).copied().unwrap_or((lo, hi));
                Value::Number(lo, hi)
            }
            Value::Bool => v,
        })
    }

    pub fn field(&self, name: &str) -> Option<(i64, i64)> {
        let f = self
            .concept
            .fields
            .iter()
            .find(|f| f.name == name && f.ty == Type::Number)?;
        Some(
            self.field_facts
                .get(name)
                .copied()
                .unwrap_or(f.range.unwrap_or((i64::MIN, i64::MAX))),
        )
    }

    fn scalar(&self, e: &Expr) -> Option<(Place, (i64, i64))> {
        match e {
            Expr::Ident(n) => Some((Place::Local(n.clone()), self.local(n)?.number().ok()?)),
            Expr::Field(base, n) if matches!(base.as_ref(), Expr::Ident(n) if n == self.input && !self.locals.contains_key(n)) => {
                Some((Place::Field(n.clone()), self.field(n)?))
            }
            _ => None,
        }
    }

    pub fn branch(&self, condition: &Expr, truth: bool) -> Self {
        let mut branch = self.clone();
        if !branch.assume(condition, truth) {
            // Keep the original obligation in an impossible arm. This slice
            // neither skips its types/effects nor uses an empty interval as a
            // proof of arithmetic safety. Discard ALL newly collected facts.
            return self.clone();
        }
        branch
    }

    fn assume(&mut self, e: &Expr, truth: bool) -> bool {
        match e {
            Expr::Not(e) => self.assume(e, !truth),
            Expr::Binary(BinOp::And, a, b) if truth => self.assume(a, true) && self.assume(b, true),
            Expr::Binary(BinOp::Or, a, b) if !truth => {
                self.assume(a, false) && self.assume(b, false)
            }
            Expr::Binary(op, a, b) => {
                let Some(op) = comparison(*op, truth) else {
                    return true;
                };
                let candidate = self
                    .scalar(a)
                    .zip(literal(b))
                    .map(|(s, n)| (s, op, n))
                    .or_else(|| {
                        self.scalar(b)
                            .zip(literal(a))
                            .map(|(s, n)| (s, reverse(op), n))
                    });
                if let Some(((place, range), op, n)) = candidate {
                    let Some(range) = restrict(range, op, n) else {
                        return false;
                    };
                    match place {
                        Place::Local(name) => self.local_facts.insert(name, range),
                        Place::Field(name) => self.field_facts.insert(name, range),
                    };
                }
                true
            }
            _ => true,
        }
    }
}

fn literal(e: &Expr) -> Option<i64> {
    match e {
        Expr::Number(n) => Some(*n),
        Expr::Neg(n) => match n.as_ref() {
            Expr::Number(n) => n.checked_neg(),
            _ => None,
        },
        _ => None,
    }
}

fn comparison(op: BinOp, truth: bool) -> Option<BinOp> {
    let opposite = match op {
        BinOp::Eq => BinOp::NotEq,
        BinOp::NotEq => BinOp::Eq,
        BinOp::Lt => BinOp::GtEq,
        BinOp::LtEq => BinOp::Gt,
        BinOp::Gt => BinOp::LtEq,
        BinOp::GtEq => BinOp::Lt,
        _ => return None,
    };
    Some(if truth { op } else { opposite })
}

fn reverse(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::LtEq => BinOp::GtEq,
        BinOp::Gt => BinOp::Lt,
        BinOp::GtEq => BinOp::LtEq,
        _ => op,
    }
}

fn restrict((lo, hi): (i64, i64), op: BinOp, n: i64) -> Option<(i64, i64)> {
    // i128 makes strict comparisons at MIN/MAX ordinary interval operations.
    let (lo, hi, n) = (lo as i128, hi as i128, n as i128);
    let (lo, hi) = match op {
        BinOp::Eq => (lo.max(n), hi.min(n)),
        BinOp::NotEq if lo == n => (lo + 1, hi),
        BinOp::NotEq if hi == n => (lo, hi - 1),
        // A single interval cannot represent a hole. Keep the original range.
        BinOp::NotEq => (lo, hi),
        BinOp::Lt => (lo, hi.min(n - 1)),
        BinOp::LtEq => (lo, hi.min(n)),
        BinOp::Gt => (lo.max(n + 1), hi),
        BinOp::GtEq => (lo.max(n), hi),
        _ => unreachable!("normalized comparison"),
    };
    (lo <= hi).then_some((lo as i64, hi as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_intervals_cover_every_concrete_comparison_outcome() {
        let values = [
            i64::MIN,
            i64::MIN + 1,
            -4,
            -3,
            -2,
            -1,
            0,
            1,
            2,
            3,
            4,
            i64::MAX - 1,
            i64::MAX,
        ];
        for &lo in &values {
            for &hi in values.iter().filter(|&&n| n >= lo) {
                for &n in &values {
                    for op in [
                        BinOp::Eq,
                        BinOp::NotEq,
                        BinOp::Lt,
                        BinOp::LtEq,
                        BinOp::Gt,
                        BinOp::GtEq,
                    ] {
                        for truth in [false, true] {
                            let range = restrict((lo, hi), comparison(op, truth).unwrap(), n);
                            if let Some((a, b)) = range {
                                assert!(lo <= a && a <= b && b <= hi);
                            }
                            for &x in values.iter().filter(|&&x| lo <= x && x <= hi) {
                                let actual = match op {
                                    BinOp::Eq => x == n,
                                    BinOp::NotEq => x != n,
                                    BinOp::Lt => x < n,
                                    BinOp::LtEq => x <= n,
                                    BinOp::Gt => x > n,
                                    BinOp::GtEq => x >= n,
                                    _ => unreachable!(),
                                };
                                if actual == truth {
                                    assert!(
                                        range.is_some_and(|(a, b)| a <= x && x <= b),
                                        "{lo}..{hi}, {x} {op:?} {n} == {truth}: {range:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
