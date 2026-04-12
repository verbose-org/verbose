#[derive(Debug)]
pub struct Program {
    pub version: Version,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

#[derive(Debug)]
pub enum Item {
    Concept(Concept),
    Rule(Rule),
}

#[derive(Debug)]
pub struct Concept {
    pub name: String,
    pub intention: String,
    pub source: SourceRef,
    pub fields: Vec<Field>,
}

#[derive(Debug)]
pub struct Field {
    pub name: String,
    pub ty: Type,
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

#[derive(Debug)]
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
}

#[derive(Debug)]
pub struct LogicStmt {
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
    Eq,
    NotEq,
    Gt,
    Lt,
    GtEq,
    LtEq,
    And,
    Or,
}

#[derive(Debug)]
pub struct Proofs {
    pub purity: Purity,
    pub termination: Termination,
    pub determinism: Determinism,
}

#[derive(Debug)]
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

#[derive(Debug)]
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

#[derive(Debug)]
pub struct Determinism {
    pub form: DeterminismForm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeterminismForm {
    Total,
    Conditional,
    Nondeterministic,
}
