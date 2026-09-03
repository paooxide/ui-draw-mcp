# agentctl

`agentctl` is a [Model Context Protocol](https://modelcontextprotocol.io) server that gives an AI agent
reliable, structured control of a real computer: GUI applications, a Chromium browser, and the machine
underneath them.

The difference from a shell-command agent is grounding. Instead of guessing at pixels, `agentctl` reads the
operating system's **accessibility tree** — roles, names, values, bounds — so an action targets a real UI
element rather than a screenshot coordinate. An agent can open an app, find the Save button by name, click
it, and confirm from the tree that the state changed.

It is pure capability. There is no embedded model and no internal planning loop: any MCP-capable client
(Claude Code, Claude Desktop, Cursor, Gemini) decides what to do, and the server validates, gates, executes
and reports. Desktop control is macOS-only today; the browser and the commodity engines are
platform-independent.

---

## ⚠️ Safety

This server can drive a real machine on behalf of a **remote, untrusted agent**. Treat the driving agent as
potentially prompt-injected or adversarial: the safety of the system does not rest on the model's judgment.
The policy layer is the security boundary. Read this before running it.

- **Everything is closed by default.** No filesystem roots, no runnable commands, no reachable hosts, no
  keychain services, until an operator opts in. Only `vision`, `input` and `window` are enabled at all.
- **Tiered tools.** Every tool is `read`, `standard` or `dangerous`. Enabling a category never enables its
  dangerous tools; each of those must additionally be named in `policy.enable`.
- **Per-call consent.** High-impact actions raise a native dialog whose default button is Deny, out of band
  from the agent, which can neither see nor answer it. In autonomous mode there is no channel, so anything
  needing consent is denied rather than allowed.
- **Consent fatigue is a threat.** `max_consent_prompts` caps how often one session may interrupt a human,
  because unlimited dialogs train people to click Allow.
- **Secrets never reach the agent.** There is no plaintext secret read anywhere; secure text fields,
  credential values and cookies are redacted from results and from the audit log.
- **Kill switch.** Creating `~/.agentctl/STOP` aborts in-flight work; it is checked before every policy
  decision.
- **Full audit.** Every call is written before and after execution to an append-only JSONL log.
- **Run it from a different terminal than it controls.** If the agent drives GUI apps, keystrokes go to
  whatever is frontmost. Typed text destined for a shell is screened for destructive commands, and editors
  with integrated terminals are screened too.

The posture is mapped to OWASP web and LLM-application guidance. See
[`docs/architecture.md`](docs/architecture.md) §8 for the concern → control → location table, and
[`docs/threat-model.md`](docs/threat-model.md) for actors, abuse cases and the known limits of each
heuristic.

---

## What it does

106 tools across 12 categories. The full reference, generated from the server's own descriptors, is
[`docs/tools.md`](docs/tools.md).

| Category | Tools | Names | Dangerous |
|---|---|---|---|
| vision | 6 | `capture_screen`, `capture_window`, `find_elements`, `get_element`, `get_ui_tree`, `list_displays` | 0 |
| input | 10 | `clipboard_read`, `clipboard_write`, `drag_drop`, `hover`, `keyboard_shortcut`, `keyboard_type`, `mouse_action`, `scroll`, `set_value`, `ui_action` | 0 |
| window | 11 | `close_app`, `control_window`, `focus_app`, `handle_dialogs`, `launch`, `list_apps`, `list_windows`, `menu_invoke`, `menu_list`, `menu_open`, `wait_for` | 0 |
| desktop | 8 | `idle_status`, `lock_screen`, `media_control`, `notify_user`, `play_audio`, `power_control`, `speak`, `system_settings` | 1 |
| browser | 13 | `browser_act`, `browser_connect`, `browser_cookies`, `browser_dialog`, `browser_disconnect`, `browser_eval`, `browser_navigate`, `browser_network`, `browser_query`, `browser_screenshot`, `browser_snapshot`, `browser_tabs`, `browser_wait` | 3 |
| terminal | 14 | `command_info`, `exec`, `man_page`, `process_list`, `process_signal`, `pty_close`, `pty_list`, `pty_read`, `pty_resize`, `pty_signal`, `pty_spawn`, `pty_write`, `scheduled_tasks`, `service_control` | 4 |
| filesystem | 15 | `fs_archive`, `fs_copy`, `fs_delete`, `fs_list`, `fs_metadata`, `fs_mkdir`, `fs_move`, `fs_patch`, `fs_read`, `fs_search`, `fs_symlink`, `fs_watch`, `fs_write`, `mount_control`, `storage_inspect` | 2 |
| network | 8 | `bluetooth_pair`, `dns_lookup`, `firewall_rules`, `http_request`, `network_interfaces`, `network_manage`, `packet_diagnostics`, `socket_inspection` | 4 |
| system | 9 | `bus_devices`, `disk_usage`, `echo`, `hardware_telemetry`, `os_info`, `ping`, `proc_memory_read`, `sys_logs`, `system_config` | 2 |
| packages | 7 | `app_info`, `app_install`, `app_install_plan`, `app_list_installed`, `app_search`, `app_uninstall`, `app_update` | 3 |
| credentials | 2 | `secure_vault`, `ssh_gpg_identities` | 1 |
| memory | 3 | `memory_find`, `memory_forget`, `memory_save` | 0 |

Only `vision`, `input` and `window` are enabled out of the box, and only their non-dangerous tools.

**The cheapest way to observe is `get_ui_tree`** — plain text, no image, no vision tokens. Screen capture is
the documented fallback for surfaces with no accessibility tree (canvases, games, some Electron apps).
Captures deduplicate against the previous frame, so polling an unchanged screen costs nothing.

---

## Install

Desktop control requires macOS. The browser, filesystem, process, network, system, credential, PTY, package
and recall engines build and run on Linux too.

**From a release.** Download the archive for your platform, verify it, and unpack:

```sh
curl -LO https://github.com/paooxide/ui-draw-mcp/releases/download/v0.1.0/agentctl-v0.1.0-aarch64-apple-darwin.tar.gz
curl -LO https://github.com/paooxide/ui-draw-mcp/releases/download/v0.1.0/SHA256SUMS
shasum -a 256 -c SHA256SUMS --ignore-missing
tar xzf agentctl-v0.1.0-aarch64-apple-darwin.tar.gz
```

Release binaries are **not signed or notarized**. macOS quarantines anything a browser downloaded, so clear
the flag before first run (a `curl` download is not quarantined and needs no such step):

```sh
xattr -d com.apple.quarantine ./agentctl 2>/dev/null || true
```

**From source.**

```sh
cargo install --git https://github.com/paooxide/ui-draw-mcp agentctl --locked
# or, without compiling:
cargo binstall --git https://github.com/paooxide/ui-draw-mcp agentctl
```

**macOS permissions.** Grant **Accessibility** (and **Screen Recording** if you want captures) to the
application that *launches* `agentctl` — your terminal, Claude Desktop, Cursor — not to `agentctl` itself.
macOS attributes a child process's permissions to whoever spawned it. `agentctl doctor` reports what is
granted; [`docs/clients.md`](docs/clients.md) explains the rest.

---

## Quick start

```sh
agentctl doctor          # OS, permissions, enabled categories, kill switch, transport
```

Register it with a client — for Claude Code:

```sh
claude mcp add agentctl -- /usr/local/bin/agentctl serve
```

Then ask the agent to do something a shell cannot: *"Open TextEdit, type today's date, and save it to my
Desktop."* Per-client setup for Claude Desktop, Cursor and Gemini is in
[`docs/clients.md`](docs/clients.md).

To check the protocol without a client, pipe newline-delimited JSON-RPC at it:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ping","arguments":{}}}' \
  | agentctl serve
```

---

## Configuration

Settings live in `~/.agentctl/config.toml`, or wherever `$AGENTCTL_CONFIG` points.
[`config.example.toml`](config.example.toml) documents every key and is the file to copy.

A config file that exists but does not parse **aborts startup**. Falling back to defaults could silently
widen a policy an operator had deliberately made stricter.

```sh
agentctl config print    # the effective configuration, secrets redacted
agentctl tools           # the tools an agent would currently be offered
agentctl tools --all     # every tool in the build, regardless of policy
```

Other commands:

```sh
agentctl serve           # MCP over stdio (default)
agentctl serve --http    # MCP over loopback HTTP with a bearer token
```

The HTTP transport is loopback-only, requires a token, refuses cross-origin requests, and implements the
JSON subset of Streamable HTTP. stdio is the supported path.

---

## Architecture

A Cargo workspace: a protocol core, a policy kernel, one engine crate per capability category, one backend
crate per operating system, and a composition-root binary. Dependencies point inward — engines depend on
shared types and their own backends, never on the core — so the policy gate cannot be bypassed from inside
an engine.

| Crate | Role |
|---|---|
| `mcp-types` | Shared vocabulary: `Envelope`, `ErrorCode`, `ToolDescriptor`, `Tier`, `Category`, `ToolModule`, `CallCtx`. |
| `mcp-core` | Protocol: JSON-RPC framing, `initialize`/`tools/list`/`tools/call`, dispatch, registry. |
| `mcp-policy` | The security kernel: category and tier gates, destructive-input gate, consent, denial budget, kill switch, audit, redaction. |
| `mcp-a11y` | Accessibility snapshots, the element-ref arena, tree flattening. |
| `mcp-vision` | Displays and screen/window capture, with change detection and cost accounting. |
| `mcp-input` | Semantic and coordinate input, and the destructive-keystroke gate. |
| `mcp-window` | Windows, applications, menus, dialogs, and the `wait_for` settle primitive. |
| `mcp-browser` | The Chrome DevTools Protocol engine — OS-independent, so the real backend ships here. |
| `mcp-fs` | Filesystem, contained by a resolve-then-check path jail. |
| `mcp-proc` | `exec` (argv, no shell by default), process listing and signals. |
| `mcp-net` | HTTP with SSRF containment, DNS, interfaces. |
| `mcp-sys` | Read-only OS, hardware, disk and log telemetry. |
| `mcp-sec` | Credentials. No plaintext secret read exists. |
| `mcp-pty` | Real PTY sessions on `posix_openpt`. |
| `mcp-pkg` | Package lifecycle, with a protected set that can never be uninstalled. |
| `mcp-desktop` | Session, power and settings — including `notify_user`, the agent's channel to a human. |
| `mcp-memory` | Optional recall of task recipes. Off by default. |
| `mcp-macos` | The real macOS backend: AXUIElement, CGEvent, CoreGraphics. |
| `test-support` | An in-process MCP client, so tests drive the real protocol rather than calling engines. |
| `agentctl` | The composition root: CLI, config, and wiring only the enabled engines. |

---

## Status

Every planned capability category has an engine, and both halves are validated against real systems rather
than test doubles — there are no fake backends in this repository by policy.

| Area | State |
|---|---|
| Protocol, policy, consent, audit, kill switch | Shipped, verified end to end |
| Perception, input, windows, menus, capture | Shipped, validated on real macOS |
| Browser (CDP) | Shipped, validated against live Chrome |
| Terminal, filesystem, network, system, credentials, PTY, packages, recall | Shipped |
| Hardening: fuzzing, red-team suites, HTTP transport, CI | Shipped; six real defects found and fixed |
| Linux and Windows desktop backends | Not started |
| `ocr_region`, `capture_audio`, `virtual_desktop` | Deferred by design |
| `privilege_run` | Deliberately never — root defeats every other control |

[`PROGRESS.md`](PROGRESS.md) is the running log, including what each validation run actually proved and the
bugs it caught.

---

## Development

```sh
cargo build --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
AGENTCTL_SKIP_LIVE=1 cargo test --workspace
```

Live tests drive real browsers and the real desktop. `AGENTCTL_SKIP_LIVE=1` skips them; the GUI suites
additionally require `AGENTCTL_LIVE_GUI=1`, because they steal window focus and synthesise keystrokes.
See [`CONTRIBUTING.md`](CONTRIBUTING.md).

---

## Docs

- [`docs/tools.md`](docs/tools.md) — every tool, its tier and its arguments (generated).
- [`docs/clients.md`](docs/clients.md) — connecting Claude Code, Claude Desktop, Cursor and Gemini.
- [`docs/threat-model.md`](docs/threat-model.md) — actors, abuse cases, controls, and known gaps.
- [`docs/planning.md`](docs/planning.md) — design source of truth: decisions, tool catalog, implementation log.
- [`docs/architecture.md`](docs/architecture.md) — crate decomposition, layering rules, security architecture.
- [`docs/implementation-plan.md`](docs/implementation-plan.md) — phases and workstreams.
- [`docs/test-plan.md`](docs/test-plan.md) — the test taxonomy.
- [`CHANGELOG.md`](CHANGELOG.md) · [`SECURITY.md`](SECURITY.md) · [`CONTRIBUTING.md`](CONTRIBUTING.md)

## License

Apache-2.0. See [`LICENSE`](LICENSE).
