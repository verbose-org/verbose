//! A nonempty union of at most two signed intervals. These are compiler facts,
//! never a runtime representation. Operations use at most four interval pairs;
//! excess result pieces widen to their hull instead of enumerating paths.
//! A guard that would need a third piece retains its prior conservative fact.
use super::*;

pub(super) type Interval = (i64, i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ranges {
    first: Interval,
    second: Option<Interval>,
}

impl Ranges {
    pub fn interval(lo: i64, hi: i64) -> Self {
        assert!(lo <= hi, "nonempty numeric interval");
        Self {
            first: (lo, hi),
            second: None,
        }
    }

    pub fn hull(self) -> Interval {
        (self.first.0, self.second.unwrap_or(self.first).1)
    }

    pub fn pieces(self) -> impl Iterator<Item = Interval> {
        std::iter::once(self.first).chain(self.second)
    }

    /// All callers produce at most four pieces. Keep the temporary workspace
    /// fixed too; canonical ordering and widening are independent of hash order.
    fn collect(pieces: impl IntoIterator<Item = Interval>) -> Option<Self> {
        let mut sorted = [(0, 0); 4];
        let mut len = 0;
        for (lo, hi) in pieces {
            if lo <= hi {
                assert!(len < sorted.len(), "at most four temporary intervals");
                sorted[len] = (lo, hi);
                len += 1;
            }
        }
        if len == 0 {
            return None;
        }
        sorted[..len].sort_unstable();
        let mut merged = 0;
        for i in 0..len {
            let (lo, hi) = sorted[i];
            if merged > 0 && lo as i128 <= sorted[merged - 1].1 as i128 + 1 {
                sorted[merged - 1].1 = sorted[merged - 1].1.max(hi);
            } else {
                sorted[merged] = (lo, hi);
                merged += 1;
            }
        }
        Some(match merged {
            1 => Self::interval(sorted[0].0, sorted[0].1),
            2 => Self {
                first: sorted[0],
                second: Some(sorted[1]),
            },
            _ => Self::interval(sorted[0].0, sorted[merged - 1].1),
        })
    }

    pub fn join(self, other: Self) -> Self {
        Self::collect(self.pieces().chain(other.pieces())).expect("nonempty union")
    }

    pub fn map(
        self,
        mut f: impl FnMut(Interval) -> Result<Interval, String>,
    ) -> Result<Self, String> {
        self.pairwise(Self::interval(0, 0), |a, _| f(a))
    }

    pub fn pairwise(
        self,
        other: Self,
        mut f: impl FnMut(Interval, Interval) -> Result<Interval, String>,
    ) -> Result<Self, String> {
        let mut pieces = [(0, 0); 4];
        let mut len = 0;
        for a in self.pieces() {
            for b in other.pieces() {
                // Check EVERY pair before widening. A safe joined result cannot
                // excuse an overflowing intermediate, zero divisor or MIN / -1.
                pieces[len] = f(a, b)?;
                len += 1;
            }
        }
        Ok(Self::collect(pieces[..len].iter().copied()).expect("nonempty result"))
    }

    pub fn arithmetic(self, op: BinOp, other: Self) -> Result<Self, String> {
        self.pairwise(other, |a, b| {
            super::arithmetic(op, a, b)?.number().map(Self::hull)
        })
    }

    pub fn restrict(self, op: BinOp, n: i64) -> Option<Self> {
        let n = n as i128;
        let mut pieces = [(0, 0); 4];
        let mut len = 0;
        let mut push = |lo: i128, hi: i128| {
            if lo <= hi {
                pieces[len] = (lo as i64, hi as i64);
                len += 1;
            }
        };
        for (lo, hi) in self.pieces() {
            let (lo, hi) = (lo as i128, hi as i128);
            match op {
                BinOp::Eq => push(lo.max(n), hi.min(n)),
                BinOp::NotEq => {
                    push(lo, hi.min(n - 1));
                    push(lo.max(n + 1), hi);
                }
                BinOp::Lt => push(lo, hi.min(n - 1)),
                BinOp::LtEq => push(lo, hi.min(n)),
                BinOp::Gt => push(lo.max(n + 1), hi),
                BinOp::GtEq => push(lo.max(n), hi),
                _ => unreachable!("normalized numeric comparison"),
            }
        }
        // Only an interior != exclusion can increase this normalized domain
        // beyond two pieces. Keep the prior fact in that case: the new guard
        // is not evidence of the excluded point, but cannot erase older facts.
        if len > 2 {
            Some(self)
        } else {
            Self::collect(pieces[..len].iter().copied())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains(r: Ranges, x: i64) -> bool {
        r.pieces().any(|(lo, hi)| lo <= x && x <= hi)
    }

    fn domains() -> Vec<Ranges> {
        let mut out = Vec::new();
        for lo in -3..=3 {
            for hi in lo..=3 {
                out.push(Ranges::interval(lo, hi));
                for hole in lo + 1..hi {
                    out.push(
                        Ranges::interval(lo, hi)
                            .restrict(BinOp::NotEq, hole)
                            .unwrap(),
                    );
                }
            }
        }
        out
    }

    #[test]
    fn two_interval_arithmetic_checks_all_concrete_pairs_before_joining() {
        let domains = domains();
        for &a in &domains {
            for &b in &domains {
                for op in [BinOp::Add, BinOp::Sub, BinOp::Mul, BinOp::Div, BinOp::Mod] {
                    let result = a.arithmetic(op, b);
                    if result.is_err() {
                        assert!(matches!(op, BinOp::Div | BinOp::Mod) && contains(b, 0));
                        continue;
                    }
                    for x in (-3..=3).filter(|&x| contains(a, x)) {
                        for y in (-3..=3).filter(|&y| contains(b, y)) {
                            let actual = match op {
                                BinOp::Add => x + y,
                                BinOp::Sub => x - y,
                                BinOp::Mul => x * y,
                                BinOp::Div => x / y,
                                BinOp::Mod => x % y,
                                _ => unreachable!(),
                            };
                            assert!(
                                contains(*result.as_ref().unwrap(), actual),
                                "{a:?} {op:?} {b:?}: {x},{y} -> {actual}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn two_interval_widening_retains_every_value_and_never_hides_failure() {
        let minus = Ranges::interval(-2, -2);
        let plus = Ranges::interval(2, 2);
        let nonzero = minus.join(plus);
        assert!(!contains(nonzero, 0));
        let wide = nonzero.join(Ranges::interval(4, 4));
        assert_eq!(wide, Ranges::interval(-2, 4));
        assert!(Ranges::interval(100, 100)
            .arithmetic(BinOp::Div, wide)
            .is_err());
        for op in [BinOp::Div, BinOp::Mod] {
            assert!(Ranges::interval(i64::MIN, i64::MIN)
                .arithmetic(op, Ranges::interval(-1, -1).join(plus))
                .is_err());
        }
        assert!(Ranges::interval(i64::MAX, i64::MAX)
            .arithmetic(BinOp::Add, nonzero)
            .is_err());
        let edge = Ranges::interval(i64::MIN, i64::MIN).join(Ranges::interval(i64::MAX, i64::MAX));
        assert_eq!(
            edge.restrict(BinOp::NotEq, i64::MIN).unwrap().hull(),
            (i64::MAX, i64::MAX)
        );
        assert!(edge.restrict(BinOp::Eq, 0).is_none());
        assert_eq!(
            edge.join(Ranges::interval(0, 0)),
            Ranges::interval(i64::MIN, i64::MAX)
        );
    }

    #[test]
    fn two_interval_joins_and_guards_cover_all_concrete_members() {
        let domains = domains();
        for &a in &domains {
            for &b in &domains {
                let joined = a.join(b);
                for x in -3..=3 {
                    if contains(a, x) || contains(b, x) {
                        assert!(contains(joined, x));
                    }
                }
                assert_eq!(joined, b.join(a));
            }
            for n in -4..=4 {
                for op in [
                    BinOp::Eq,
                    BinOp::NotEq,
                    BinOp::Lt,
                    BinOp::LtEq,
                    BinOp::Gt,
                    BinOp::GtEq,
                ] {
                    let refined = a.restrict(op, n);
                    for x in (-3..=3).filter(|&x| contains(a, x)) {
                        let matches = match op {
                            BinOp::Eq => x == n,
                            BinOp::NotEq => x != n,
                            BinOp::Lt => x < n,
                            BinOp::LtEq => x <= n,
                            BinOp::Gt => x > n,
                            BinOp::GtEq => x >= n,
                            _ => unreachable!(),
                        };
                        if matches {
                            assert!(
                                refined.is_some_and(|r| contains(r, x)),
                                "{a:?} {op:?} {n}: lost {x}"
                            );
                        }
                    }
                    if let Some(r) = refined {
                        let (lo, hi) = r.hull();
                        assert!(a.hull().0 <= lo && hi <= a.hull().1);
                        let pieces: Vec<_> = r.pieces().collect();
                        if pieces.len() == 2 {
                            assert!(pieces[0].1 as i128 + 1 < pieces[1].0 as i128);
                        }
                    }
                }
            }
        }
    }
}
