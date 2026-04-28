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
    /// Phase 11 slice 1: a top-level outbound TCP connection. The host
    /// (IPv4 literal) and port are compile-time constants; the request
    /// bytes are evaluated at the call site via `fetch(<name>, <bytes>)`.
    /// Declaring the connection at top-level (rather than inline at every
    /// fetch site) gives the auditor a single place to enumerate every
    /// endpoint the program can touch — same discipline as Resource for
    /// filesystem reads. No DNS, no TLS, no keep-alive in slice 1: the
    /// surface stays narrow on purpose.
    Connection(Connection),
}

/// Phase 11 slice 1: a declared outbound TCP destination. Mirrors the
/// shape established for `Resource` and `Service` (intention + source for
/// audit, declared bounds enforced by the verifier).
///
/// `host` is parsed as a quad-dotted IPv4 literal — no DNS lookup, no
/// domain names, no IPv6. The auditor reads the source (or `strings` the
/// binary) and sees every IP/port the program can attempt to dial.
///
/// `max_response` bounds the response buffer. Verifier enforces
/// `1 <= max_response <= 64 MiB` so the buffer can be allocated on the
/// stack at compile time. Bytes beyond this bound are simply not read by
/// the read syscall — slice 1 is one-shot blocking, no streaming.
#[derive(Debug, Clone)]
pub struct Connection {
    pub name: String,
    pub intention: String,
    pub source: SourceRef,
    /// IPv4 host as a dotted-quad literal (e.g. "127.0.0.1"). Each octet
    /// 0..=255; the parser rejects anything else (no DNS resolution, no
    /// IPv6, no localhost shorthand).
    pub host: String,
    /// TCP port, 1..=65535.
    pub port: u16,
    /// Stack buffer size for the response read; verifier-bounded to
    /// [1, 64 MiB].
    pub max_response: u32,
    /// Phase 11 slice 1: only `Abort` accepted today (mirrors slice 9.1
    /// pattern). On socket / connect / write / read failure the process
    /// exits with status 1. `Drop` lands in a later slice if needed;
    /// keeping the strict policy as the only option in slice 1 keeps
    /// the failure mode obvious.
    pub on_connect_error: ErrorPolicy,
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
    /// Phase 9 slice 9.4: opt-in `cache: true | false` (default `false`).
    ///
    /// When `false` (the default): for HTTP services the open/read/close
    /// syscalls fire INSIDE the accept loop — every request consults the
    /// file fresh, so on-disk edits are picked up immediately at the cost
    /// of one open()+read()+close() per request (~3 µs of syscall work).
    /// Sequential mode + `cache: false` is byte-for-byte identical to the
    /// pre-9.4 binary.
    ///
    /// When `true`: for HTTP services the read sequence runs ONCE at
    /// server startup, between LISTEN and the accept_top label (so the
    /// cached buffer also crosses fork() in `concurrency: forked` mode —
    /// children inherit the parent's already-populated slot via COW with
    /// no per-child read cost). Trade staleness for the per-request open
    /// overhead; ideal for static assets that are stable across the
    /// server's lifetime.
    ///
    /// For rules: the resource is already read once per rule invocation
    /// (above loop_top), so `cache: true` is a no-op there. Allowed for
    /// grammar uniformity; documented as harmless.
    ///
    /// `cache: true` requires `on_read_error: abort` — the parser already
    /// rejects `drop` so this is structurally guaranteed; no extra check
    /// in the verifier.
    pub cache: bool,
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
    /// Phase 8 slice 8a/8e: zero or more `log:` blocks. Each block declares
    /// one effect (today: `append_file <path> <content>`) plus its own
    /// `on_error` policy, and fires once per service invocation after the
    /// handler body runs and before the response is written. Multiple
    /// blocks fire in source order, each independently — a `Drop` log
    /// can sit next to an `Abort` log so an operator can have a
    /// best-effort metrics sink alongside a fail-closed audit sink.
    /// Empty Vec means no logging (silent service).
    pub logs: Vec<LogBlock>,
    /// Phase 10 slice 10: how the accept loop dispatches each connection.
    /// Defaults to `Sequential` so existing services compile byte-for-byte
    /// identically — the slice is purely additive. `Forked` makes the
    /// service `fork()` after each accept and have the child run the
    /// handler / log / response while the parent immediately closes the
    /// client fd and loops back to accept; `SIGCHLD` is set to `SIG_IGN`
    /// once at startup so the kernel auto-reaps the children with no
    /// per-request bookkeeping. The knob is service-level (not per-rule)
    /// because concurrency belongs to the wire-facing layer, not the
    /// pure logic the handler computes.
    pub concurrency: ConcurrencyMode,
}

