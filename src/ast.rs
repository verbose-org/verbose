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
    pub vectorizable: Option<bool>,
    pub parallel: Option<bool>,
    pub cache_result: Option<bool>,
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

#[derive(Debug, Clone)]
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
