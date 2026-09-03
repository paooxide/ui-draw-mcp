# ARCHITECTURE: `agentctl-mcp`

This doc is the **structural** source of truth: how the product is decomposed, how the files
are laid out, the rules that keep the decomposition honest, the concurrency model, and the security
architecture mapped to OWASP guidance. File trees below mark shipped crates vs. deferred ones.

---

## 1. Design principles

1. **Fail closed / deny by default.** Absence of an explicit allow is a deny. Unknown tool, disabled category,
   un-opted-in dangerous tool, ambiguous consent → `POLICY_DENIED`, never a best-effort execution.
2. **Single policy choke point.** Exactly one code path reaches an engine, and it runs the policy gate first.
   No engine is callable except through `mcp-core::dispatch`. This is architecturally enforced, not a
   convention (§4, §6).
3. **Least privilege, layered.** Category gate → tier gate → allowlist → consent. Each narrows further. Powers
   the agent doesn't need are not advertised (`tools/list` filtered) and not reachable.
4. **Untrusted client assumption.** The driving agent may be adversarial or prompt-injected (directly, or
   indirectly via a hostile web page reached through the browser engine). The server never trusts tool
   arguments, never trusts that the agent "meant well," and treats every call as attacker-controlled input.
5. **Defense in depth.** Schema validation → engine-level semantic validation → policy → OS permission → audit.
   A bypass of one layer is caught by the next.
6. **Separation of concerns + dependency inversion.** `mcp-core` depends on an abstract `ToolModule` trait, not
   on concrete engines. Engines depend only on `mcp-types` and their OS crates. Dependencies point inward.
7. **Deterministic & auditable.** Every call is validated, gated, and written to an append-only audit log
   before and after execution. Secrets are redacted centrally.
8. **Testable by construction.** No hidden global state; all capability handles flow through an injected
   `CallCtx`; every engine has a fake backend so the full pipeline runs on any OS in CI.

---

## 2. Views

### 2.1 System context
```
┌────────────────┐   MCP / JSON-RPC 2.0    ┌──────────────────────┐   native OS APIs   ┌──────────┐
│  AI agent      │  (stdio; http later)    │  agentctl (server)   │  AX / capture /    │  macOS   │
│  (Gemini 3.8,  │ ───────────────────────▶│  policy · engines    │  input · CDP …     │  (later  │
│   Claude, …)   │ ◀───────────────────────│  audit               │ ──────────────────▶│  Win/Lx)│
└────────────────┘   results (redacted)    └──────────┬───────────┘                    └──────────┘
   UNTRUSTED                                           │ append-only
                                                       ▼
                                                 audit/<session>.jsonl
```
Trust boundary is the MCP interface: everything left of it is untrusted. A second, inner boundary exists at the
browser engine: content in a driven web page is untrusted and can attempt indirect prompt injection back
through the agent.

### 2.2 Containers (crates)
See §3. Three layers: **protocol/policy core** (`mcp-core`, `mcp-policy`, `mcp-types`), **engines** (one per
capability category), **composition root** (`agentctl` binary + `test-support`).

### 2.3 Runtime sequence (one `tools/call`)
```
agent → core.transport → core.jsonrpc(parse) → core.protocol(tools/call)
      → core.dispatch:
          1. registry.lookup(tool)                     ── unknown → INVALID_ARGS
          2. schema.validate(args)                     ── bad shape → INVALID_ARGS
          3. policy.gate(tool, args, ctx):
               category → tier → allowlist → destructive → consent → budget
                                                         ── deny → POLICY_DENIED (audited)
          4. audit.pre(record)
          5. engine.call(ctx, args)  [with timeout + killswitch poll]
          6. redact(result)
          7. audit.post(record)
      → envelope → jsonrpc(response) → agent
```

---

## 3. Structure & files