/// Phase 10 slice 10: how a service's accept loop dispatches each
/// connection. Closed set; new variants (e.g. a worker pool) would land
/// in their own slice with explicit emitter behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrencyMode {
    /// Default. One connection at a time — accept, handle to completion,
    /// loop back. The shape every Phase 7/8/9 slice was emitted under;
    /// keeping it as the default means absent-knob services compile to
    /// byte-for-byte identical binaries.
    Sequential,
    /// `fork()` after each accept. Parent closes the client fd and loops
    /// back to accept; child runs the handler / log / response then
    /// `sys_exit(0)`. `SIGCHLD` is set to `SIG_IGN` once at startup so
    /// no `wait`/`waitpid` is needed and no zombies accumulate.
    Forked,
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

/// Phase 8 slice 8e: one log block declared on a service. Carries its
/// own effect AND its own on_error policy, so multiple blocks on the
/// same service can mix policies (e.g., a `Drop` metrics sink next to
/// an `Abort` audit sink). Today the effect is always `AppendFile` —
/// the parser refuses other shapes — but the type is the shared
/// `Effect` enum so future log effects (e.g., `print` for a tee to
/// stdout) compose without a second wrapper type.
#[derive(Debug, Clone)]
pub struct LogBlock {
    pub effect: Effect,
    pub on_error: ErrorPolicy,
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
    /// Phase 11 slice 1: open a TCP socket to a declared `connection`,
    /// send the given request bytes, read up to the connection's
    /// `max_response` bytes, close the socket, and return the response
    /// as `text`. The String names the declared connection; the Expr
    /// is the request bytes (must produce text). The verifier rejects
    /// references to undeclared connections, and the rule's `reads:`
    /// purity proof must list every connection referenced (same shape
    /// as `Read` for resources). Slice 1 is one-shot blocking: at most
    /// one fetch per connection per rule invocation.
    Fetch(String, Box<Expr>),
    /// Phase 12 slice (json_escape): pure text-transform primitive that
    /// escapes 5 JSON-significant bytes in its input — `"`, `\`, `\n`,
    /// `\r`, `\t` — leaving every other byte unchanged. The result is
    /// `text` and is a function of the input alone (no syscalls, no
    /// state). Introduced so that JSONL log content composed via
    /// `concat(...)` of request fields stays valid JSON when those
    /// fields contain JSON-significant characters. Other bytes (including
    /// `\b`, `\f`, control chars below 0x20) pass through unchanged in
    /// this slice; `\u00XX` escaping lands in a follow-up if a real use
    /// case appears.
    JsonEscape(Box<Expr>),
    /// `parse_int(<text_expr>)` — convert a text value to a number.
    /// Accepted shape: optional leading `-`, then 1+ ASCII digits, then
    /// end-of-input. Anything else (whitespace, non-digit, empty input)
    /// aborts the binary with sys_exit(1) — same fail-closed posture as
    /// `on_read_error: abort`. Introduced so that numbers loaded from a
    /// file (`parse_int(read(threshold))`) flow into number contexts
    /// without forcing every caller to wrap in match_result. Native
    /// emits a strict scan loop length-aware for both NUL-terminated
    /// argv text and Read-bound (ptr, len) text.
    ParseInt(Box<Expr>),
    /// `now_unix()` — current Unix epoch seconds as a number. Sampled
    /// ONCE per rule invocation (clock_gettime(CLOCK_REALTIME) above
    /// the record loop), then every reference in the rule's logic
    /// loads the same captured value from a dedicated rbp slot.
    /// Mirror of how `req.timestamp` works in HTTP services (slice 8c)
    /// — one wall-clock sample per request, every log line in the
    /// scope sees the same instant. The clock is an external effect
    /// (non-deterministic source), so the verifier requires the rule's
    /// `reads:` proof to declare the synthetic name `now`. Auditors
    /// grep for `now` in `reads:` to find every rule that touches the
    /// system clock — same audit shape as `read(<resource>)`.
    NowUnix,
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
