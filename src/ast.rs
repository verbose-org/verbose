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
    Service(Service),
    /// Phase 9 slice 1: a top-level read-only file resource. The path is a
    /// compile-time literal; the file contents are read at runtime by any
    /// rule that references the resource via `read(<name>)`. Declaring the
    /// resource at top-level (rather than inline at every read site) gives
    /// the auditor a single place to enumerate every file the program can
    /// touch.
    Resource(Resource),
}

/// Phase 9 slice 1: a declared read-only file resource. Mirrors the shape
/// established for `Service` and `Concept` (intention + source for audit,
/// declared bounds enforced by the verifier).
///
/// `max_bytes` is the upper bound on a single read; the verifier enforces
/// `1 <= max_bytes <= 64 MiB` so the buffer can be allocated on the stack
/// at compile time without relying on heap or guessed sizes. Reads that
/// would exceed `max_bytes` are truncated by the read syscall (slice 1 has
/// no streaming support — that is a later slice).
#[derive(Debug, Clone)]
pub struct Resource {
    pub name: String,
    pub intention: String,
    pub source: SourceRef,
    /// Filesystem path, baked into the binary as a literal at the open site.
    /// No concat, no field substitution, no request-derived paths — the
    /// auditor reads the source (or `strings` the binary) and sees every
    /// file the program can attempt to open.
    pub path: String,
    /// Stack buffer size for the read; verifier-bounded to [1, 64 MiB].
    pub max_bytes: u32,
    /// Phase 9 slice 1: only `Abort` accepted today (mirrors slice 8d). On
    /// open or read failure the process exits with status 1. `Drop`
    /// (silent-ignore) lands in a later slice if needed; making the strict
    /// policy the only option in slice 1 keeps the failure mode obvious.
    pub on_read_error: ErrorPolicy,
}

/// A service is a long-running program declaration. It binds a listener
/// (protocol, port, bounded request size) to a handler rule that runs once
/// per incoming request. See docs/phase-7-design.md for the rationale and
/// the closed set of supported protocols.
///
/// First Phase 7 slice: AST + parser + verifier only. Native emission is a
/// follow-up commit — trying to compile a program containing a Service with
/// --native today returns a clear "not yet implemented" error.
#[derive(Debug, Clone)]
pub struct Service {
    pub name: String,
    pub intention: String,
    pub source: SourceRef,
    pub protocol: Protocol,
    pub port: u16,
    pub max_request: u32,
    pub handler: String,
    /// Phase 8 slice 8a: optional `log:` block. When present, the declared
    /// effect fires once per service invocation, after the handler body
    /// runs and before the response is written. Only `AppendFile` is
    /// accepted by the parser (slice 8a scope); multi-effect logging and
    /// other sinks land in later slices.
    pub log: Option<Effect>,
    /// Phase 8 slice 8d: error policy applied when the log effect's
    /// underlying syscalls (open / write) fail. `Drop` (the default and
    /// the slice-8a behaviour) silently ignores the error so a downed log
    /// surface never takes the request path down. `Abort` exits the
    /// process with status 1, which is what an Article 12 audit chain
    /// needs: if the log line cannot be persisted, the service must not
    /// claim to have served the request. Meaningless when `log` is None;
    /// parser fills the default in that case.
    pub log_on_error: ErrorPolicy,
}

/// Phase 8 slice 8d: how a service should react when its log effect
/// fails to write. Closed set; new variants need to land one at a time
/// with explicit emitter behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPolicy {
    /// Default. Silently ignore syscall failures so the service keeps
    /// serving requests even if the log target is unreachable.
    Drop,
    /// Exit the process with status 1 when any log syscall fails. Use
    /// this when the audit trail is part of the service's contract: no
    /// log = no claim to have processed the request.
    Abort,
}

/// Closed set of protocols a service may declare. Unknown names are rejected
/// by the parser — no open-ended strings, no user-written protocol parsers
/// in Phase 7. Additional variants land one at a time in later phases, each
/// with its own protocol parser + serializer in the native backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Raw TCP: the handler receives the received bytes (bounded by
    /// max_request) and returns bytes to send back. No structured request /
    /// response. Simplest first protocol; no built-in parser needed.
    RawTcp,
    /// HTTP/1.0: the compiler parses one GET/POST/... request off the
    /// wire (method + path only, headers discarded) into a built-in
    /// `HttpRequest` concept; the handler returns a built-in
    /// `HttpResponse` concept the compiler serialises as a minimal
    /// HTTP/1.0 response (status + Content-Length + body). The built-in
    /// concepts are synthesised by the verifier when any Http10 service
    /// is declared; user concepts reusing those names are rejected.
    Http10,
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

