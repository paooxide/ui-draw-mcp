# TEST PLAN — `agentctl-mcp`

Companion to [`architecture.md`](./architecture.md), [`planning.md`](./planning.md), and
[`implementation-plan.md`](./implementation-plan.md). This doc is the concrete test catalogue, with emphasis
(per request) on **concurrency / race conditions**, **partial-failure states**, and **boundary conditions**
(nil/null, empty sets, max payloads), plus the security tests that back the OWASP mapping in `architecture.md`
§8. Test IDs are stable and referenced in code (`#[test]` names) and CI gates.

Scope note (D12): concrete examples target the MVP crates (`mcp-core`, `mcp-policy`, `mcp-a11y`, `mcp-vision`,
`mcp-input`, `mcp-window`, `mcp-browser`); the patterns generalize to deferred engines when built.

---

## 0. Test taxonomy & tooling

| Layer | What | Tool | Runs where |
|---|---|---|---|
| Unit | OS-independent logic (flatten, validate, map, redact) | `cargo test` | any OS |
| Golden | text/flatten outputs vs fixtures | `insta` | any OS (fixtures per OS) |
| Mock/fake | full dispatch loop w/ fake OS backends | `test-support` fakes | any OS |
| Property | invariants (gate-before-engine, redaction-always) | `proptest` | any OS |
| Concurrency | races, ordering, deadlock, cancellation | `loom`, `tokio::test(flavor=multi_thread)`, `turmoil` (net, later) | any OS |
| Policy | allow/deny/consent/budget/redact | `test-support` policy kit | any OS |
| Conformance | MCP handshake/list/call/error | in-proc client | any OS |
| Live-OS | real APIs | `--features live-os` | that OS only (some manual) |
| Fuzz | parser + schema validator | `cargo-fuzz` | CI (P10) |
| Red-team | adversarial security inputs | dedicated suite | CI (P10) |

**Coverage gates (CI-enforced):** every tool has ≥1 allow-path and ≥1 deny-path test; every secret-bearing
field has a redaction test; every engine has an error-mapping test (no `INTERNAL` for expected failures);
every shared-state type has ≥1 concurrency test.

---

## 1. Property / invariant tests (the security spine)

| ID | Invariant | Method |
|---|---|---|
| INV-1 | No engine call happens without a prior `Allow` from `policy.gate` | `proptest` over random (tool,args); instrument a call-counter; assert engine-count ≤ allow-count |
| INV-2 | Every terminal path writes exactly one pre + one post audit record (deny writes pre+decision) | fake audit sink, assert record pairs |
| INV-3 | `redact` runs on 100% of outbound `data` and audit payloads | tag secret fields in fakes; assert never present raw downstream |
| INV-4 | `tools/list` never advertises a disabled category | random category configs → assert filter |
| INV-5 | Fail-closed: unknown tool / disabled category / un-opted dangerous → deny, engine never invoked | table + proptest |
| INV-6 | Cancelled/timed-out engine call never yields `ok:true` | inject slow/cancelling fakes |

---

## 2. Concurrency & race-condition tests

Design intent from `architecture.md` §7. These are the highest-value tests for a multi-tasked server holding
snapshot/session state.

### 2.1 SnapshotArena (a11y refs)
| ID | Scenario | Expected |
|---|---|---|
| CC-ARENA-1 | Thread A takes a new snapshot (write) while thread B resolves a ref from the previous `snapshot_id` | B gets `STALE_REF` deterministically; no panic, no wrong-element resolution |
| CC-ARENA-2 | Two concurrent `get_ui_tree` calls (two new snapshots) | both succeed; the later `snapshot_id` wins; earlier refs invalidated; arena not corrupted |
| CC-ARENA-3 | Ref read races with eviction of that exact `snapshot_id` | either valid resolve or `STALE_REF` — never a dangling/native-handle use-after-free |
| CC-ARENA-4 | `loom` model: read lock during a write-swap | no deadlock; no torn read of the map |
| CC-ARENA-5 | 1000 refs numbered under concurrent snapshots | `@eN` numbering monotonic and unique within each `snapshot_id` |

### 2.2 Session registries (browser targets; pty later)
| ID | Scenario | Expected |
|---|---|---|
| CC-SESS-1 | `browser_close`/target-crash concurrent with `browser_act` on same `target_id` | act returns `NOT_FOUND`/`ACTION_FAILED`; never operates a freed target |
| CC-SESS-2 | Concurrent `browser_tabs open` ×N | N distinct ids; no id reuse/collision |
| CC-SESS-3 | Use of a `target_id` after its browser disconnects | `NOT_FOUND`, cleanup ran (no leaked CDP connection) |
| CC-SESS-4 | Two calls mutate the same session (navigate + act) | serialized per-session; no interleaved CDP command corruption |

