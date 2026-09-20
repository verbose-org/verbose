//! Branch-local numeric facts. Direct comparisons of scalars and signed literals
//! refine their domains; no alias relation, boolean binding or callee premise is
//! invented. The complete condition has already passed strict verification.
use super::*;

#[derive(Clone, PartialEq, Eq)]
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
    local_facts: HashMap<String, Ranges>,
    field_facts: HashMap<String, Ranges>,
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
            Value::Number(ranges) => {
                Value::Number(self.local_facts.get(name).copied().unwrap_or(ranges))
            }
            Value::Bool => v,
        })
    }

    pub fn field(&self, name: &str) -> Option<Ranges> {
        let f = self
            .concept
            .fields
            .iter()
            .find(|f| f.name == name && f.ty == Type::Number)?;
        Some(self.field_facts.get(name).copied().unwrap_or_else(|| {
            let (lo, hi) = f.range.unwrap_or((i64::MIN, i64::MAX));
            Ranges::interval(lo, hi)
        }))
    }

    fn scalar(&self, e: &Expr) -> Option<(Place, Ranges)> {
        match e {
            Expr::Ident(n) => Some((Place::Local(n.clone()), self.local(n)?.number().ok()?)),
            Expr::Field(base, n) if matches!(base.as_ref(), Expr::Ident(n) if n == self.input && !self.locals.contains_key(n)) => {
                Some((Place::Field(n.clone()), self.field(n)?))
            }
            _ => None,
        }
    }

    /// Only two direct reads of the same current numeric definition qualify.
    /// Equal domains, aliases, calls and structurally equal computations do not.
    pub fn same_scalar(&self, left: &Expr, right: &Expr) -> bool {
        self.scalar(left)
            .zip(self.scalar(right))
            .is_some_and(|((left, _), (right, _))| left == right)
    }

    fn operand(&self, e: &Expr) -> Option<(Option<Place>, Ranges)> {
        self.scalar(e)
            .map(|(place, range)| (Some(place), range))
            .or_else(|| literal(e).map(|n| (None, Ranges::interval(n, n))))
    }

    fn record(&mut self, place: Option<Place>, range: Ranges) {
        match place {
            Some(Place::Local(name)) => {
                self.local_facts.insert(name, range);
            }
            Some(Place::Field(name)) => {
                self.field_facts.insert(name, range);
            }
            None => {}
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
                let Some(((a_place, a), (b_place, b))) = self.operand(a).zip(self.operand(b))
                else {
                    return true;
                };
                if a_place == b_place {
                    // Literal-only comparisons keep their existing behavior.
                    // A scalar compared with itself adds no facts either:
                    // in particular x < x must not narrow x twice.
                    return true;
                }
                // Snapshot BOTH operands before recording either projection.
                let Some(a_refined) = a.constrain(op, b) else {
                    return false;
                };
                let Some(b_refined) = b.constrain(reverse(op), a) else {
                    return false;
                };
                self.record(a_place, a_refined);
                self.record(b_place, b_refined);
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
                            let range = Ranges::interval(lo, hi)
                                .restrict(comparison(op, truth).unwrap(), n);
                            if let Some(r) = range {
                                for (a, b) in r.pieces() {
                                    assert!(lo <= a && a <= b && b <= hi);
                                }
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
                                        range.is_some_and(|r| r
                                            .pieces()
                                            .any(|(a, b)| a <= x && x <= b)),
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
