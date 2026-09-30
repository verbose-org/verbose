//! Lexical type facts for constructor obligations. No backend restrictions or
//! runtime representation decisions belong here. Unknown facts propagate until
//! a constructor needs them; the legacy general checker keeps its own contract.
use super::{type_display, VerifyError};
use crate::ast::*;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
enum Fact {
    Known(Type),
    Result(Box<Fact>, Box<Fact>),
    // Only an unconstructed Result arm can be absent. A variable bound from it
    // is UNKNOWN, never a wildcard proof of a constructor field's type.
    Absent,
    Unknown,
}
impl Fact {
    fn from_type(t: &Type) -> Self {
        match t {
            Type::Result(ok, err) => Self::Result(
                Box::new(Self::from_type(ok)),
                Box::new(Self::from_type(err)),
            ),
            _ => Self::Known(t.clone()),
        }
    }
    fn complete_type(&self) -> Option<Type> {
        match self {
            Self::Known(t) => Some(t.clone()),
            Self::Result(ok, err) => Some(Type::Result(
                Box::new(ok.complete_type()?),
                Box::new(err.complete_type()?),
            )),
            _ => None,
        }
    }
    fn fits(&self, t: &Type) -> bool {
        match (self, t) {
            (Self::Known(actual), _) => actual == t,
            (Self::Result(ok, err), Type::Result(t, e)) => {
                (matches!(**ok, Self::Absent) || ok.fits(t))
                    && (matches!(**err, Self::Absent) || err.fits(e))
            }
            _ => false,
        }
    }
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Absent, t) | (t, Self::Absent) => t,
            (Self::Result(a, b), Self::Result(c, d)) => {
                Self::Result(Box::new(a.join(*c)), Box::new(b.join(*d)))
            }
            (a, b) if a == b => a,
            _ => Self::Unknown,
        }
    }
    fn bound(self) -> Self {
        if matches!(self, Self::Absent) {
            Self::Unknown
        } else {
            self
        }
    }
    fn element(&self) -> Self {
        match self {
            Self::Known(Type::Collection(name)) => Self::Known(match name.as_str() {
                "number" => Type::Number,
                "bool" => Type::Bool,
                "text" => Type::Text,
                "bytes" => Type::Bytes,
                _ => Type::Named(name.clone()),
            }),
            _ => Self::Unknown,
        }
    }
    fn collection(self) -> Self {
        match self {
            Self::Known(
                t @ (Type::Number | Type::Bool | Type::Text | Type::Bytes | Type::Named(_)),
            ) => Self::Known(Type::Collection(type_display(&t))),
            _ => Self::Unknown,
        }
    }
}
type Env = HashMap<String, Fact>;

pub(super) fn check_rule(
    rule: &Rule,
    rules: &[&Rule],
    concepts: &HashMap<String, &Concept>,
    state: Option<&Concept>,
    report_obligations: bool,
    errors: &mut Vec<VerifyError>,
) {
    let mut env = Env::new();
    // `state` has a synthetic concept which is not in the program namespace.
    let mut concepts = concepts.clone();
    if let Some(state) = state {
        concepts.insert(state.name.clone(), state);
        env.insert("state".into(), Fact::Known(Type::Named(state.name.clone())));
    }
    env.insert(rule.input_name.clone(), Fact::from_type(&rule.input_ty));
    if let (Some(name), Some(ty)) = (&rule.context_name, &rule.context_ty) {
        env.insert(name.clone(), Fact::from_type(ty));
    }
    let mut checker = Checker {
        rule,
        rules,
        concepts: &concepts,
        errors,
        constructors: 0,
        report_obligations,
    };
    for (name, rhs) in &rule.logic.bindings {
        let fact = checker.walk(rhs, &env);
        env.insert(name.clone(), fact); // including unknown: masks the old value
    }
    checker.walk(&rule.logic.value, &env);
}

