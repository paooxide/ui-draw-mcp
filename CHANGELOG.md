# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **PII tokenizer, opt-in** (`policy.anonymize`, `AGENTCTL_ANONYMIZE`). Tool results reach the model with
  personal data replaced by stable tokens (`<SSN_1>`, `<EMAIL_2>`): registered names, SSA-valid SSNs,
  Luhn-checked card numbers, phone numbers, emails, MRNs, IPv4 addresses and common API key formats. The
  audit log keeps the tokens. Plaintext is restored only when a token is typed into a local field
  (`keyboard_type`, `set_value`, `ui_fill_form`, `browser_fill_form`, `browser_act`); a token in any other
  tool's arguments is refused before the gate, so it cannot be sent off the machine in a URL, command or
  file. Typing a token into a field on a page the model chose still delivers it; that gap is pinned in
  `documented_known_bypasses`. Off by default, because it rewrites every result.
- **Signed audit logs and compliance export.** Each audit record carries a sequence number, the previous
  record's hash and an Ed25519 signature, so deletion, reordering or editing breaks the chain.
  `agentctl audit keygen` creates a signing key for `policy.audit_signing_key`; `agentctl audit verify
  --pubkey` fails unless the log was signed by that key, and without one reports the log as
  self-consistent only. `agentctl audit export` writes a SOC2-style report or a HIPAA access-event CSV/JSON.
- **UX testing: accessibility, design-token style, component and responsive checks.** `browser_assert`
  gained UX clauses that ride the same `{passed, checks}` flow and `agentctl test` report as the functional
  ones: `a11y` runs a built-in WCAG audit (alt text, form labels, control names, colour contrast, target
  size, positive tabindex, duplicate ids, page lang); `style` checks computed colours/fonts/font-sizes/
  spacing against allow-lists and flags off-token values; `component` asserts one element's role, visibility
  and states (disabled/expanded/checked/...); `within` scopes any of these to a component subtree. New
  `browser_viewport` tool (and `viewport` flow step) emulates device metrics for responsive testing.
- **UX testing: visual regression and a judge-scored UX review.** `browser_assert` gained `visual`
  (screenshot vs a named baseline: the first run saves it, later runs diff the pixels in-page and fail past
  a tolerance or on a dimension change) and `ux` (the judge scores clarity/hierarchy/affordance/consistency
  over the page's facts; advisory by default and skipped when the judge is off, `gate:true` makes a low
  dimension fail). Baselines are a file-backed store next to the flow store. `agentctl test` now surfaces
  every UX check (a11y violations, off-token values, visual diff, UX scores) per flow in its report, so a
  run shows them even when it passed overall.

- **Browser engine built for native regression/UI testing.** `browser_act` can locate by `by`+`query`
  selector in one call (no separate `browser_query`). `browser_capture` installs a persistent page hook
  recording fetch/XHR with request/response bodies plus console errors and uncaught exceptions
  (Dangerous-tier; off unless enabled). `browser_assert` settles then checks text/url/selector and, with
  capture on, no console errors and no failed requests, returning `{passed, checks}`. `browser_flow`
  saves and replays a named sequence of steps deterministically, stopping at the first failing step so a
  green run never needs a model.
- **`agentctl test`** replays saved flows and reports each flow's result, elapsed time and the issues it
  hit (failing step, console errors, failed requests), writing a `--json` report (per-flow and total ms)
  and exiting non-zero on failure (or on any issue with `--strict`) for CI. With no `--attach` it launches
  its own throwaway browser and stops it when done; it shows a window when a display is present (so a local
  run can be watched) and stays headless in CI, with `--headed`/`--headless` to force either way.
- **The browser engine can launch any installed Chromium-family browser and always stops the tree it
  started.** Launch discovery now covers Chrome, Chromium, Edge and Brave across native, snap and flatpak
  locations (the engine speaks CDP, so Firefox and Safari remain unsupported). Each launch takes its own
  free port rather
  than a fixed one, so back-to-back launches never collide. A launched browser is stopped with a CDP
  `Browser.close`, the only thing that reaps a sandboxed (flatpak/snap) browser's whole process tree,
  which a signal to the launcher cannot reach.
