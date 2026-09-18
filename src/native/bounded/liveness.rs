//! Scalar binding lifetimes in a verified numeric rule. Slots are released
//! between complete binding expressions, never partway through a branch or call.
use crate::ast::{Expr, Rule};
use std::collections::{BTreeSet, HashMap};

pub(super) struct Plan {
    /// Reusable slot for each binding; unused values need no persistent slot.
    pub slots: Vec<Option<usize>>,
    /// Scratch starts above every live local before each binding and the output.
    pub prefixes: Vec<usize>,
}

impl Plan {
    pub fn for_rule(rule: &Rule) -> Self {
        fn uses(expr: &Expr, scope: &HashMap<&str, usize>, last: &mut [Option<usize>], at: usize) {
            if let Expr::Ident(name) = expr {
                if let Some(&definition) = scope.get(name.as_str()) {
                    last[definition] = Some(at);
                }
            }
            crate::verifier::walk_expr_children(expr, &mut |child| uses(child, scope, last, at));
        }

        let n = rule.logic.bindings.len();
        let mut last = vec![None; n];
        let mut scope = HashMap::new();
        for (i, (name, expr)) in rule.logic.bindings.iter().enumerate() {
            // A shadowing let's initializer still reads the previous definition.
            uses(expr, &scope, &mut last, i);
            scope.insert(name.as_str(), i);
        }
        uses(&rule.logic.value, &scope, &mut last, n);

        let mut releases = vec![Vec::new(); n + 1];
        let mut live = BTreeSet::new();
        let mut free = BTreeSet::new();
        let mut count = 0;
        let mut slots = Vec::with_capacity(n);
        let mut prefixes = Vec::with_capacity(n + 1);
        for i in 0..=n {
            // All locals referenced anywhere in this expression remain intact
            // until its result reaches registers. Holes below this prefix are
            // available for later bindings, not for expression scratch.
            prefixes.push(live.last().map_or(0, |slot| slot + 1));
            for slot in &releases[i] {
                live.remove(slot);
                free.insert(*slot);
            }
            if let Some(Some(end)) = last.get(i) {
                let slot = free.pop_first().unwrap_or_else(|| {
                    let slot = count;
                    count += 1;
                    slot
                });
                live.insert(slot);
                releases[*end].push(slot);
                slots.push(Some(slot));
            } else if i < n {
                slots.push(None);
            }
        }
        Self { slots, prefixes }
    }
}
