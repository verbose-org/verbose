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
    /// Phase B slice 1: a group of mutually-recursive sum-type concepts
    /// sharing a single set of `[max_depth: N, max_nodes: M]` bounds.
    ///
    /// The group is the AST-level analogue of a strongly-connected
    /// component in the type graph: every concept inside may reference
    /// every other concept in the same group via `Type::Named(...)`
    /// inside variant payloads, and cycles between them are EXPECTED
    /// (that's the whole point of the construct). Bounds apply to the
    /// combined tree shape across all concepts in the group, not
    /// per-concept — see docs/recursive-types-design.md §3.2 / §4.
    ///
    /// In B.1 this is parser+verifier-only: a program that contains a
    /// `ConceptGroup` plus a rule whose input/output references a
    /// concept inside it is refused at verify time with a clear
    /// breadcrumb pointing at slice B.3+. Native and codegen also
    /// refuse with a forward-looking message; the optimizer passes
    /// through unchanged.
    ConceptGroup(ConceptGroup),
}

/// Phase B slice 1: a mutually-recursive concept group.
///
/// Carries the shared `[max_depth, max_nodes]` SCC bounds and the
/// sequence of inner concepts. Inner concepts use the same `Concept`
/// shape as top-level concepts so the existing variant / field / type
/// parsing machinery applies unchanged; the only difference is that
/// `Type::Named(N)` references inside their variants may resolve to
/// other concepts in the SAME group (intra-group recursion). Top-level
/// concept declarations remain non-recursive — the verifier rejects
/// self-reference and cross-concept reference outside a group.
#[derive(Debug, Clone)]
pub struct ConceptGroup {
    pub name: String,
    pub intention: String,
    pub source: SourceRef,
    /// Maximum recursion depth across all concepts in the group. Bounded
    /// to `(0, 65535]` by the verifier — large enough for real ASTs,
    /// small enough that index arithmetic stays within 16 bits when the
    /// node count also fits (see docs/recursive-types-design.md §6 +
    /// Q2). The slice-1 verifier check is purely sanity (positive and
    /// not absurd); the actual depth-bound emitter wiring lands in
    /// Phase B.4+.
    pub max_depth: u32,
    /// Maximum total node count across all concepts in the group. Same
    /// cap as `max_depth`: `(0, 65535]` in slice 1 so 16-bit indices
    /// always suffice. The cap is the only verifier exploitation in
    /// B.1; arena sizing follows in B.4+.
    pub max_nodes: u32,
    /// The concepts that compose the group, in declaration order. Each
    /// is a `Concept` with `variants:` (record-shape concepts inside a
    /// group are refused by the verifier — a group exists to carry sum
    /// types). Field references inside variant payloads may resolve to
    /// other concepts in the SAME group (intra-group recursion); cross-
    /// group references are refused in B.1.
    pub concepts: Vec<Concept>,
}

/// Phase B slice 1: iterate every `Concept` declared in a program,
/// whether at top level (`Item::Concept`) or nested inside a
/// `ConceptGroup`. Consumers that need the FULL concept namespace
/// (verifier name resolution, codegen, optimizer field-range collection,
/// native concept lookup, wasm concept lookup, …) should use this
/// helper rather than filtering for `Item::Concept` alone — otherwise
/// concepts inside a group are silently invisible.
///
/// Returns concepts in source order: top-level concepts and group
/// concepts interleaved in the order they appear in `items`, with each
/// group's inner concepts following the group declaration itself.
pub fn iter_all_concepts(items: &[Item]) -> impl Iterator<Item = &Concept> {
    items.iter().flat_map(|it| -> Box<dyn Iterator<Item = &Concept>> {
        match it {
            Item::Concept(c) => Box::new(std::iter::once(c)),
            Item::ConceptGroup(g) => Box::new(g.concepts.iter()),
            _ => Box::new(std::iter::empty()),
        }
    })
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
    /// Mutable state fields that persist across requests within a single
    /// process lifetime. Each field has a type, an initial value (literal),
    /// and lives in a dedicated rbp slot allocated at server startup. The
    /// handler reads state fields via `state.field`; the `after:` block
    /// mutates them via `set field = expr`.
    ///
    /// For `concurrency: sequential`: straightforward — one process, one
    /// set of slots. Mutations in the `after:` block are visible to the
    /// next accept iteration.
    ///
    /// For `concurrency: forked`: the child inherits the parent's state
    /// via fork's COW. Mutations in the child's `after:` block do NOT
    /// propagate back to the parent — documented limitation (POC-level;
    /// shared-memory state is a future design point).
    ///
    /// Number-only in slice 1. Text state fields need (ptr, len, buffer)
    /// management and are a follow-up.
    pub state_fields: Vec<StateField>,
    /// Post-response mutation block. Runs AFTER the response is written,
    /// AFTER the log blocks. Each entry mutates one state field:
    /// `set <field_name> = <expr>` where <expr> can reference `state.*`,
    /// `req.*`, `resp.*`. Empty Vec means no mutation (pure service).
    pub after_sets: Vec<StateSet>,
}