- **More places the judge helps, and per-use thresholds.** `handle_dialogs` takes `intent` and returns a
  `suggestion` naming the button that serves it (advice only; it presses nothing). `memory_find` takes
  `rerank` to reorder recalled recipes by semantic fit to the goal, falling back to success-count order
  when the judge is unavailable. `agentctl bridge --prune` lets the reference client ask the judge which
  tools a task plausibly needs and declares only those to the model, always keeping a core observe/wait
  set and degrading to the full list. `[judge]` now takes optional `destructive_threshold`,
  `injection_threshold` and `match_threshold`, each falling back to `threshold`, so the destructive second
  opinion, the injection flag and semantic ranking can be tuned separately.
- **Clipboard image and file-list formats (Linux).** `clipboard_read`/`clipboard_write` handle
  `format: "image"` (a base64 PNG) and `format: "files"` (a newline-separated `text/uri-list`) in addition
  to text and HTML, over the Wayland `wlr-data-control` path or the XWayland `xclip` fallback (`xsel`
  carries text only, and says so).
- **One-word permission profiles.** `policy.access = "ask" | "auto" | "bypass"` replaces enabling each
  category and naming each dangerous tool. All three turn on every capability; `ask` confirms a dangerous
  tool or destructive action through the dialog, `auto` runs unattended but refuses a clearly destructive
  action, and `bypass` turns off consent and the destructive gate (kill switch and human-override remain).
  The granular `categories`/`enable` config still works when `access` is unset.
- **Secret-safe input.** `keyboard_type`, `set_value`, `pty_write` and `clipboard_write` take
  `secret: true` for entering a password the owner provides. The payload is redacted from the append-only
  audit log (length marker only) and is never sent to the judge, while the real value still reaches the OS.
  The destructive-pattern check still runs offline. Without the flag, typed text is logged verbatim as
  before.
