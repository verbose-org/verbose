//! Scalar binding lifetimes and compile-time slot placement. Locals are released
//! between complete binding expressions. Scratch can reuse any already dead slot.
use crate::ast::{Expr, Rule};
use std::collections::{BTreeSet, HashMap};

pub(super) struct Plan {
    /// Whether a binding needs a persistent slot after its initializer.
    pub used: Vec<bool>,
    /// Definitions that die after each complete binding/output expression.
    pub releases: Vec<Vec<usize>>,
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
        for (definition, end) in last.iter().enumerate() {
            if let Some(end) = end {
                releases[*end].push(definition);
            }
        }
        Self {
            used: last.iter().map(Option::is_some).collect(),
            releases,
        }
    }
}

/// Shared by locals and expression scratch across expanded calls. This free
/// set exists only in the compiler; runtime values carry no allocation metadata.
#[derive(Default)]
pub(super) struct Slots {
    free: BTreeSet<usize>,
    count: usize,
}

impl Slots {
    pub fn allocate(&mut self) -> usize {
        self.free.pop_first().unwrap_or_else(|| {
            let slot = self.count;
            self.count += 1;
            slot
        })
    }

    pub fn release(&mut self, slot: usize) {
        debug_assert!(slot < self.count);
        let inserted = self.free.insert(slot);
        debug_assert!(inserted, "slot released twice");
    }
}