### 2.3 Policy, budget, kill switch, audit
| ID | Scenario | Expected |
|---|---|---|
| CC-POL-1 | N concurrent denied calls vs `max_denials` | budget counted atomically; abort triggers exactly once at threshold (no over/under count, no TOCTOU) |
| CC-POL-2 | Kill switch trips while M calls are mid-flight | all M observe cancellation; each returns abort state; no call completes as success post-trip |
| CC-POL-3 | Concurrent audit writes from many calls | single-writer serializes; JSONL lines intact, no interleaving; total order preserved |
| CC-POL-4 | Consent prompt pending for call A while call B arrives needing consent | prompts queued/serialized; B cannot consume A's answer; no double-consent |
| CC-POL-5 | Category disabled mid-session (config reload / list_changed) while a call to it is in flight | in-flight call completes or is denied atomically at the gate; no half-enabled window |

### 2.4 Cancellation safety (Drop/cleanup)
| ID | Scenario | Expected |
|---|---|---|
| CC-CANCEL-1 | Drop an `exec`/subprocess future mid-run (later) | child killed, no zombie, no leaked fd |
| CC-CANCEL-2 | Drop a `get_ui_tree` future mid-walk | AX observer/handles released; arena unchanged |
| CC-CANCEL-3 | Drop a `capture_screen` future mid-encode | buffers freed; no partial image emitted |
| CC-CANCEL-4 | Timeout fires exactly at engine completion (race) | exactly one of {result, TIMEOUT}; never both; no double audit-post |

### 2.5 Lock ordering / deadlock
| ID | Scenario | Expected |
|---|---|---|
| CC-LOCK-1 | Op needing arena+registry from two tasks in opposite apparent order | helper enforces arena-before-registry; `loom` finds no deadlock |

---

## 3. Partial-failure-state tests

Every capability that touches the OS or a subprocess can fail *between* steps. The rule: **partial ≠ success**,
and no side effect leaks past an error.

| ID | Scenario | Expected |
|---|---|---|
| PF-CORE-1 | `policy.gate` = Allow, engine panics/returns Err | mapped `ErrorCode`; post-audit written; no `ok:true`; no state mutation observable |
| PF-CORE-2 | Pre-audit write succeeds, engine dies | post-audit marks the call incomplete/failed; never silently dropped |
| PF-A11Y-1 | Snapshot walk succeeds for most nodes, one subtree read fails | tree returned with the failed node marked; call succeeds partially **and says so**, or fails cleanly per policy — never silently drops nodes as if complete |
| PF-A11Y-2 | Element vanishes between snapshot and `get_element` | `STALE_REF`/`NOT_FOUND`, not a stale value |
| PF-VIS-1 | Capture grabs frame but PNG encode fails | `ACTION_FAILED`; no truncated image content block emitted |
| PF-INPUT-1 | `keyboard_type` types 3 of 10 chars then focus lost | reports how much was applied + error; never claims full success |
| PF-INPUT-2 | `drag_drop` press+move succeed, release fails | pointer state reset (no stuck button); `ACTION_FAILED` |
| PF-WIN-1 | `menu_open` opens menu, `menu_invoke` target item missing | menu left in a known state (or closed); `NOT_FOUND` |
| PF-WAIT-1 | `wait_for` times out vs. condition met at the same instant | deterministic single outcome; no double-fire |
| PF-BROWSER-1 | Navigation starts, tab crashes mid-load | `ACTION_FAILED`; target marked dead; subsequent calls `NOT_FOUND` |
| PF-BROWSER-2 | `browser_act` finds node, node detaches before click | `STALE_REF`/`ACTION_FAILED`; no click on a stale node |
| PF-BROWSER-3 | `browser_eval` script throws / times out | error surfaced; no partial result claimed as success |
| PF-SUB-1 (deferred) | `exec` spawns then times out | child killed, partial stdout returned flagged `stdout_truncated`, `TIMEOUT` code |
| PF-FS-1 (deferred) | Atomic write: temp written, rename fails | temp file cleaned up; original intact; `ACTION_FAILED` |
| PF-FS-2 (deferred) | Disk fills mid-write | error; no half-file left at the target path |
| PF-NET-1 (deferred) | DNS resolves, TCP connect resets | `ACTION_FAILED`; connection fully closed |
| PF-REDACT-1 | Secret value exceeds redaction buffer/stream boundary | still fully redacted (no head/tail leak across the chunk boundary) |

---

## 4. Boundary-condition tests