/// A mutable state field declared in a service's `state:` block.
/// Number-only in the first slice (text state needs buffer management).
#[derive(Debug, Clone)]
pub struct StateField {
    pub name: String,
    pub ty: Type,
    pub initial_value: i64,
}

/// A mutation in a service's `after:` block.
/// `set <field_name> = <expr>` where expr runs in the post-response
/// scope (can reference state.*, req.*, resp.*).
#[derive(Debug, Clone)]
pub struct StateSet {
    pub field_name: String,
    pub value: Expr,
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
    /// Phase A slice 1: optional `variants:` block. Mutually exclusive
    /// with `fields:` — a concept is either a record (fields, current
    /// behavior, variants empty) OR a sum type (variants, fields empty).
    /// Verifier rejects both non-empty or both empty.
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    pub range: Option<(i64, i64)>,
}

/// Phase A slice 1: a variant in a sum-type concept.
///
/// Each variant has a name and zero or more typed fields, like a
/// mini-record. Pattern `variants: VarA of (x: number, y: text) |
/// VarB of (z: bool) | VarC` (the last form is a no-field variant).
///
/// Field bindings inside a variant are scoped to that variant — the
/// `match e: VarA(x, y) => ...` destructure (to be added in slice A.3)
/// binds `x` and `y` to the values stored in the matched variant's
/// payload.
#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,
    pub fields: Vec<Field>,
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
    /// `starts_with(<haystack>, <needle>)` — does the haystack text
    /// begin with the needle's bytes? Returns `bool`. Both args must
    /// be text-typed; the verifier rejects number args.
    ///
    /// Edge cases (canonical):
    ///   - empty needle → always true (every text starts with the
    ///     empty prefix — the standard convention)
    ///   - needle longer than haystack → false (no slot to match)
    ///   - byte-exact match required (no encoding awareness, no case
    ///     folding — Verbose stays at the byte level on purpose)
    ///
    /// Composes with the BoundText shape: needle can be a literal,
    /// a text input field, `read(<resource>)`, or a Phase-2I text
    /// let. Same family as `field == read(<resource>)` (slice
    /// "text equality with bound RHS" 2026-04-28).
    StartsWith(Box<Expr>, Box<Expr>),
    /// `length(<text_expr>)` — byte count of a text expression as a
    /// number. Inner must be text-typed; output is `number`.
    ///
    /// Counts bytes, not characters — Verbose stays at the byte level
    /// (no UTF-8 awareness). For ASCII this is the obvious answer; for
    /// multibyte UTF-8 the result is the storage size, not the visual
    /// length. Documented and intentional.
    ///
    /// Composes with the existing BoundText shape: argv text fields
    /// use the inline `emit_strlen` scan; bound text (read / fetch /
    /// Phase-2I let) loads the length directly from the registered
    /// len_slot — zero scan cost for runtime-loaded data whose length
    /// the prologue already knows.
    Length(Box<Expr>),
    /// `contains(<haystack>, <needle>)` — does the haystack text
    /// contain the needle's bytes anywhere as a contiguous substring?
    /// Returns `bool`. Both args must be text-typed.
    ///
    /// Edge cases (canonical):
    ///   - empty needle → always true (every text contains the empty
    ///     string as a substring)
    ///   - needle longer than haystack → false (no slot to match)
    ///   - byte-exact match required (no encoding awareness, no case
    ///     folding)
    ///
    /// Native algorithm: naive O(N*M) substring search using `rep cmpsb`
    /// for each candidate offset. Bounded by the verifier's `max:`
    /// declarations on the resource, so worst-case work is statically
    /// known. Composes with the existing BoundText shape: needle and
    /// haystack can each be a literal, a text input field, or BoundText
    /// (`read(<resource>)`, `fetch(<connection>, ...)`, Phase-2I let).
    Contains(Box<Expr>, Box<Expr>),
    /// `abs(<number_expr>)` — absolute value. Inner must be number-typed;
    /// output is `number`. Useful where the natural operator-style
    /// `a - b < window` is bug-prone (future timestamps make the
    /// difference negative and silently pass the filter); `abs(a - b)
    /// < window` expresses the symmetric time-window correctly.
    ///
    /// Native: 5-byte canonical inline (`cqo; xor rax, rdx; sub rax, rdx`)
    /// — sign-extends rax into rdx (which becomes 0 for non-negative,
    /// -1 for negative), then xor + sub flips the bits and adds 1 only
    /// when negative. No branch.
    Abs(Box<Expr>),
    /// `ends_with(<haystack>, <needle>)` — symmetric of `starts_with`.
    /// Returns `bool`: true iff `haystack`'s LAST `length(needle)` bytes
    /// match `needle` byte-for-byte. Empty needle is always true (every
    /// text ends with the empty suffix). Needle longer than haystack is
    /// false. Byte-exact, no encoding awareness.
    ///
    /// Native algorithm: load haystack and needle (ptr, len), check
    /// haystack_len >= needle_len, compute haystack_tail_ptr =
    /// haystack_ptr + (haystack_len - needle_len), then `repe cmpsb` on
    /// needle_len bytes.
    EndsWith(Box<Expr>, Box<Expr>),
    /// `min(<a>, <b>)` — binary scalar minimum, returns Number.
    /// Distinct from the existing fold-style `min(coll, var => expr)`
    /// which reduces a collection. The parser disambiguates by the
    /// presence of `=>` after the second argument: with lambda → fold,
    /// without → binary scalar. Both args must be number-typed.
    /// Native: branch-free `cmp + cmovg` (3 instructions).
    Min(Box<Expr>, Box<Expr>),
    /// `max(<a>, <b>)` — binary scalar maximum, returns Number. Same
    /// disambiguation rule and shape as Min. Native: `cmp + cmovl`.
    Max(Box<Expr>, Box<Expr>),
    /// `substring(<text_expr>, <start>, <end>)` — slice a sub-range of
    /// the input text by byte offset. Returns `text`. Semantics are
    /// half-open: bytes [start, end), so `substring(s, 0, length(s))`
    /// reproduces `s` and `substring(s, k, k)` is the empty text.
    ///
    /// Bounds are enforced mechanically at runtime, fail-closed:
    ///   - `end > length(text_expr)` → sys_exit(1)
    ///   - `start > end` → sys_exit(1)
    /// Negative start/end fall under `start > end` after the unsigned
    /// comparison reinterprets them as huge, so they abort too.
    ///
    /// No allocation: the result is `(text_ptr + start, end - start)`
    /// — a pointer slice into the same buffer as the input. The
    /// auditor reads "substring" and knows no copy happened, no heap
    /// touched, no scratch allocated. Lifetime of the result equals
    /// the lifetime of the input buffer (which lives at least until
    /// the enclosing record loop's rsp frame is freed).
    ///
    /// This is the missing tokenizer primitive — to extract a literal
    /// out of a source buffer you need to know start/end and slice it.
    /// Self-hosting target depends on this.
    Substring(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `byte_at(<text_expr>, <index>)` — read the byte at the given
    /// offset of the text expression, returning a Number in 0..256.
    /// Index is zero-based.
    ///
    /// Bounds enforced at runtime, fail-closed: `index >= length(text)`
    /// aborts with sys_exit(1). Negative index falls under the same
    /// check via unsigned reinterpretation (becomes a huge value,
    /// always > length).
    ///
    /// This is the second tokenizer primitive (along with substring):
    /// a tokenizer scans `byte_at(source, i)` byte-by-byte to find
    /// token boundaries, then uses `substring(source, start, end)` to
    /// extract the lexeme. Self-hosting depends on both.
    ///
    /// Cheap: no allocation, no scratch — the emit is a bounded
    /// `cmp + jae .abort ; movzx eax, byte [text_ptr + index]`.
    ByteAt(Box<Expr>, Box<Expr>),
    /// `fold_bytes(<text>, <init>, acc, byte, idx => <body>)`
    ///
    /// Iterate over the bytes of `text`, threading an accumulator
    /// `acc` (initialized to `<init>`) through each iteration. For
    /// each byte at index `idx`, evaluate `<body>` with three names
    /// in scope (all Number-typed):
    ///   - `acc` — the running accumulator (Number)
    ///   - `byte` — the current byte value (Number 0..256)
    ///   - `idx` — the current byte position (Number, 0-based)
    /// The body's result becomes the next accumulator value. After
    /// the last byte, the final accumulator value is the fold result.
    ///
    /// Output type: Number. Body must return Number.
    ///
    /// This is the byte-level iteration primitive that unlocks
    /// variable-length tokenizing: find first digit position, count
    /// digits, find a separator, compute a simple checksum, etc.
    /// Without it, scans require N if/else branches for a max-N-byte
    /// input — verbose and bound at compile time. fold_bytes scales
    /// to any text length.
    ///
    /// Layout in the variant: (text, init, acc_name, byte_name,
    /// idx_name, body). Six fields, mirroring the existing Fold's
    /// 5-field shape with one extra binding for the position.
    FoldBytes(Box<Expr>, Box<Expr>, String, String, String, Box<Expr>),
    /// Phase A slice 2 — variant construction. Syntax:
    ///   `ConceptName::VariantName { field: expr, ... }`         (with payload)
    ///   `ConceptName::VariantName`                              (no-payload variant)
    ///
    /// Constructs a value of the named concept tagged with the given
    /// variant. The verifier cross-checks:
    ///   - The concept exists and is a sum-type concept (non-empty `variants`).
    ///   - The variant exists on that concept.
    ///   - The field assignments match the variant's declared payload exactly
    ///     (same field names + each expression typechecks against the
    ///     declared field type).
    ///   - A no-payload variant carries an empty field list; verifier
    ///     rejects extraneous fields with a clear breadcrumb.
    ///
    /// Layout: (concept_name, variant_name, field_assignments).
    /// The same shape as `Record(name, fields)` but with the extra
    /// variant qualifier — the auditor reads the qualified form and
    /// knows which concept owns the variant.
    ///
    /// Native emit deferred to slice A.4+ (tagged union layout +
    /// dispatch). Interpreter handles construction today.
    VariantConstruct(String, String, Vec<(String, Expr)>),
    /// Phase A slice 3 — pattern match across a sum-type's variants.
    /// Syntax (block form):
    ///
    /// ```verbose
    /// match e:
    ///   VarA(x, _, z) => body_a
    ///   VarB(n)       => body_b
    ///   VarC          => body_c       -- no-payload variant
    /// ```
    ///
    /// Layout: (scrutinee, arms). Each arm pins one variant of the
    /// scrutinee's concept and provides a (positional) destructuring
    /// of the variant's payload — `None` is the wildcard `_`,
    /// `Some(name)` binds the field's value to `name` in the arm's
    /// body. Binders are positional (one per declared payload field,
    /// in declaration order); the body is scoped to its own arm so
    /// binders can be reused across arms without collision.
    ///
    /// Verifier cross-checks (slice 3):
    ///   - scrutinee type is `Type::Named(C)` for a sum-type concept C
    ///     (verifier rejects if C is a record concept or unknown)
    ///   - every arm's variant name exists on C
    ///   - every arm's binder count == that variant's payload arity
    ///   - the set of arm variant names equals C's variant set exactly
    ///     (no missing variant, no duplicate, no unknown extra)
    ///   - each arm body typechecks against the rule's output type
    ///     (with binders introduced into the lambda-scope set so
    ///     purity's `reads:` proof doesn't trip on them)
    ///
    /// Generalization of the existing `match_result` (which stays
    /// grandfathered for `Result(T, E)` consumption). Native emit
    /// deferred to slice A.4+ (tag dispatch + payload load); today the
    /// interpreter handles dispatch and binding.
    MatchVariant(Box<Expr>, Vec<MatchArm>),
}

/// Phase A slice 3 — one arm of a pattern match. Each arm pins a
/// variant by name and provides positional destructuring of its
/// payload. A `None` binder is the wildcard `_` (skip the field);
/// `Some(name)` binds the field's value to `name` in `body`'s scope.
/// `binders` length MUST match the variant's payload arity; the
/// verifier rejects mismatches with a clear breadcrumb.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub variant_name: String,
    pub binders: Vec<Option<String>>,
    pub body: Expr,
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
    pub structural: Option<String>,
    pub decreasing: Option<String>,
    pub increasing: Option<String>,
}