/// A declared reaction effect. Each variant carries exactly the fields its
/// kind needs; there is no generic "args bag". Adding a new effect is a new
/// variant with typed fields, so the pattern cannot degrade into "untagged
/// stringly-typed args".
#[derive(Debug, Clone)]
pub enum Effect {
    /// print expr... — write to stdout. Arguments are printed space-separated.
    Print(Vec<Expr>),
    /// append_file "path" content — append the content text to the file.
    /// The path is a string LITERAL (not an expression), so the auditor can
    /// read the source and see every file path this program can touch.
    /// No implicit newline: the content is exactly what is written.
    AppendFile { path: String, content: Expr },
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
    /// Raw bytes: the arbitrary-content counterpart to Text. Introduced in
    /// Phase 7 slice 2a so that TCP socket input (which can contain NUL
    /// bytes, binary data, or invalid UTF-8) has a honest type that does
    /// not pretend to be text. The byte bound `[..N]` is declared on the
    /// concept field, same mechanism as text. Bytes never implicitly
    /// convert to or from Text — the isolation is the point. See
    /// docs/phase-7-design.md for rationale.
    Bytes,
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
    /// Architectural layer (optional).
    /// When declared, the verifier enforces that this rule only calls other
    /// layered rules, and only those of layers this layer is allowed to call.
    /// Rules without a declared layer are unchecked (backward-compatible),
    /// but a layered rule may NOT call an unlayered one — layered code is a
    /// sealed subgraph.
    pub layer: Option<Layer>,
    /// Optional context input: a second concept whose fields are read ONCE
    /// (not per-record). Used for config/policy/threshold data that is
    /// constant across all records. None for single-input rules.
    pub context_name: Option<String>,
    pub context_ty: Option<Type>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// Core business concepts. Can only call other domain rules.
    Domain,
    /// Use cases and orchestrations. Can call domain or application rules.
    Application,
    /// Boundary-facing rules. Can call any layered rule.
    Interface,
}

impl Layer {
    pub fn as_str(&self) -> &'static str {
        match self {
            Layer::Domain => "domain",
            Layer::Application => "application",
            Layer::Interface => "interface",
        }
    }

    /// Returns true if a rule at `self` is allowed to call a rule at `target`.
    /// Captures the stratification: domain < application < interface.
    pub fn can_call(&self, target: Layer) -> bool {
        match self {
            Layer::Domain => target == Layer::Domain,
            Layer::Application => matches!(target, Layer::Domain | Layer::Application),
            Layer::Interface => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Hints {
    /// Declared iff the AI believes SIMD is safe; the String is the justification.
    /// Verifier then cross-checks the claim (no calls, etc.).
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
    /// ConceptName { field: expr, field: expr, ... }
    /// Constructs a record of the named concept. The verifier cross-checks
    /// that the field set matches the concept's declaration exactly and
    /// that each field's expression type matches the declared field type.
    Record(String, Vec<(String, Expr)>),
    /// concat(e1, e2, ...) — variadic text builder.
    /// Each argument is converted to its text form (Number -> decimal,
    /// Bool -> "true"/"false", Text as-is); the result is text. Non-scalar
    /// arguments (collection, Result, record) are rejected by the verifier.
    /// Not an operator overload on `+` — we keep `+` strictly numeric and
    /// make text composition an explicit, audit-visible call.
    Concat(Vec<Expr>),
    /// Phase 9 slice 1: read the contents of a top-level `resource` as
    /// text. The string identifies the declared resource by name; the
    /// verifier rejects references to undeclared resources, and the
    /// rule's `reads:` purity proof must list every resource referenced
    /// in its logic. Returns `text` — usable in any text-typed position.
    Read(String),
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
}

#[derive(Debug, Clone)]
pub struct Purity {
    pub reads: Vec<Path>,
    pub calls: Vec<Path>,
}

#[derive(Debug, Clone)]
pub struct Path {
    pub segments: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Termination {
    pub bound: Option<i64>,
}