### 4.1 Nil / null / absent
| ID | Input | Expected |
|---|---|---|
| BND-NIL-1 | Optional arg omitted vs. explicit JSON `null` | both treated as absent identically; documented; no `unwrap` panic |
| BND-NIL-2 | Required arg missing | `INVALID_ARGS` with the field named; engine never entered |
| BND-NIL-3 | Empty string where a value is expected (`keyboard_type text:""`) | no-op success or `INVALID_ARGS` per tool spec — defined, not accidental |
| BND-NIL-4 | `null` inside a nested object (`region:{x:null}`) | schema/engine rejects with a clear message |
| BND-NIL-5 | Unicode/RTL/emoji/combining marks/NUL in `keyboard_type` text | typed faithfully or rejected if NUL; never truncated mid-grapheme; no injection via control chars |

### 4.2 Empty sets
| ID | Input | Expected |
|---|---|---|
| BND-EMPTY-1 | `get_ui_tree` of an app with zero interactive elements | valid `Flattened` with header + empty element index; not an error |
| BND-EMPTY-2 | `list_windows` when the app has no windows | `[]`, `ok:true` |
| BND-EMPTY-3 | `list_displays` returns exactly one / (hypothetically) zero displays | handled; capture with an out-of-range display index → `INVALID_ARGS` |
| BND-EMPTY-4 | `browser_query` matches nothing | empty match list, `ok:true` (absence ≠ error) |
| BND-EMPTY-5 | `clipboard_read` on empty clipboard | empty/`{data:null}` success, not a crash |
| BND-EMPTY-6 | `tools/list` with `categories = []` | valid empty tool list; server still handshakes |
| BND-EMPTY-7 | `menu_invoke path:[]` | `INVALID_ARGS` (path must be non-empty) |

### 4.3 Max payloads / large inputs
| ID | Input | Expected |
|---|---|---|
| BND-MAX-1 | a11y tree exceeding `max_tree_chars` | auto skeleton; if still over, hard-truncate with `[HARD TRUNCATED N]` marker — never silent, never unbounded |
| BND-MAX-2 | Screenshot larger than `max_image_bytes` | downscaled/capped; size asserted ≤ cap |
| BND-MAX-3 | Extremely deep a11y tree | respects `max_depth`; no stack overflow (iterative walk) |
| BND-MAX-4 | Huge tool-args JSON (multi-MB) | rejected over a size limit before parse work explodes; `INVALID_ARGS` |
| BND-MAX-5 | Very long file path / >255-byte component (deferred fs) | OS error mapped cleanly, no panic |
| BND-MAX-6 | Thousands of windows/tabs | listing bounded/paged or capped with a documented marker |
| BND-MAX-7 | `browser_eval` returning a huge/circular object | serialization bounded; circular → error, not hang |
| BND-MAX-8 | Max `@eN` (e.g. `@e999999999999`) or malformed (`@e`, `@eabc`, `e3`) | `INVALID_ARGS`/`STALE_REF`; regex `^@e\d+$` enforced in engine |
| BND-MAX-9 | PTY output flooding past `max_pty_buffer` (deferred) | tail-kept with marker; memory bounded |

### 4.4 Numeric / enum / range edges
| ID | Input | Expected |
|---|---|---|
| BND-NUM-1 | `wait_for timeout_ms` = 0, negative, and > max (30000) | 0/neg rejected or floored per spec; >max clamped to 30000 (documented) |
| BND-NUM-2 | `scroll amount` = 0 and negative | 0 → no-op; negative → `INVALID_ARGS` or direction-flip per spec (defined) |
| BND-NUM-3 | Coordinate off-screen / negative (`mouse_action x:-5`) | clamped to allowed-window bounds or `INVALID_ARGS`; never a blind click at a wild coordinate |
| BND-NUM-4 | `fs_read offset` beyond EOF / negative `length` (deferred) | `eof:true` empty / `INVALID_ARGS` |
| BND-NUM-5 | Unknown enum value (`ui_action action:"frobnicate"`) | `INVALID_ARGS`; enumerated in schema |
| BND-NUM-6 | Integer overflow in size/offset args | checked arithmetic; `INVALID_ARGS`, no wrap |

---

## 5. Protocol & malformed-input tests (fuzz seeds)

| ID | Input | Expected |
|---|---|---|
| PROTO-1 | Truncated / non-JSON / wrong-framing message | JSON-RPC parse error; connection stays alive; no panic |
| PROTO-2 | Valid JSON, invalid JSON-RPC (missing `method`/`id`) | proper JSON-RPC error object |
| PROTO-3 | `tools/call` for unknown tool | `INVALID_ARGS`/method-not-found; audited |
| PROTO-4 | Duplicate / reused request `id` | handled per spec; no state confusion |
| PROTO-5 | Batch request (if unsupported) | explicit rejection, documented |
| PROTO-6 | Deeply nested / huge JSON (billion-laughs style) | depth/size limits reject before exhaustion |
| PROTO-7 | Invalid UTF-8 in a string field | rejected cleanly |
| FUZZ-1 | `cargo-fuzz` on the framing+jsonrpc parser | no panic/UB/OOM over the corpus |
| FUZZ-2 | `cargo-fuzz` on the tool-args schema validator | no panic; rejects or accepts within schema |