### 3.1 Workspace
```
agentctl/                      # composition root (binary)   [MVP]
crates/
  mcp-types/                   # shared vocabulary            [MVP]
  mcp-core/                    # protocol + dispatch          [MVP]
  mcp-policy/                  # the security kernel          [MVP]
  mcp-a11y/                    # perception (a11y tree)       [MVP]
  mcp-vision/                  # capture                      [MVP]
  mcp-input/                   # keyboard/mouse/clipboard     [MVP]
  mcp-window/                  # windows/menus/apps           [MVP]
  mcp-browser/                 # CDP                          [MVP+ (post-gate)]
  mcp-desktop/                 # session/power/settings       [deferred]
  mcp-pty/ mcp-proc/           # terminal/process             [deferred]
  mcp-fs/ mcp-net/ mcp-sys/    # fs/network/kernel            [deferred]
  mcp-sec/                     # credentials                  [deferred]
  mcp-memory/                  # optional recall              [deferred]
  test-support/                # in-proc client, fakes        [MVP]
```

### 3.2 File-level layout (MVP crates)
```
mcp-types/src/
  lib.rs
  envelope.rs      # Envelope, ToolError, ErrorCode (the API contract)
  descriptor.rs    # ToolDescriptor, Tier, Category, JsonSchema (Gemini-subset)
  module.rs        # ToolModule trait (descriptors() + call())
  context.rs       # CallCtx: arena, session registries, killswitch, config view, audit handle

mcp-core/src/
  lib.rs
  transport/{mod.rs, stdio.rs, framing.rs}   # byte framing (VERIFY revision); stdout is protocol-only
  jsonrpc.rs       # request/response/notification, id handling, batch rejection policy
  protocol.rs      # initialize, tools/list (category-filtered), tools/call
  dispatch.rs      # THE pipeline (§2.3). Only place engines are invoked.
  registry.rs      # aggregates ToolModules; startup name-collision + schema-lint check
  error.rs         # ErrorCode → JSON-RPC error mapping; INTERNAL never leaks detail

mcp-policy/src/
  lib.rs
  gate.rs          # Decision = Allow | Deny{code,reason} | NeedConsent{prompt}; ordered checks
  category.rs      # enabled-category allowlist
  tier.rs          # read/standard/dangerous + per-tool enable list
  allowlist.rs     # apps, fs_roots/deny, http hosts (SSRF), browser origins
  destructive.rs   # typed/clipboard/pty/exec-argv denylist (configurable regex)
  consent.rs       # channel trait (stdin baseline; notification/TUI pluggable)
  budget.rs        # denial budget (atomic)
  killswitch.rs    # STOP-file poll + optional hotkey; cancellation token source
  audit.rs         # append-only JSONL sink, rotation, pre/post records
  redact.rs        # central secret redactor + secret-field registry

mcp-a11y/src/
  lib.rs
  arena.rs         # SnapshotArena: @eN numbering, snapshot_id scoping, eviction → STALE_REF
  tree.rs          # OS-independent node model
  flatten.rs       # Flattened{text,index}; budget → skeleton → hard-truncate marker
  secure.rs        # secure-field detection (redact before flatten)
  backend/{mod.rs, macos.rs}   # AXUIElement [cfg]; windows.rs/linux.rs deferred
  tools.rs         # get_ui_tree, get_element (descriptors + handlers)

mcp-vision/src/    # capture.rs, encode.rs (PNG + size cap/downscale), displays.rs, backend/macos.rs, tools.rs
mcp-input/src/     # semantic.rs, keyboard.rs, mouse.rs (coordinate), clipboard.rs, clamp.rs, backend/macos.rs, tools.rs
mcp-window/src/    # windows.rs, menu.rs, apps.rs, dialogs.rs, wait.rs, backend/macos.rs, tools.rs
mcp-browser/src/   # cdp/{client.rs, conn.rs}, targets.rs (session registry), snapshot.rs, act.rs, cookies.rs, tools.rs

agentctl/src/
  main.rs
  cli.rs           # serve | doctor | config print
  config.rs        # load config.toml + AGENTCTL_* overrides + validation
  wire.rs          # build registry from ENABLED categories only (secure default)
  doctor.rs        # OS permission checks (Accessibility/Screen Recording on macOS)

test-support/src/  # client.rs (in-proc MCP client), fakes/ (per-engine fake backends), fixtures.rs, assert.rs
```

