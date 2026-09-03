# agentctl

`agentctl` is a [Model Context Protocol](https://modelcontextprotocol.io) server that gives an AI agent
reliable, structured control of GUI/desktop applications. Instead of guessing at pixels, it grounds the agent
in the operating system's **accessibility tree** — roles, names, and bounds — so actions target real UI
elements rather than screenshot coordinates. It is pure capability: there is no embedded LLM and no internal
planning loop; any MCP-capable agent (Gemini, Claude, Cursor, a local model over an MCP bridge) validates a
call, the server gates it through policy, executes it against the OS, and returns a structured result.
Cross-platform is planned; the MVP is **macOS first**.

---

## ⚠️ Safety

This server can drive a real machine on behalf of a **remote, untrusted agent**. Treat the driving agent as a
potentially adversarial or prompt-injected third party — the safety of the system does **not** rely on the
model's judgment. The policy layer is the security boundary. Read this section before running it.

- **Untrusted-agent model.** The agent never sees consent material and cannot answer its own prompts. Even a
  fully compromised agent is confined to enabled categories, non-dangerous tools, and allowlisted targets.
- **Tiered tools.** Every tool is `read`, `standard`, or `dangerous`. `read`/`standard` tools are enabled only
  within explicitly enabled categories (default: `vision`, `input`, `window`).
- **Dangerous tools are off by default.** They require explicit per-tool opt-in (`policy.enable = [...]`);
  enabling a whole category never enables its dangerous tools.
- **Per-call consent.** In interactive mode, high-impact actions surface a `NeedConsent` prompt to the human
  out-of-band. In autonomous mode there is no channel, so consent-required calls are denied.
- **Full audit log.** Every call is written before and after execution to an append-only JSONL log. Secrets
  (secure text fields, credential values, cookies, API keys) are redacted from both results and the audit log.
- **Kill switch.** Creating the file `~/.agentctl/STOP` aborts in-flight calls; it is polled before every
  policy decision and inside every long-running call.
- **Run it from a different terminal than it controls.** If the agent drives GUI apps, run the server from a
  separate terminal/app — otherwise it may type into its own console.

The security posture is mapped to OWASP web and LLM-application guidance (deny-by-default access control,
input validation, command-injection and SSRF guards, secret redaction, security logging). See
[`docs/architecture.md`](docs/architecture.md) §8 for the concern → control → location table.

---

## Status

**Phase 0 complete** — the walking skeleton is built and passing tests:

- MCP stdio protocol (newline-delimited JSON-RPC 2.0, protocol version `2025-11-25`): `initialize`,
  `tools/list` (filtered to enabled categories), `tools/call`.
- The dispatch pipeline: schema validation → policy gate (category → tier → consent → denial budget) →
  engine → result envelope → secret redaction → audit.
- `mcp-policy` skeleton: category/tier gates, consent stub, denial budget, kill-switch poll, audit sink.
- A `ping`/`echo` system tool that exercises the full path end-to-end.

Perception (`get_ui_tree` + screen capture) and input (semantic + coordinate) are next. No GUI, capture,
input, terminal, filesystem, network, or credential capability is built yet — do not expect those tools to
exist until their phase lands. See [`docs/implementation-plan.md`](docs/implementation-plan.md) for the phase
plan and the validation gate.

---

## Build & Run

```sh
cargo build          # build the workspace
cargo test           # run the test suite
```

Run the server over stdio and complete a handshake by piping newline-delimited JSON-RPC into it:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ping","arguments":{}}}' \
  | ./target/debug/agentctl serve
```

Other subcommands:

```sh
agentctl doctor         # report OS permission state (Accessibility / Screen Recording on macOS)
agentctl config print   # show the effective config, secrets redacted
```

Configuration is loaded from `config.toml` with `AGENTCTL_*` environment overrides on top of secure built-in
defaults; bad config fails startup with a clear message. See [`docs/planning.md`](docs/planning.md) §9.

---

## Architecture at a glance

A Cargo workspace: a protocol/policy core, one engine crate per capability category, and the `agentctl`
composition-root binary. Dependencies point inward — engines depend only on `mcp-types` and their OS crates,
never on the core, so the policy gate cannot be bypassed.

| Crate | Role |
|---|---|
| `mcp-types` | Shared vocabulary: `Envelope`, `ToolError`/`ErrorCode`, `ToolDescriptor`, `Tier`, `Category`, the `ToolModule` trait, `CallCtx`. |
| `mcp-core` | Protocol: JSON-RPC framing, `initialize`/`tools/list`/`tools/call`, the dispatch pipeline and registry. |
| `mcp-policy` | The security kernel: category/tier gates, allowlists, destructive-input gate, consent, denial budget, kill switch, audit, redaction. |
| `agentctl` | The binary: CLI (`serve`/`doctor`/`config print`), config load, and wiring only the enabled engines into the registry. |

Engine crates (`mcp-a11y`, `mcp-vision`, `mcp-input`, `mcp-window`, `mcp-browser`, and the deferred
terminal/filesystem/network/system/credential engines) land in later phases — see
[`docs/architecture.md`](docs/architecture.md) §3 for the full crate map and file layout.

---

## Docs

- [`docs/planning.md`](docs/planning.md) — design source of truth: ADRs, the full tool catalog, protocol
  surface, policy/consent/audit design, config.
- [`docs/architecture.md`](docs/architecture.md) — structural source of truth: crate decomposition, layering
  rules, concurrency model, and the OWASP-mapped security architecture (§8).
- [`docs/implementation-plan.md`](docs/implementation-plan.md) — executable work breakdown: phases,
  cross-cutting workstreams, the macOS-first MVP, and the validation gate.

## License

Apache-2.0.