struct Checker<'a, 'b> {
    rule: &'a Rule,
    rules: &'a [&'a Rule],
    concepts: &'a HashMap<String, &'a Concept>,
    errors: &'b mut Vec<VerifyError>,
    constructors: usize,
    report_obligations: bool,
}
impl Checker<'_, '_> {
    fn error(&mut self, context: &str, message: String) {
        if !self.report_obligations {
            return;
        }
        self.errors.push(VerifyError {
            context: format!("rule '{}' / constructor {}", self.rule.name, context),
            message,
        });
    }
    fn produced(valid: bool, ty: Type) -> Fact {
        if valid {
            Fact::from_type(&ty)
        } else {
            Fact::Unknown
        }
    }
    fn byte_addressable(&self, e: &Expr, f: &Fact, env: &Env) -> bool {
        f.fits(&Type::Text)
            || (f.fits(&Type::Bytes)
                && (matches!(e, Expr::Bytes(_) | Expr::Random(_))
                    || matches!(e, Expr::Field(base, _) if matches!(base.as_ref(), Expr::Ident(n)
                if n == &self.rule.input_name && env.get(n) == Some(&Fact::from_type(&self.rule.input_ty))))))
    }
    /// Retain the existing precise operand diagnostics when the legacy checker
    /// can read this exact scope. This only explains an already failed field
    /// obligation; it never establishes a type or changes acceptance.
    fn explain_invalid_field(&mut self, value: &Expr, expected: &Type, env: &Env) {
        // Legacy inference special-cases the input and does not enter binder
        // scopes. Do not let either shortcut report an outer value's type.
        if env.get(&self.rule.input_name) != Some(&Fact::from_type(&self.rule.input_ty))
            || !super::collect_lambda_bound_names(value).is_empty()
        {
            return;
        }
        let mut bindings = super::Bindings::default();
        for (name, fact) in env {
            if let Some(ty) = fact.complete_type() {
                if let Some(concept) = super::record_concept_of(&ty, self.concepts) {
                    bindings.records.insert(name.clone(), concept);
                } else {
                    bindings.scalars.insert(name.clone(), ty);
                }
            }
        }
        let input = match &self.rule.input_ty {
            Type::Named(name) => self.concepts.get(name).copied(),
            _ => None,
        };
        super::check_expr_against(
            value,
            expected,
            self.rule,
            self.rules,
            input,
            self.concepts,
            &bindings,
            self.errors,
        );
    }
    fn walk(&mut self, e: &Expr, env: &Env) -> Fact {
        use Fact::{Known, Unknown};
        match e {
            Expr::Number(_) | Expr::NowUnix => Known(Type::Number),
            Expr::Text(_) | Expr::Read(_) => Known(Type::Text),
            Expr::Bytes(_) | Expr::Random(_) => Known(Type::Bytes),
            Expr::Ident(n) => env.get(n).cloned().unwrap_or(Unknown),
            Expr::Field(base, name) => {
                let fact = self.walk(base, env);
                if let Known(Type::Named(n)) = fact {
                    if let Some(field) = self
                        .concepts
                        .get(&n)
                        .and_then(|c| c.fields.iter().find(|f| f.name == *name))
                    {
                        return Fact::from_type(&field.ty);
                    }
                }
                Unknown
            }
            Expr::Record(n, fields) | Expr::VariantConstruct(n, _, fields) => {
                self.constructors += 1;
                let decl = self.concepts.get(n).and_then(|c| match e {
                    Expr::Record(..) if c.variants.is_empty() => Some(c.fields.as_slice()),
                    Expr::VariantConstruct(_, v, _) => c
                        .variants
                        .iter()
                        .find(|d| d.name == *v)
                        .map(|v| v.fields.as_slice()),
                    _ => None,
                });
                let label = if let Expr::VariantConstruct(_, v, _) = e {
                    format!("'{n}::{v}'")
                } else {
                    format!("'{n}'")
                };
                let mut valid = decl.is_some();
                if decl.is_none() {
                    self.error(
                        &label,
                        "unknown concept or incompatible constructor form/variant".into(),
                    );
                }
                let mut seen = HashSet::new();
                for (field, value) in fields {
                    let actual = self.walk(value, env);
                    let context = format!("{label} / field '{field}'");
                    if !seen.insert(field) {
                        valid = false;
                        self.error(&context, format!("duplicate field '{field}'"));
                    }
                    if let Some(decl) = decl {
                        if let Some(d) = decl.iter().find(|d| d.name == *field) {
                            if !actual.fits(&d.ty) {
                                valid = false;
                                self.explain_invalid_field(value, &d.ty, env);
                                let message = match &actual {
                                    Known(t) => format!("expression has type '{}' but context expects '{}'", type_display(t), type_display(&d.ty)),
                                    _ => format!("cannot establish constructor field type '{}': unknown binding, incompatible branches or invalid operands", type_display(&d.ty)),
                                };
                                self.error(&context, message);
                            }
                        } else {
                            valid = false;
                            self.error(&context, format!("unknown field '{field}'"));
                        }
                    }
                }
                if let Some(decl) = decl {
                    for d in decl {
                        if !seen.contains(&d.name) {
                            valid = false;
                            self.error(&label, format!("missing field '{}'", d.name));
                        }
                    }
                }
                Self::produced(valid, Type::Named(n.clone()))
            }
            Expr::Call(name, args) => {
                let actual: Vec<_> = args.iter().map(|a| self.walk(a, env)).collect();
                if let (Some(callee), [arg]) = (
                    self.rules.iter().find(|r| r.name == *name),
                    actual.as_slice(),
                ) {
                    Self::produced(arg.fits(&callee.input_ty), callee.output_ty.clone())
                } else {
                    Unknown
                }
            }
            Expr::If(c, t, f) => {
                let cond = self.walk(c, env);
                let yes = self.walk(t, env);
                let no = self.walk(f, env);
                if cond.fits(&Type::Bool) {
                    yes.join(no)
                } else {
                    Unknown
                }
            }
            Expr::Ok(v) => Fact::Result(Box::new(self.walk(v, env)), Box::new(Fact::Absent)),
            Expr::Err(v) => Fact::Result(Box::new(Fact::Absent), Box::new(self.walk(v, env))),
            Expr::MatchResult(target, ok, yes, err, no) => {
                let target = self.walk(target, env);
                let valid = matches!(target, Fact::Result(..));
                let (a, b) = match target {
                    Fact::Result(a, b) => (a.bound(), b.bound()),
                    _ => (Unknown, Unknown),
                };
                let mut scope = env.clone();
                scope.insert(ok.clone(), a);
                let yes = self.walk(yes, &scope);
                let mut scope = env.clone();
                scope.insert(err.clone(), b);
                let no = self.walk(no, &scope);
                if valid {
                    yes.join(no)
                } else {
                    Unknown
                }
            }
            Expr::MatchVariant(target, arms) => {
                let target = self.walk(target, env);
                let concept = if let Known(Type::Named(n)) = target {
                    self.concepts.get(&n).copied()
                } else {
                    None
                };
                let mut valid = concept.is_some_and(|c| !c.variants.is_empty());
                let mut seen = HashSet::new();
                let mut result = None;
                for arm in arms {
                    let decl = concept
                        .and_then(|c| c.variants.iter().find(|v| v.name == arm.variant_name));
                    valid &= seen.insert(&arm.variant_name)
                        && decl.is_some_and(|d| d.fields.len() == arm.binders.len());
                    let mut scope = env.clone();
                    let mut names = HashSet::new();
                    for (i, name) in arm.binders.iter().enumerate() {
                        if let Some(name) = name {
                            valid &= names.insert(name);
                            scope.insert(
                                name.clone(),
                                decl.and_then(|d| d.fields.get(i))
                                    .map(|f| Fact::from_type(&f.ty))
                                    .unwrap_or(Unknown),
                            );
                        }
                    }
                    let fact = self.walk(&arm.body, &scope);
                    result = Some(match result {
                        None => fact,
                        Some(r) => Fact::join(r, fact),
                    });
                }
                valid &= concept.is_some_and(|c| c.variants.iter().all(|v| seen.contains(&v.name)));
                if valid {
                    result.unwrap_or(Unknown)
                } else {
                    Unknown
                }
            }
            Expr::Quantifier(_, coll, item, body)
            | Expr::Map(coll, item, body)
            | Expr::Filter(coll, item, body) => {
                let coll = self.walk(coll, env);
                let element = coll.element();
                let valid = !matches!(element, Unknown);
                let mut scope = env.clone();
                scope.insert(item.clone(), element);
                let body = self.walk(body, &scope);
                if !valid {
                    return Unknown;
                }
                match e {
                    Expr::Map(..) => body.collection(),
                    Expr::Filter(..) => {
                        if body.fits(&Type::Bool) {
                            coll
                        } else {
                            Unknown
                        }
                    }
                    _ => Self::produced(body.fits(&Type::Bool), Type::Bool),
                }
            }
            Expr::Fold(coll, init, acc, item, body) => {
                let coll = self.walk(coll, env).element();
                let init = self.walk(init, env);
                let mut scope = env.clone();
                scope.insert(acc.clone(), init.clone());
                scope.insert(item.clone(), coll.clone());
                let before = self.constructors;
                let body = self.walk(body, &scope);
                if body != init && self.constructors != before {
                    self.error("in fold", "cannot establish constructor obligations across iterations: fold accumulator type changes or is unresolved".into());
                }
                if matches!(coll, Unknown) || body != init {
                    Unknown
                } else {
                    init.join(body)
                }
            }
            Expr::FoldBytes(text, init, acc, byte, index, body) => {
                let text = self.walk(text, env);
                let init = self.walk(init, env);
                let mut scope = env.clone();
                scope.insert(acc.clone(), init.clone());
                scope.insert(byte.clone(), Known(Type::Number));
                scope.insert(index.clone(), Known(Type::Number));
                let body = self.walk(body, &scope);
                Self::produced(
                    text.fits(&Type::Text) && init.fits(&Type::Number) && body.fits(&Type::Number),
                    Type::Number,
                )
            }
            Expr::Binary(op, a, b) => {
                let a = self.walk(a, env);
                let b = self.walk(b, env);
                let numeric = a.fits(&Type::Number) && b.fits(&Type::Number);
                let (valid, ty) = match op {
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                        (numeric, Type::Number)
                    }
                    BinOp::Gt | BinOp::Lt | BinOp::GtEq | BinOp::LtEq => (numeric, Type::Bool),
                    BinOp::And | BinOp::Or => {
                        (a.fits(&Type::Bool) && b.fits(&Type::Bool), Type::Bool)
                    }
                    BinOp::Eq | BinOp::NotEq => (
                        numeric || (a.fits(&Type::Text) && b.fits(&Type::Text)),
                        Type::Bool,
                    ),
                };
                Self::produced(valid, ty)
            }
            Expr::Min(a, b)
            | Expr::Max(a, b)
            | Expr::BitAnd(a, b)
            | Expr::BitOr(a, b)
            | Expr::BitXor(a, b)
            | Expr::Shl(a, b)
            | Expr::Shr(a, b) => {
                let a = self.walk(a, env);
                let b = self.walk(b, env);
                Self::produced(a.fits(&Type::Number) && b.fits(&Type::Number), Type::Number)
            }
            Expr::StartsWith(a, b) | Expr::EndsWith(a, b) | Expr::Contains(a, b) => {
                let a = self.walk(a, env);
                let b = self.walk(b, env);
                Self::produced(a.fits(&Type::Text) && b.fits(&Type::Text), Type::Bool)
            }
            Expr::Not(v)
            | Expr::Neg(v)
            | Expr::Abs(v)
            | Expr::BitNot(v)
            | Expr::ParseInt(v)
            | Expr::JsonEscape(v)
            | Expr::Le32(v)
            | Expr::Le64(v)
            | Expr::AbortIf(v)
            | Expr::Fetch(_, v) => {
                let (input, output) = match e {
                    Expr::Not(_) => (Type::Bool, Type::Bool),
                    Expr::ParseInt(_) => (Type::Text, Type::Number),
                    Expr::JsonEscape(_) | Expr::Fetch(..) => (Type::Text, Type::Text),
                    Expr::Le32(_) | Expr::Le64(_) | Expr::AbortIf(_) => (Type::Number, Type::Bytes),
                    _ => (Type::Number, Type::Number),
                };
                Self::produced(self.walk(v, env).fits(&input), output)
            }
            Expr::Length(v) => {
                let f = self.walk(v, env);
                Self::produced(self.byte_addressable(v, &f, env), Type::Number)
            }
            Expr::ByteAt(v, index) | Expr::TryByteAt(v, index) => {
                let f = self.walk(v, env);
                let index = self.walk(index, env);
                let valid = self.byte_addressable(v, &f, env) && index.fits(&Type::Number);
                Self::produced(
                    valid,
                    if matches!(e, Expr::TryByteAt(..)) {
                        crate::bounds::result_type()
                    } else {
                        Type::Number
                    },
                )
            }
            Expr::Substring(text, start, end) => {
                let text = self.walk(text, env);
                let start = self.walk(start, env);
                let end = self.walk(end, env);
                Self::produced(
                    text.fits(&Type::Text) && start.fits(&Type::Number) && end.fits(&Type::Number),
                    Type::Text,
                )
            }
            Expr::ArenaScope(v) => {
                let v = self.walk(v, env);
                if v.fits(&Type::Number) || v.fits(&Type::Bytes) {
                    v
                } else {
                    Unknown
                }
            }
            Expr::Concat(args) => {
                let args: Vec<_> = args.iter().map(|a| self.walk(a, env)).collect();
                let bytes = args.iter().any(|a| a.fits(&Type::Bytes));
                let valid = args.iter().all(|a| {
                    if bytes {
                        a.fits(&Type::Bytes)
                    } else {
                        a.fits(&Type::Number) || a.fits(&Type::Bool) || a.fits(&Type::Text)
                    }
                });
                Self::produced(valid, if bytes { Type::Bytes } else { Type::Text })
            }
        }
    }
}