---

## 6. Security / OWASP-backed tests

Each maps to `architecture.md` §8. These are pass/deny assertions, not observational.

| ID | Guidance | Test |
|---|---|---|
| SEC-ACL-1 | A01 / LLM08 | Call into a disabled category → `POLICY_DENIED`, engine never invoked |
| SEC-ACL-2 | A01 | Dangerous tool without `policy.enable` → denied even in an enabled category |
| SEC-INJ-1 | A03 | `exec`/pty text with `rm -rf`, `; sudo`, `$(…)`, backticks, `| sh` → destructive gate denies/consent-gates |
| SEC-INJ-2 | A03 | `exec` cannot run a shell string (argv-only) — attempt to smuggle `sh -c "…"` still argv, metachars inert |
| SEC-PATH-1 | A01/A03 | `fs_*` (deferred) path `../../etc/…`, symlink to outside `fs_roots`, NUL byte → denied |
| SEC-SSRF-1 | A10 | `http_client` (deferred) to `127.0.0.1`, `169.254.169.254`, `::1` → blocked unless opted in |
| SEC-BROWSER-1 | LLM01/A03 | `browser_eval` off by default; enabled + origin not in `allowed_origins` → denied |
| SEC-SECRET-1 | A02 / LLM06 | Secure a11y field value never appears in result or audit (redacted before flatten) |
| SEC-SECRET-2 | LLM06 | `browser_cookies get` value redacted in audit; returned only with consent |
| SEC-SECRET-3 | A09 | Gemini/API keys never in stdout/stderr/audit at any log level |
| SEC-ERR-1 | ASVS V7 | Forced `INTERNAL` returns a generic message to the agent; full detail only in stderr/audit |
| SEC-DEF-1 | A05 | Fresh install with no config → only `vision,input,window` read/standard tools reachable |
| SEC-AGENCY-1 | LLM08 | A "compromised agent" script attempting privileged/dangerous calls is confined to the policy envelope; kill switch aborts within one call |
| SEC-SUPPLY-1 | A06 | `cargo deny` + `cargo audit` green in CI; no yanked/vuln deps |

---

## 7. Functional / golden / conformance (per MVP crate, summary)

| Area | Representative tests |
|---|---|
| `mcp-core` | handshake; `tools/list` reflects config; `tools/call` round-trip; error mapping; PROTO-* |
| `mcp-policy` | allow/deny/consent matrix; budget; redaction; kill switch; INV-* |
| `mcp-a11y` | flatten goldens (editor/terminal/file-manager fixtures); secure redaction; budget→skeleton→truncate; stale ref |
| `mcp-vision` | capture size cap; display index bounds; encode failure path |
| `mcp-input` | semantic action on fake; type Unicode; coordinate clamp; destructive gate; clipboard redaction |
| `mcp-window` | list/control on fake; `menu_invoke` by path; `wait_for` outcomes; empty-window set |
| `mcp-browser` | attach fake CDP; `browser_snapshot` golden; act on node; cookie redaction; origin allowlist |
| Gemini bridge (X-H) | schema-subset lint; `functionCall` round-trip; `thoughtSignature` echo; no `temperature/topP/topK` |

---

## 8. Live-OS (macOS MVP) — manual/gated

| ID | Test | Note |
|---|---|---|
| LIVE-1 | `agentctl doctor` reports Accessibility/Screen-Recording state | needs a granted TCC permission; document manual grant |
| LIVE-2 | `get_ui_tree` of a real target app yields a usable tree | the D12 spike; run before building P2/P3 |
| LIVE-3 | Coordinate click + `menu_invoke` drive a real app to a verifiable state | Milestone A input path |
| LIVE-4 | 🚦 **Validation-gate demo**: Gemini completes a GUI-only task ≥4/5 | the go/no-go for everything beyond the MVP |
| LIVE-5 | Attach to Chrome started with `--remote-debugging-port`; `browser_snapshot` reads a page | post-gate |

CI can't grant TCC/permissions headlessly, so live-OS runs on a self-hosted macOS runner or manually with
results recorded in `planning.md` §12. Everything else (§1–§7) runs in CI against fakes on any OS.

---

## 9. CI gating summary

- PR blocks on: fmt, clippy `-D warnings`, `cargo test --workspace` (fakes), `cargo deny`, `cargo audit`,
  schema-subset lint, and the coverage gates in §0.
- Nightly/optional: `loom` concurrency suite, `cargo-fuzz` corpus run, red-team suite (§6 SEC-*).
- Release (P10) blocks on: fuzz + red-team green, live-OS macOS pass recorded, threat-model reviewed.
