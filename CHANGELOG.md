# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `find_elements` — query the accessibility tree by role, name substring or
  proximity to a screen point instead of reading all of it. On a busy
  application a targeted query is an order of magnitude smaller than the full
  tree, and the refs it returns are usable by `ui_action` because it takes and
  installs a fresh snapshot rather than querying a retained one.
- MCP tool annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`,
  `openWorldHint`) and display titles in `tools/list`, derived from the tier the
  policy gate already enforces.
- `policy.mode = "dry_run"` — a rehearsal in which read-tier tools run normally
  and anything that would change something reports what it would have done,
  including whether a human would have been asked.
- `browser_disconnect`, and a shutdown hook so browsers this session launched
  are stopped on exit rather than leaked.
- `agentctl tools` and a generated tool reference at `docs/tools.md`, checked by
  CI so it cannot drift from the descriptors.

### Security

- Results from the tools that return third-party content now carry
  `provenance: "untrusted"`, with an advisory `suspicious_instructions` flag for
  text that reads as an instruction aimed at a model. Applied centrally so it
  cannot be forgotten or spoofed. `redteam_injection.rs` documents what the
  heuristic misses.
- The server's own state directory is denied inside any filesystem root.

### Fixed

- `keyboard_type` inherited latched modifier flags, so with Command held every
  character became a menu shortcut: nothing was typed and the call reported
  success.
- `list_windows` ignored its `app` argument and answered about whatever was
  frontmost.

## [0.1.0] — 2026-09-03

First tagged release. An MCP server that gives an AI agent grounded control of a real computer: 105 tools
across 12 capability categories, gated by a policy layer that treats the driving agent as untrusted.

### Added

- **Protocol.** MCP over stdio (JSON-RPC 2.0, protocol revision `2025-11-25`): `initialize`, `tools/list`
  filtered to enabled categories, and `tools/call`. An optional loopback HTTP transport with mandatory
  bearer authentication.
- **Policy kernel.** Category and tier gates, a per-tool opt-in for dangerous tools, an out-of-band native
  consent dialog with a prompt budget, a denial budget, a kill switch, secret redaction, and an
  append-only JSONL audit log written before and after every call.
- **Perception.** `get_ui_tree` and `get_element` over the macOS accessibility tree, with element refs that
  are stable paths carrying role and name identity rather than native handles, so they survive UI mutation.
  `list_displays`, `capture_screen` and `capture_window` return native MCP image blocks, deduplicated
  against the previous frame and right-sized for vision-token cost.
- **Input.** Semantic actions (`ui_action`, `set_value`), keyboard (`keyboard_type`, `keyboard_shortcut`),
  coordinate input (`mouse_action`, `scroll`, `hover`, `drag_drop`) and the clipboard. Keystrokes destined
  for a shell are screened for destructive commands, with editors screened alongside terminals because
  their integrated terminals run the same shell.
- **Windows and applications.** `list_windows`, `list_apps`, `launch`, `close_app`, `control_window`,
  `focus_app`, `menu_open`/`menu_invoke`/`menu_list`, `handle_dialogs`, and the `wait_for` settle
  primitive.
- **Browser.** Thirteen tools over the Chrome DevTools Protocol, with a hand-rolled CDP client rather than
  a heavyweight automation dependency. Page-side XPath refs are re-resolved on act, so they survive
  navigation.
- **Commodity engines.** Filesystem behind a resolve-then-check path jail; `exec` on argv with no shell by
  default; HTTP with SSRF containment; read-only system telemetry; credentials with no plaintext read; real
  PTY sessions; package lifecycle with a protected set; optional recall of task recipes.
- **Session and desktop.** `notify_user` — the agent's channel to a human — plus idle status, screen lock,
  media and power control, speech and audio playback.
- **CLI.** `serve`, `doctor`, `config print`, and `tools` for a generated tool reference.
- **Test support.** An in-process MCP client so tests drive the real protocol, and live task suites that
  exercise a real browser and a real desktop end to end.

### Security

Six defects found by the hardening phase, each now pinned by a test:

- `rm  -rf /` with two spaces walked through the destructive-command check. Substring matching against a
  raw command string is defeated by the space bar; matching now runs on a whitespace-collapsed, lowercased
  form, with privilege words checked as tokens.
- `~/.SSH/id_rsa` bypassed the credential deny-list on macOS's case-insensitive filesystem, opening the
  very same file as `~/.ssh/id_rsa`. Comparison is now case-insensitive.
- IPv6 transition addresses smuggled IPv4 past the SSRF guard: `64:ff9b::a9fe:a9fe` (NAT64) and
  `2002:a9fe:a9fe::` (6to4) both reach cloud metadata. The guard now judges the embedded v4 address.
- An unterminated JSON-RPC frame exhausted memory before any policy ran. Frames are capped and the stream
  resynchronises at the next newline.
- A large floating-point request id came back as a different number, so a client could never match the
  reply to its request. Ids are restricted to strings and integers.
- `notifications/initialized` sent with an id got no reply. Notification-ness is decided by the absence of
  an id, not by the method name.

Also in this release:

- The server's own state directory is denied inside any filesystem root. With `roots = ["~"]` the agent
  could otherwise rewrite `config.toml` to widen its policy, truncate the audit log, or delete the STOP
  file.
- Fuzzing of the JSON-RPC parser with no fuzzing dependency, and red-team suites for the destructive gate,
  the filesystem jail and the SSRF guard. Each suite ends with a test asserting the known bypasses still
  exist, so the limits of a heuristic stay visible.

### Fixed

Field bugs that only a run on real hardware could surface:

- `AXFocusedApplication` returns nothing on modern macOS even with an app plainly frontmost, which made
  every accessibility and window tool fail. The frontmost process is now resolved from the CoreGraphics
  window list.
- Synthetic mouse events left `kCGMouseEventClickState` unset, so double- and triple-clicks arrived as
  ordinary single clicks, and a plain click behaved as a shift-click by inheriting latched modifiers.
- `keyboard_type` inherited whatever modifiers the system believed were held. With Command latched, every
  character became a menu shortcut: nothing was typed, and the call still reported the full character
  count.
- `list_windows` discarded its `app` argument and answered about whatever was frontmost, so asking about
  one application returned another's windows.
- Drag was a teleport with no intermediate motion, which targets that track movement ignore.
- Sheets, popovers and cross-process authentication panels were invisible to `handle_dialogs`.
- A JavaScript dialog wedged a tab once the CDP `Page` domain was enabled.
- Launched browsers were never stopped: the `Child` handle was dropped, leaking a process and a temporary
  profile per launch.
- Only the filesystem jail canonicalised its roots, so a root configured as `/tmp/work` never matched a
  resolved path on macOS, where `/tmp` is a symlink.

### Known limitations

- Desktop control is macOS-only. The browser and commodity engines are platform-independent.
- Release binaries are unsigned and un-notarized.
- `ocr_region`, `capture_audio` and `virtual_desktop` are deferred; `privilege_run` and process-memory
  writes are deliberate omissions.
- The destructive-command gate cannot see through shell expansion, and hard links are invisible to path
  resolution. Both are documented in [`SECURITY.md`](SECURITY.md) and asserted by tests.

[Unreleased]: https://github.com/paooxide/ui-draw-mcp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/paooxide/ui-draw-mcp/releases/tag/v0.1.0
