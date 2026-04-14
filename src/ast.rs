#[derive(Debug, Clone)]
pub struct Program {
    pub version: Version,
    pub uses: Vec<String>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

#[derive(Debug, Clone)]
pub enum Item {
    Concept(Concept),
    Rule(Rule),
    Reaction(Reaction),
}

/// A reaction is a block with declared side effects.
/// Unlike rules (pure computation), reactions DO things: print, write, send.
/// But every effect must be declared — no hidden side effects.
#[derive(Debug, Clone)]
pub struct Reaction {
    pub name: String,
    pub intention: String,
    pub source: SourceRef,
    pub trigger: String,
    pub effects: Vec<Effect>,
}

#[derive(Debug, Clone)]
pub struct Effect {
    pub kind: EffectKind,
    pub args: Vec<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EffectKind {
    Print,
}

#[derive(Debug, Clone)]
pub struct Concept {
    pub name: String,
    pub intention: String,
    pub source: SourceRef,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    pub range: Option<(i64, i64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Number,
    Bool,
    Text,
    Collection(String),
    Named(String),
    /// Result(T, E) — a declared failure path.
    /// A rule returning Result(T, E) produces either Ok(t) or Err(e).
    /// The failure path is part of the declared output, not an implicit panic.
    Result(Box<Type>, Box<Type>),
}

#[derive(Debug, Clone)]
pub struct SourceRef {
    pub file: String,
    pub line: u32,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub intention: String,
    pub source: SourceRef,
    pub input_name: String,
    pub input_ty: Type,
    pub output_name: String,
    pub output_ty: Type,
    pub logic: LogicStmt,
    pub proofs: Proofs,
    pub hints: Option<Hints>,
}

#[derive(Debug, Clone)]
pub struct Hints {
    /// Declared iff the AI believes SIMD is safe; the String is the justification.
    /// Verifier then cross-checks the claim (no calls, pure verdict, etc.).
    /// The justification is the audit surface: what makes the AI claim this is safe.
    pub vectorizable: Option<String>,
    pub parallel: Option<String>,
    pub cache_result: Option<String>,
    pub overflow: Option<OverflowHint>,
}

/// Overflow hint: the AI declares value bounds for the output.
/// The compiler verifies these bounds against the arithmetic in the logic.
/// If verified: no runtime overflow check needed (faster than default).
/// If unverifiable: the compiler rejects the hint and adds a runtime check.
#[derive(Debug, Clone)]
pub struct OverflowHint {
    pub min: i64,
    pub max: i64,
}

#[derive(Debug, Clone)]
pub struct LogicStmt {
    pub bindings: Vec<(String, Expr)>,
    pub target: String,
    pub value: Expr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantifierKind {
    All,
    Any,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Number(i64),
    Text(String),
    Ident(String),
    Field(Box<Expr>, String),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    Quantifier(QuantifierKind, Box<Expr>, String, Box<Expr>),
    /// fold(collection, initial, acc_name, item_name => body)
    /// Functional reduction: accumulates a value over a collection.
    Fold(Box<Expr>, Box<Expr>, String, String, Box<Expr>),
    /// map(collection, var => body)
    /// Transforms each element through a pure expression, returning collection(T).
    /// Same proof structure as Quantifier: reads/writes/calls from the body are
    /// checked with the lambda variable scoped out.
    Map(Box<Expr>, String, Box<Expr>),
    /// filter(collection, var => pred)
    /// Keeps elements for which pred is true, returning a collection of the same
    /// element type. Same proof structure as Quantifier.
    Filter(Box<Expr>, String, Box<Expr>),
    /// Ok(expr) — success constructor for a Result-typed output.
    /// Pass-through for purity/termination: inherits from inner expr.
    Ok(Box<Expr>),
    /// Err(expr) — failure constructor for a Result-typed output.
    /// The failure reason is a declared expression (typically text), not a panic.
    Err(Box<Expr>),
    /// match_result(target, ok_var => ok_body, err_var => err_body)
    /// The minimal Result consumer: both arms named and explicit, no implicit
    /// Err-propagation. If target is Ok(v), bind ok_var to v and evaluate
    /// ok_body; if Err(e), bind err_var to e and evaluate err_body.
    /// Same proof structure as Quantifier: lambda vars scoped out of reads.
    MatchResult(
        Box<Expr>,         // target
        String, Box<Expr>, // ok_var, ok_body
        String, Box<Expr>, // err_var, err_body
    ),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Gt,
    Lt,
    GtEq,
    LtEq,
    And,
    Or,
}

#[derive(Debug, Clone)]
pub struct Proofs {
    pub purity: Purity,
    pub termination: Termination,
    pub determinism: Determinism,
}

#[derive(Debug, Clone)]
pub struct Purity {
    pub reads: Vec<Path>,
    pub writes: Vec<Path>,
    pub calls: Vec<Path>,
    pub verdict: PurityVerdict,
}

#[derive(Debug, Clone)]
pub struct Path {
    pub segments: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum PurityVerdict {
    Pure,
    PureExcept(Vec<Path>),
    Impure,
}

#[derive(Debug, Clone)]
pub struct Termination {
    pub form: TerminationForm,
    pub bound: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationForm {
    ConstantBound,
    VariableBound,
    DecreasingRecursion,
    Unproven,
}

#[derive(Debug, Clone)]
pub struct Determinism {
    pub form: DeterminismForm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeterminismForm {
    Total,
    Conditional,
    Nondeterministic,
}
