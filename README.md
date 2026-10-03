# agentctl

`agentctl` is a [Model Context Protocol](https://modelcontextprotocol.io) server that gives an AI agent
reliable, structured control of a real computer: GUI applications, a Chromium browser, and the machine
underneath them.

The difference from a shell-command agent is grounding. Instead of guessing at pixels, `agentctl` reads the
operating system's **accessibility tree** (roles, names, values, bounds), so an action targets a real UI
element rather than a screenshot coordinate. An agent can open an app, find the Save button by name, click
it, and confirm from the tree that the state changed.

It is pure capability. There is no embedded model and no internal planning loop: any MCP-capable client
(Claude Code, Claude Desktop, Cursor, Gemini) decides what to do, and the server validates, gates, executes
and reports. Desktop control runs on macOS and on Linux (GNOME on Wayland is where it is validated); the
browser and the commodity engines are platform-independent.

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
- **Full audit.** Every call is written before and after execution to an append-only JSONL log, hash-chained
  and Ed25519-signed. With an operator key (`agentctl audit keygen`) a rewritten log fails
  `agentctl audit verify --pubkey`; without one, verify says the log is only self-consistent.
- **Roles and invariants.** A role (`policy.role`) narrows what a session may call and never widens it.
  `[invariants]` refuses protected paths and denied domains anywhere in a call's arguments, before the gate
  and under every profile. Invariants read argument text: they are a tripwire, not containment, and their
  known gaps are pinned in tests.
- **Run it from a different terminal than it controls.** If the agent drives GUI apps, keystrokes go to
  whatever is frontmost. Typed text destined for a shell is screened for destructive commands, and editors
  with integrated terminals are screened too.

The posture is mapped to OWASP web and LLM-application guidance. See
[`docs/architecture.md`](docs/architecture.md) §8 for the concern → control → location table, and
[`docs/threat-model.md`](docs/threat-model.md) for actors, abuse cases and the known limits of each
heuristic.

---

## What it does

122 tools across 12 categories. The full reference, generated from the server's own descriptors, is
[`docs/tools.md`](docs/tools.md).

| Category | Tools | Names | Dangerous |
|---|---|---|---|
| vision | 8 | `capture_screen`, `capture_window`, `find_elements`, `get_element`, `get_ui_tree`, `list_displays`, `ocr_region`, `ui_extract` | 0 |
| input | 12 | `clipboard_read`, `clipboard_write`, `drag_drop`, `hover`, `input_showcase`, `keyboard_shortcut`, `keyboard_type`, `mouse_action`, `scroll`, `set_value`, `ui_action`, `ui_fill_form` | 0 |
| window | 11 | `close_app`, `control_window`, `focus_app`, `handle_dialogs`, `launch`, `list_apps`, `list_windows`, `menu_invoke`, `menu_list`, `menu_open`, `wait_for` | 0 |
| desktop | 8 | `idle_status`, `lock_screen`, `media_control`, `notify_user`, `play_audio`, `power_control`, `speak`, `system_settings` | 1 |
| browser | 25 | `browser_act`, `browser_assert`, `browser_branch`, `browser_capture`, `browser_challenge`, `browser_checkpoint`, `browser_connect`, `browser_cookies`, `browser_dialog`, `browser_disconnect`, `browser_eval`, `browser_extract`, `browser_fill_form`, `browser_flow`, `browser_navigate`, `browser_network`, `browser_profile`, `browser_query`, `browser_record`, `browser_screenshot`, `browser_showcase`, `browser_snapshot`, `browser_tabs`, `browser_viewport`, `browser_wait` | 4 |
| terminal | 14 | `command_info`, `exec`, `man_page`, `process_list`, `process_signal`, `pty_close`, `pty_list`, `pty_read`, `pty_resize`, `pty_signal`, `pty_spawn`, `pty_write`, `scheduled_tasks`, `service_control` | 4 |
| filesystem | 15 | `fs_archive`, `fs_copy`, `fs_delete`, `fs_list`, `fs_metadata`, `fs_mkdir`, `fs_move`, `fs_patch`, `fs_read`, `fs_search`, `fs_symlink`, `fs_watch`, `fs_write`, `mount_control`, `storage_inspect` | 2 |
| network | 8 | `bluetooth_pair`, `dns_lookup`, `firewall_rules`, `http_request`, `network_interfaces`, `network_manage`, `packet_diagnostics`, `socket_inspection` | 4 |
| system | 9 | `bus_devices`, `disk_usage`, `echo`, `hardware_telemetry`, `os_info`, `ping`, `proc_memory_read`, `sys_logs`, `system_config` | 2 |
| packages | 7 | `app_info`, `app_install`, `app_install_plan`, `app_list_installed`, `app_search`, `app_uninstall`, `app_update` | 3 |
| credentials | 2 | `secure_vault`, `ssh_gpg_identities` | 1 |
| memory | 3 | `memory_find`, `memory_forget`, `memory_save` | 0 |

Only `vision`, `input` and `window` are enabled out of the box, and only their non-dangerous tools.

**The cheapest way to observe is `get_ui_tree`**: plain text, no image, no vision tokens. Ask it for a
delta with `since` and a follow-up observation costs only what changed. `find_elements` is cheaper still
when the question is narrow, and `expect` on an input tool folds act-wait-verify into one call.

For surfaces with no accessibility tree (canvases, games, some Electron apps), `ocr_region` reads the text
and returns a clickable box for each line. Screen capture remains the last resort; captures deduplicate
against the previous frame, so polling an unchanged screen costs nothing.

---

## Install

Desktop control runs on macOS (AXUIElement) and Linux (AT-SPI and the desktop portals). The browser,
filesystem, process, network, system, credential, PTY, package and recall engines run on both.

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
application that *launches* `agentctl` (your terminal, Claude Desktop, Cursor), not to `agentctl` itself.
macOS attributes a child process's permissions to whoever spawned it. `agentctl doctor` reports what is
granted; [`docs/clients.md`](docs/clients.md) explains the rest.

**Linux.** Nothing to grant up front. Perception reads the AT-SPI accessibility bus, which every graphical
session has. The first synthetic keystroke or click opens a remote-desktop portal session, and the desktop
asks you once, in its own dialog, to allow it; the grant is remembered. Consent for risky actions is a
zenity dialog (or a notification with Allow and Deny buttons) with Deny as the default. `agentctl doctor`
reports the bus, the portals and what is missing.

---

## Quick start

```sh
agentctl doctor          # OS, permissions, enabled categories, kill switch, transport
```

Register it with a client. For Claude Code:

```sh
claude mcp add agentctl -- /usr/local/bin/agentctl serve
```

Then ask the agent to do something a shell cannot: *"Open TextEdit, type today's date, and save it to my
Desktop."* Per-client setup for Claude Desktop, Cursor and Gemini is in
[`docs/clients.md`](docs/clients.md).

Or drive it without a client at all. `agentctl bridge` is a complete MCP client: it spawns the server as
a child process and runs the whole loop against Gemini.

```sh
printf 'GEMINI_API_KEY=%s\n' "$KEY" > .env && chmod 600 .env
agentctl bridge --list-models
agentctl bridge --task "What application windows are open right now?"
```

Tool calls are traced as they happen. [`docs/demo.md`](docs/demo.md) covers both routes and the recorded
transcripts; the first two runs of this bridge found two real defects the whole test suite had passed
over, which is roughly what it is for.

To check the protocol without a client, pipe newline-delimited JSON-RPC at it:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ping","arguments":{}}}' \
  | agentctl serve
```

---

## Browser testing

The `browser` engine drives a Chromium-family browser (Chrome, Chromium, Edge or Brave) over the DevTools
Protocol, and it is built for an agent to run regressions and UI tests natively. Safari is available on macOS
as an experimental second engine through `safaridriver` (`launch.browser: "safari"`; run
`safaridriver --enable` once, enable Develop > Allow Remote Automation, and restart Safari). Verified live on
Safari 27: navigation, snapshot, `browser_act` click and type by ref and selector, `within`, `browser_fill_form`,
`htmx_settled` (against the real htmx 2.x library), `browser_eval` on a page whose CSP forbids `eval`, action
errors reported unchanged under `browser_showcase`, cookies, profile state and restore, capture and screenshots. It
has no network interception, dialog policy, key presses, device emulation, recording, branches or checkpoints (each
returns `UNSUPPORTED`). `browser_capture` arms only the page that is open, not later navigations. Firefox speaks a
different protocol and is not supported.

- `browser_act` acts on a `ref` or, in one call, a `by`+`query` selector (no separate `browser_query`).
- `browser_capture` installs a page hook (persists across navigations) that records fetch/XHR with
  request and response bodies, plus console errors and uncaught exceptions. Bodies can hold secrets, so
  it is Dangerous-tier and off unless enabled.
- `browser_assert` settles then checks text, URL, a selector's count, and (with capture on) no console
  errors and no failed requests, returning `{passed, checks}` and an error when it fails.
- `browser_flow` saves a named sequence of steps and replays it deterministically, stopping at the first
  failing step. A green run never invokes a model; a failure is where an agent takes over.

### UX suite

`browser_assert` also carries UX clauses, so accessibility, design-system and visual checks ride the same
`{passed, checks}` flow, `browser_flow` and `agentctl test` report as the functional ones:

- **Accessibility** (`a11y: true`) runs a built-in WCAG audit: alt text, form labels, control names, colour
  contrast, target size, positive tabindex, duplicate ids and page lang. `{ignore:[...]}` drops rules.
- **Style / design tokens** (`style: {colors, fonts, font_sizes, spacing}`) flags computed values that are
  not on the allow-lists, so a UI that drifts off the design system fails.
- **Component** (`component: {selector, role, visible, states}`) asserts one element's role, visibility and
  ARIA states; `within` scopes the a11y/style checks to that component's subtree.
- **Responsive** — `browser_viewport` (or a `viewport` flow step) emulates device metrics, so a flow can
  assert at phone, tablet and desktop widths.
- **Visual regression** (`visual: "name"`) screenshots and compares to a saved baseline; the first run
  saves it, later runs fail when the changed-pixel ratio exceeds `tolerance` or the dimensions change.
- **Judge UX review** (`ux: {dims:[...]}`) scores clarity, hierarchy, affordance and consistency with the
  judge. Advisory by default (it never fails a run, and degrades to skipped when the judge is off); set
  `gate: true` to fail when a dimension falls below `min`. An AI opinion is a signal, not a real user.

A flow step is just an assert with these fields, e.g. `{op:"viewport",width:390,height:844,mobile:true}`
then `{op:"assert",a11y:true}`, `{op:"assert",visual:"home"}`, `{op:"assert",ux:{}}`.

Point a Claude (or other MCP) session at agentctl as a server so these are native tool calls rather than a
shell driver. A minimal project wiring:

```json
{ "mcpServers": { "agentctl": {
  "command": "/abs/path/target/release/agentctl", "args": ["serve"],
  "env": { "AGENTCTL_CONFIG": "/abs/path/agentctl.test.toml" } } } }
```

with a test profile of `access = "bypass"` (every capability on, no prompts) for a machine you own and are
watching.

`agentctl test` replays saved flows and reports each flow's result, its elapsed time, and the issues it
hit (failing step, console errors, failed requests). With no `--attach` it launches its own throwaway
browser (any Chromium-family browser it finds: Chrome, Chromium, Edge or Brave, native or flatpak) and
stops it again when done; a fresh one per flow means no state leaks between tests. By default it shows a window when a display is present, so
you can watch the run, and stays headless in CI where there is none. It exits non-zero on any failure, so
it drops into a pipeline:

```sh
agentctl test                       # all saved flows (headed if a display exists, else headless)
agentctl test checkout login        # named flows
agentctl test --headless            # force headless (or --headed to force a window)
agentctl test --attach 9222         # reuse a Chromium you started with --remote-debugging-port=9222
agentctl test --json report.json --strict   # machine report (per-flow + total ms); issues also fail
```

A green run never invokes a model; a failure is where an agent session takes over.

---

## The judge (optional)

The deterministic checks decide everything they can. What they cannot decide is semantic: which of forty
buttons is the one that saves, whether the text a page returned is talking to the model, whether a typed
line would wipe a disk when the pattern list did not match. With `[judge]` enabled, those questions go to a
System One model (TypeSafe's `jev`), which answers a typed question with a probability in about a hundred
milliseconds. Where it is used:

- `find_elements` takes `describe` ("the button that saves the document") and returns the candidates
  ranked, each with a probability, plus whether anything fits at all.
- `handle_dialogs` takes `intent` ("save my work") and returns a `suggestion` naming the button that
  serves it. Advice only: it presses nothing, and the agent still acts through `ui_action` on a ref.
- `memory_find` takes `rerank` to reorder recalled recipes by how well each one's goal fits yours, rather
  than by success count alone. It falls back to success-count order when the judge cannot answer.
- `wait_for` and every `expect` clause take `judge`, a plain-language claim about the UI, and report the
  probability alongside the structural result.
- Results marked untrusted get a second opinion on whether their text is addressed to a model. The flag
  can be added, never removed.
- Text headed for a shell gets a second opinion after the destructive patterns. A yes escalates to consent
  or a denial; a no changes nothing.
- `agentctl bridge --prune` asks the judge which tools a task plausibly needs and declares only those to
  the model, always keeping a core observe/wait set.

**A judgment can only tighten.** Nothing it says is consulted on an allow path, so a wrong, absent or
manipulated answer makes the server more careful, never less. When the service is unreachable, the
deterministic answer stands and the failure is counted. The key comes from `TYPESAFE_API_KEY`, `.env` or
`~/.agentctl/typesafe.key`, never from the config file or an argument. What is sent leaves the machine:
element names, window titles, page text after redaction, typed commands, never a secure field's value.
The `threshold` bar applies everywhere by default; `destructive_threshold`, `injection_threshold` and
`match_threshold` tune the destructive second opinion, the injection flag and semantic ranking separately.
See [`docs/threat-model.md`](docs/threat-model.md) §7.

---

## Permissions in one word

Granting each category and naming each dangerous tool is a chore. `policy.access` collapses it to one
setting that turns on every capability and picks how risk is handled:

| `access` | Capability | Prompts | Destructive action |
|---|---|---|---|
| `ask` | all on | dangerous tools and destructive actions ask you | confirmed via the dialog |
| `auto` | all on | none | refused rather than done blind |
| `bypass` | all on | none | gate off; only the kill switch and human-override remain |

`ask` is the safe profile: read and act freely, confirm the dangerous. `bypass` is the explicit "I take
responsibility". When `access` is set, the granular `categories` and `enable` lists are ignored; leave it
unset to keep the fine-grained control. The kill switch (`~/.agentctl/STOP`) and the mouse-override stop
the agent under every profile, including bypass.

---

## Entering a password

The owner can have the agent enter a password, in a terminal prompt or a GUI field, by passing it as an
argument. Mark the call `secret: true` on `keyboard_type`, `set_value`, `pty_write` or `clipboard_write`,
and two things happen: the value is replaced with a length marker in the append-only audit log, and it is
never sent to the judge. The action still carries the real value to the OS. Without the flag the payload
would be logged verbatim, so the flag is the difference between a password that persists and one that does
not.

`browser_act` and `browser_fill_form` take the same flag. A recorded browser flow never stores a password:
the recorder writes a named `secret_ref`, and the value is supplied when the flow is replayed
(`browser_flow run` with `secrets`, or `AGENTCTL_SECRET_<REF>` for `agentctl test`).

Two limits worth knowing. A GUI password field's *contents* are never readable (secure fields are redacted
from every perception tool), but writing to one is allowed, which is what entering a password needs. And on
Wayland `keyboard_type` needs the target window focused; `set_value` on a field ref does not, so for a
background login form `set_value` with `secret: true` is the robust choice.

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
crate per operating system, and a composition-root binary. Dependencies point inward: engines depend on
shared types and their own backends, never on the core, so the policy gate cannot be bypassed from inside
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
| `mcp-browser` | The Chrome DevTools Protocol engine. OS-independent, so the real backend ships here. |
| `mcp-fs` | Filesystem, contained by a resolve-then-check path jail. |
| `mcp-proc` | `exec` (argv, no shell by default), process listing and signals. |
| `mcp-ssrf` | The resolved-address guard, a dependency-free leaf shared by the network and browser engines. |
| `mcp-judge` | Typed judgments from a System One model (TypeSafe's `jev`), consulted only where a judgment can tighten a decision or rank candidates. Off by default. |
| `mcp-net` | HTTP with SSRF containment, DNS, interfaces. |
| `mcp-sys` | Read-only OS, hardware, disk and log telemetry. |
| `mcp-sec` | Credentials. No plaintext secret read exists. |
| `mcp-pty` | Real PTY sessions on `posix_openpt`. |
| `mcp-pkg` | Package lifecycle, with a protected set that can never be uninstalled. |
| `mcp-desktop` | Session, power and settings, including `notify_user`, the agent's channel to a human. |
| `mcp-memory` | Optional recall of task recipes. Off by default. |
| `mcp-macos` | The real macOS backend: AXUIElement, CGEvent, CoreGraphics. |
| `mcp-linux` | The real Linux backend: AT-SPI2 perception and actions, RemoteDesktop and Screenshot portals, D-Bus session control. |
| `test-support` | An in-process MCP client, so tests drive the real protocol rather than calling engines. |
| `agentctl` | The composition root: CLI, config, and wiring only the enabled engines. |

---

## Personal data (optional)

With `policy.anonymize = true`, tool results reach the model with personal data replaced by stable tokens:
`Patient SSN 123-45-6789` becomes `Patient SSN <SSN_1>`, and the audit log records the token. Card numbers
are Luhn-checked, SSNs follow SSA rules, phone numbers must look like phone numbers, and names you register
are matched exactly. When the model types `<SSN_1>` into a field (`keyboard_type`, `set_value`,
`ui_fill_form`, `browser_fill_form`, `browser_act`), the real value goes to the field. A token anywhere
else, such as a URL, a command or a file, is refused, so it cannot be sent off the machine by
de-tokenization.

What it does not do: a model that types a token into a field on a page it chose, or into a terminal, still
delivers the value there. It is off by default because it rewrites every result, including addresses and
numbers in network and system output.

---

## Verifying an audit log

```sh
agentctl audit keygen                     # once: writes a 0600 key and prints its public key
# config.toml: [policy] audit_signing_key = "<path it printed>"
agentctl audit verify <session> --pubkey <hex>
agentctl audit export <session> --pubkey <hex> --format soc2   # or soc2-json, csv, hipaa-json
```

Keep the public key somewhere the agent cannot write. The exports describe what the log contains; they are
evidence for a review, not a certification.

---

## Status

Every planned capability category has an engine, and both halves are validated against real systems rather
than test doubles. There are no fake backends in this repository by policy.

| Area | State |
|---|---|
| Protocol, policy, consent, audit, kill switch | Shipped, verified end to end |
| Perception, input, windows, menus, capture | Shipped, validated on real macOS and on GNOME 50 (Wayland) |
| Browser (CDP), including forms, profiles, branches, checkpoints, recording | Shipped, validated against live Chrome |
| Browser on Safari (WebDriver) | Experimental; live suite run on Safari 27 (macOS 26.7); CDP-only features return `UNSUPPORTED` |
| PII tokenizer, signed audit and compliance export, roles and invariants | Shipped, opt-in |
| Demo mode (pointer glide, browser overlay) | Shipped, off by default |
| Terminal, filesystem, network, system, credentials, PTY, packages, recall | Shipped |
| Hardening: fuzzing, red-team suites, HTTP transport, CI | Shipped; six real defects found and fixed |
| Windows desktop backend | Not started |
| `capture_audio`, `virtual_desktop` | Deferred by design |
| `privilege_run` | Deliberately never: root defeats every other control |

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

- [`docs/tools.md`](docs/tools.md): every tool, its tier and its arguments (generated).
- [`docs/clients.md`](docs/clients.md): connecting Claude Code, Claude Desktop, Cursor and Gemini.
- [`docs/demo.md`](docs/demo.md): driving the server with a real model, and the recorded transcripts.
- [`docs/threat-model.md`](docs/threat-model.md): actors, abuse cases, controls, and known gaps.
- [`docs/architecture.md`](docs/architecture.md): crate decomposition, layering rules, security architecture.
- [`docs/test-plan.md`](docs/test-plan.md): the test taxonomy.
- [`CHANGELOG.md`](CHANGELOG.md) · [`SECURITY.md`](SECURITY.md) · [`CONTRIBUTING.md`](CONTRIBUTING.md)

## License

Apache-2.0. See [`LICENSE`](LICENSE).