- **The judge** (`mcp-judge`, `[judge]` in the config, off by default): typed judgments from a System One
  model (TypeSafe's `jev`), consulted only where a judgment can tighten a decision or rank candidates.
  `find_elements` takes `describe` and returns candidates ranked with probabilities; `wait_for` and
  `expect` take `judge`, a claim about the UI, and report its probability; results marked untrusted get a
  second opinion on whether their text is addressed to a model (the flag can be added, never removed);
  text headed for a terminal or a PTY gets a second opinion after the destructive patterns (a yes
  escalates, a no changes nothing). Unreachable, keyless or disabled, it is skipped and counted. The key
  is read from `TYPESAFE_API_KEY`, `.env` or `~/.agentctl/typesafe.key`, never from config or argv. A
  red-team suite pins that no scripted answer can loosen anything.

- **Linux desktop backend** (`mcp-linux`). Perception over AT-SPI2: `get_ui_tree`, `find_elements`,
  `get_element` and delta snapshots read the same tree a screen reader does, with refs that keep the
  object reference and fall back to a path replay and an identity search when a widget is replaced.
  Semantic input (`ui_action`, `set_value`) through the widgets' own AT-SPI actions and text interfaces,
  so no pointer is involved. Keyboard and pointer input through the `RemoteDesktop` portal, which asks the
  person once and is remembered; `cmd` in a combo is translated to Control. Capture through the
  `Screenshot` portal, display geometry from Mutter, text recognition with the `ocrs` engine (models
  fetched on first use, or placed by hand). Windows, applications, menus and dialogs from AT-SPI;
  `launch` and `focus_app` through desktop entries and `org.freedesktop.Application`; `control_window`
  through GNOME's shortcuts. Session control over D-Bus: lock, idle, notifications, volume, colour scheme,
  brightness, do-not-disturb, MPRIS media, logind power, `espeak-ng` speech. Validated on GNOME 50 on
  Wayland.
- A consent dialog on Linux: `zenity --question` with Deny as the default, a critical notification with
  Allow and Deny buttons when zenity is absent, and a timeout that denies either way.
- `agentctl doctor` on Linux reports the accessibility bus, the session accessibility flag, the portal
  versions, whether the input grant has been given, the consent channel, the OCR models and the helper
  binaries, each with what to do when it is missing.
- The commodity engines now have real Linux paths where they shelled out to macOS tools: trash via `gio`
  (with an XDG fallback), storage and mounts via `lsblk`, `findmnt` and `udisksctl`, Wi-Fi and VPN via
  `nmcli`, services via `systemctl`, the keyring via the Secret Service (existence checks never see the
  value), bus devices, sysctl, process maps and telemetry from `/sys` and `/proc`.
- A Linux live suite (`agentctl/tests/live_linux.rs`): the read-only half runs on any graphical session,
  the acting half behind `AGENTCTL_LIVE_GUI=1`.
- The Linux clipboard falls back to `xclip` or `xsel` over XWayland where the compositor withholds the
  `wlr-data-control` protocol (GNOME/Mutter), and reports the limitation clearly where no bridge exists.

### Fixed

- `pty_spawn` with no `shell` picked the first allowed shell whether or not it existed, so on a machine
  without `/bin/zsh` every default spawn failed. It now prefers `$SHELL` when allowed and present, then
  the first allowed shell that exists, and names every candidate when none does.
- The `docs/tools.md` check now runs on both CI legs, since both build every engine.

### Security

- `browser_navigate` now runs the same resolved-address guard as `http_request`. With
  `browser.allowed_origins` empty the agent could point the browser at cloud metadata or a loopback
  service; every address the target resolves to must now be public unless the new
  `browser.allow_private` is on. Only `http`, `https` and `about:blank` are accepted, since `file:` walks
  past the filesystem jail and `javascript:` past the `browser_eval` opt-in.
- `browser.allowed_origins` entries are matched on the parsed origin, not as a string prefix:
  `https://ok.example` no longer admits `https://ok.example.evil`.

### Changed

- The SSRF guard moved from `mcp-net` into its own dependency-free crate, `mcp-ssrf`, so the browser
  engine can share it without one engine depending on another. `mcp-net` re-exports it unchanged.
- Driving a local development server through the browser now needs `browser.allow_private = true`.

## [0.1.0] - 2026-09-03

First tagged release. An MCP server that gives an AI agent grounded control of a real computer: 107 tools
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
- **Session and desktop.** `notify_user`, the agent's channel to a human, plus idle status, screen lock,
  media and power control, speech and audio playback.
- **CLI.** `serve`, `doctor`, `config print`, and `tools` for a generated tool reference.
- **Test support.** An in-process MCP client so tests drive the real protocol, and live task suites that
  exercise a real browser and a real desktop end to end.
- `expect` postconditions on `ui_action`, `set_value`, `keyboard_type`,
  `keyboard_shortcut` and `mouse_action`. The action runs, the condition is
  waited for, and the result carries the delta against the snapshot taken
  before it, so checking whether an action worked is one call rather than
  four. A failed expectation still returns the delta, because the action
  happened and what it did is what the agent needs to see.
- `since` on `get_ui_tree`: return what changed rather than the whole tree.
- `gone` and `focused` conditions on `wait_for`, which now shares its evaluator
  with `expect`.
- `ocr_region`: read text off the screen through Apple's Vision framework,
  returning a box per line in screen coordinates. The fallback for surfaces the
  accessibility tree does not describe, and unlike a screenshot it hands back
  coordinates that can be clicked. No longer deferred: a small Swift helper is
  compiled on first use rather than linking a framework or requiring the Xcode
  toolchain at build time.
- `find_elements`: query the accessibility tree by role, name substring or
  proximity to a screen point instead of reading all of it. On a busy
  application a targeted query is an order of magnitude smaller than the full
  tree, and the refs it returns are usable by `ui_action` because it takes and
  installs a fresh snapshot rather than querying a retained one.
- MCP tool annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`,
  `openWorldHint`) and display titles in `tools/list`, derived from the tier the
  policy gate already enforces.
- `policy.mode = "dry_run"`: a rehearsal in which read-tier tools run normally
  and anything that would change something reports what it would have done,
  including whether a human would have been asked.
- `agentctl bridge`: a reference MCP client driven by Gemini, which spawns
  `agentctl serve` as a child and runs the whole call/response loop over the
  real transport. It doubles as a schema conformance test: every descriptor is
  asserted to be a fixed point of the declaration sanitizer, so a tool that
  grows a keyword the API rejects fails in CI rather than taking every other
  declaration down with it. The API key is read from the environment, `.env` or
  `~/.agentctl/gemini.key`, never from an argument.
- `agentctl transcript --from-audit`: rebuild the same session record for a
  client that does not cooperate with us, from the audit log. Recordings in
  `docs/fixtures/` are validated against the live tool list in CI.
- `browser_disconnect`, and a shutdown hook so browsers this session launched
  are stopped on exit rather than leaked.
- `notifications/progress` on stdio, driven by `_meta.progressToken`. A wait is
  the one place this server is deliberately slow, so it is the one place
  silence is ambiguous between working and hung.
- MCP resources (`resources/list`, `resources/read`): the latest screenshot,
  the tail of the audit log, and the effective configuration with secrets
  redacted, so a person operating the client can see what the agent is working
  from without spending a turn to ask.
- MCP prompts (`prompts/list`, `prompts/get`): a cookbook for driving a GUI
  app, filling a web form, and acting-and-verifying in one step.
- `agentctl tools` and a generated tool reference at `docs/tools.md`, checked by
  CI so it cannot drift from the descriptors.

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
- **Human override.** Reaching for the mouse now stops the agent. While
  agentctl is driving, a sustained divergence between where the pointer is and
  where the server put it trips the kill switch, cancels any drag in progress,
  and posts a notification explaining how to resume. Configured under `[input]`;
  keyboard has no equivalent signal and is deliberately not covered. The kill
  switch now records *why* it engaged, and persists the reason so a restart
  does not silently resume the agent.

- Results from the tools that return third-party content now carry
  `provenance: "untrusted"`, with an advisory `suspicious_instructions` flag for
  text that reads as an instruction aimed at a model. Applied centrally so it
  cannot be forgotten or spoofed. `redteam_injection.rs` documents what the
  heuristic misses.
- The server's own state directory is denied inside any filesystem root.

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
- `expect` was declared beside the arguments rather than among them on all five
  action tools. Valid JSON, and invisible to any client that reads the schema
  to learn what a tool takes, so the act-and-confirm round trip could only be
  used by someone who had read the source.
- `list_apps` returned every process on the machine, including daemons and
  shells that `launch` and `focus_app` would never accept. It now names the
  applications that own windows.
- `list_windows` with no `app` resolved to the frontmost application, so the
  obvious opening question (what is open?) returned one app's windows, or
  none, with nothing to say it had been narrowed. It now covers the machine
  unless an app is named or `focus_app` has pinned one, and reads the
  CoreGraphics window list rather than walking accessibility trees, because
  Chromium exposes no tree until something asks it to.
- `keyboard_type` inherited latched modifier flags, so with Command held every
  character became a menu shortcut: nothing was typed and the call reported
  success.
- `list_windows` ignored its `app` argument and answered about whatever was
  frontmost.
- An accessibility traversal had no time bound. Every attribute read is an IPC
  round trip to the target application, so a busy or large app made each one
  slow: measured at 12 seconds for 58 elements from Finder. Because the
  traversal is synchronous FFI it never yields, so a `wait_for` with a one
  second deadline overran it twelvefold and nothing could interrupt it. The
  walk now has its own budget, and a tree cut short reports `partial: true`
  with advice on narrowing the scope. A partial tree and a genuinely small one
  are otherwise indistinguishable, and an agent that cannot tell concludes the
  control it needs does not exist.

### Known limitations

- Desktop control is macOS-only. The browser and commodity engines are platform-independent.
- Release binaries are unsigned and un-notarized.
- `ocr_region`, `capture_audio` and `virtual_desktop` are deferred; `privilege_run` and process-memory
  writes are deliberate omissions.
- The destructive-command gate cannot see through shell expansion, and hard links are invisible to path
  resolution. Both are documented in [`SECURITY.md`](SECURITY.md) and asserted by tests.

[Unreleased]: https://github.com/paooxide/ui-draw-mcp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/paooxide/ui-draw-mcp/releases/tag/v0.1.0