---

## 4. Layering & dependency rules

```
mcp-types  ◀── mcp-policy ◀── mcp-core ◀── agentctl ──▶ engines ──▶ mcp-types (+ OS crates)
                                   ▲                         │
                                   └──── ToolModule trait ───┘   (core calls engines only via the trait)
```

**Enforced rules** (checked in review; some by `cargo-deny`/lints):
- Engines depend on `mcp-types` and their OS crates **only**. An engine importing `mcp-core` or `mcp-policy`
  is a bug (would allow bypassing the gate or circular deps).
- `mcp-policy` depends on `mcp-types` only. It never depends on an engine (it gates by descriptor metadata,
  not by concrete type).
- `mcp-core` orchestrates: depends on `mcp-types` + `mcp-policy` + the `ToolModule` trait; it does **not**
  depend on concrete engines (they're injected at the composition root).
- `agentctl` is the only crate that knows every engine; it wires enabled ones into the registry.
- **No engine is `pub`-callable outside its `tools.rs` handler**, and handlers are registered as trait
  objects the registry owns. `dispatch.rs` is the sole caller. (Test T-SEC-1 asserts no other call site.)

---

## 5. Core abstractions

```rust
// mcp-types
pub enum Tier { Read, Standard, Dangerous }
pub enum Category { Vision, Input, Window, Browser, /* deferred: Terminal, Filesystem, … */ }

pub struct ToolDescriptor {
    pub name: &'static str, pub category: Category, pub tier: Tier,
    pub description: &'static str, pub input_schema: JsonSchema,   // Gemini-subset
}

#[async_trait] pub trait ToolModule: Send + Sync {
    fn descriptors(&self) -> Vec<ToolDescriptor>;
    async fn call(&self, name: &str, args: Value, ctx: &CallCtx) -> Envelope;
}

pub struct CallCtx {                 // injected; the only way an engine reaches shared state
    pub arena: SnapshotArenaHandle,  // a11y refs (Arc<RwLock<…>> internally)
    pub sessions: SessionRegistries, // pty/browser-target handles (deferred/browser)
    pub cancel: CancelToken,         // fires on kill switch / timeout
    pub config: ConfigView,          // read-only snapshot of relevant config
    pub audit: AuditHandle,          // engines may add structured detail (redacted)
}
```
`CallCtx` deliberately does **not** carry the policy handle: policy runs in `dispatch` *before* the engine,
so an engine cannot re-decide or skip it.

---

## 6. Request lifecycle & the choke point

`dispatch::handle_call` is the single security-relevant function. Invariants (property-tested, §T of test-plan):
- Every path that reaches `engine.call` has first returned `Allow` from `policy.gate`.
- Every terminal path (allow-then-execute, deny, error) writes an audit record.
- `redact` runs on every outbound `Envelope.data` and on every audit payload.
- The engine call is wrapped in `tokio::time::timeout` and a `select!` on `ctx.cancel`; a cancelled/timed-out
  engine call yields `TIMEOUT`, the child/handle is killed, and no partial success is reported as success.

---

## 7. Concurrency model

- **Runtime:** `tokio` multi-threaded. Each `tools/call` is a task. Blocking OS calls (AX, capture, subprocess
  waits) run on `spawn_blocking` so they never stall the reactor.
- **Shared mutable state is minimized and owned:**
  - `SnapshotArena`: `Arc<RwLock<Arena>>`; a snapshot write takes the write lock briefly to swap in the new
    map and evict the old; ref reads take the read lock. Refs carry their `snapshot_id`; a read validates the
    id against the current arena → `STALE_REF` if evicted. **No ref handle outlives its snapshot_id check.**
  - **Session registries** (browser targets; pty later): `DashMap`/`Mutex<HashMap>` keyed by id; create/close
    are atomic; every use re-looks-up the id (no cached handle) so a concurrent close yields `NOT_FOUND`, never
    a use-after-free.
  - **Denial budget**: `AtomicUsize`; incremented on each deny; compared under a single fetch to avoid TOCTOU.
  - **Audit sink**: single writer task behind an `mpsc` channel; callers send records, the task serializes
    writes → no interleaving/corruption under concurrency, total order preserved.
  - **Kill switch**: one `CancellationToken`; a poller task trips it; all in-flight tasks observe it via
    `ctx.cancel`. Idempotent.
- **Lock ordering:** the only place two locks are held together is arena+registry; the rule is
  **arena before registry, always**, to prevent deadlock (enforced by a helper that acquires in order).
- **Cancellation safety:** engines must be cancellation-safe: a dropped future must not leave a spawned child
  process, an open AX observer, or a half-written file. Each engine documents its cleanup (Drop guards).
- **Backpressure / DoS bounds:** per-call timeout, output/screenshot/tree size caps, max concurrent
  sessions, denial budget, and (http transport later) request rate limits.

Concurrency is validated with `loom` (lock-based state) and `tokio` cancellation tests (see test-plan §2).

---

## 8. Security architecture (OWASP-mapped)

The server is an **LLM tool/plugin surface** with system reach, so both the classic web guidance and the
OWASP Top 10 for LLM Applications apply. Mapping of concern → guidance → control → location:

| Concern | Guidance | Control in agentctl | Location |
|---|---|---|---|
| Access control | OWASP A01 · ASVS V4 · **LLM08 Excessive Agency**, **LLM07 Insecure Plugin Design** | Deny-by-default; category+tier gates; dangerous opt-in; `tools/list` advertises only enabled categories | `mcp-policy/{gate,category,tier}.rs`, `core/protocol.rs` |
| Input validation | A03 · ASVS V5 | JSON-schema validate → engine semantic validate (ref shape, enums, one-of, path canon, combo regex) | `core/dispatch.rs`, each `tools.rs` |
| Command injection | A03 · ASVS V5.3 | `exec`/subprocess use **argv arrays, never shell strings**; destructive-input denylist on shell-bound text | `mcp-policy/destructive.rs`, engines |
| Path traversal | A01/A03 · ASVS V12 | canonicalize + `fs_roots`/`fs_deny` allowlist + symlink-escape denial (deferred fs; pattern defined now) | `mcp-fs/*` (deferred), `policy/allowlist.rs` |
| SSRF | **A10** · ASVS V12 | `http_client` blocks loopback/link-local/metadata unless opted in (deferred net) | `policy/allowlist.rs`, `mcp-net` |
| JS/code injection via page | A03 · **LLM01 Prompt Injection (indirect)**, **LLM02 Insecure Output Handling** | `browser_eval` dangerous+opt-in; `browser.allowed_origins`; page content treated as untrusted; results redacted | `mcp-browser/*`, `policy/allowlist.rs` |
| Secrets / sensitive data | A02 · ASVS V6/V8 · **LLM06 Sensitive Info Disclosure** | central redactor for secure fields, cookies, credential values, API keys, in results **and** audit | `mcp-policy/redact.rs`, `mcp-a11y/secure.rs` |
| Security logging | **A09** · ASVS V7 | append-only pre/post audit of every call incl. decision; single-writer, redacted; `trace` for full bodies | `mcp-policy/audit.rs`, `mcp-core` |
| Error handling | ASVS V7 · **LLM02** | fail-closed; stable `ErrorCode`; `INTERNAL` never leaks internals to the agent | `core/error.rs`, `mcp-types/envelope.rs` |
| Secure defaults / misconfig | **A05** · ASVS V14 | minimal default categories (`vision,input,window`); dangerous off; stdout is protocol-only; secrets never in stdout | `agentctl/{config,wire}.rs` |
| Supply chain | **A06/A08** · SLSA | pinned deps; `cargo deny` (licenses/advisories) + `cargo audit` in CI; VERIFY each crate | CI (X-B2), `deny.toml` |
| Resource exhaustion / DoS | A04 Insecure Design · **LLM04** | timeouts, size caps, max sessions, denial budget, kill switch | `mcp-policy/*`, `core/dispatch.rs` |
| Human-in-the-loop for high impact | **LLM08 Excessive Agency** | interactive consent on `NeedConsent`; agent cannot self-answer; per-call reason | `mcp-policy/consent.rs` |
| Authentication (remote transport) | A07 · ASVS V2/V13 | stdio has no network surface; http transport (later) requires bearer/mTLS, off by default | `core/transport/*` (P10) |

**Threat model** lives in `docs/threat-model.md` (X-D1) and enumerates abuse cases per category; this table is
the control-to-guidance index into it. Every `dangerous`-tier engine gets a security-review sign-off (X-D8)
before it ships.

Key stance on **prompt injection**: we cannot stop a client agent from being manipulated, so we don't rely on
the agent's judgment for safety. The **policy layer is the security boundary**, not the model. Even a fully
compromised agent is confined to: enabled categories, non-dangerous tiers (unless opted in), allowlisted
apps/roots/origins, with every action audited and the kill switch available. That confinement is the product's
core security property.

---

## 9. Error handling & taxonomy

- One `ErrorCode` enum (`mcp-types`), stable and documented; each engine maps native errors to exactly one code
  (test: every engine has a mapping test, no `INTERNAL` for expected failures).
- Fail closed: on any ambiguity the pipeline denies/aborts rather than proceeds.
- The agent sees `{code, message, suggestion?}`: actionable but never internal (no stack traces, no paths it
  didn't provide, no secret fragments). `INTERNAL` is logged fully to stderr/audit, returned generically.
- Partial success is never reported as success (§6, §7); a cancelled multi-step op returns the furthest safe
  state plus the abort reason.

---

## 10. Configuration & secure defaults

- Precedence: built-in secure defaults ← `config.toml` ← `AGENTCTL_*` env. Validated at load; bad config fails
  startup with a clear message (fail closed).
- Secure defaults: `categories = [vision,input,window]`, `enable = []` (no dangerous tools),
  `mode = interactive`, `clamp_input_to_allowed = true`, `redact_cookies = true`, http transport off.
- `agentctl config print` shows the effective config with secrets redacted.

---

## 11. Observability & audit

- Logs → **stderr only** (`tracing`); stdout is exclusively the MCP protocol. Gemini/API keys redacted in logs.
- Audit → append-only JSONL, single-writer task, rotation; pre-record (tool, args-redacted, tier, decision),
  post-record (ok, error_code, latency, result summary). `--trace` adds full redacted bodies.
- Metrics: per-call latency, result size, capture/flatten cost surfaced in the session summary.

---

## 12. Extensibility: adding a tool or engine

1. Add descriptor(s) to the engine's `tools.rs` (name, category, tier, Gemini-subset schema, description).
2. Implement the handler; validate args in the handler; map errors to `ErrorCode`.
3. Register the module in `agentctl/wire.rs` under its category.
4. If it returns secrets, register the field with `redact.rs`.
5. If dangerous, it's automatically off until named in `policy.enable`; add a threat-model entry + X-D8 review.
6. Add: allow-path test, deny-path test, boundary tests, and a fake backend in `test-support`.

The trait contract is the whole extension surface: no engine touches transport, policy, or audit directly.

---

## 13. Deployment & trust boundaries

- Run `agentctl` as a **normal user**, from a **different app than it controls** (else it types into its own
  console). It needs OS permissions (macOS: Accessibility; Screen Recording for capture) granted to the
  launching app; `agentctl doctor` reports state.
- Never run as root/admin for the MVP; `privilege_run` (deferred) is the only elevation path and routes through
  the OS's own auth prompt.
- stdio deployment has no network attack surface. The optional http transport (P10) is the only remote surface
  and is authenticated + off by default; enabling it is a deliberate, documented blast-radius increase.
```
