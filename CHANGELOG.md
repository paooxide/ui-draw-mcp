# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Reproducible latency bench** (`cargo run --release --example bench -- --n 30`, see `docs/bench/`).
  It starts `agentctl serve` over stdio under a temporary config and times `ping`, `list_windows`,
  `capture_screen`, `ocr_region`, and `browser_snapshot` and `browser_act` against a local fixture page, after
  5 discarded warm-up calls, reporting min, median, p95, max and standard deviation. Each call is timed twice:
  the client round trip, and the server's own `latency_ms` read back from the audit log. Raw samples, the
  commit, OS, CPU, display and Chrome version go to `docs/bench/results/`. An operation that fails (a missing
  Screen Recording grant, say) is recorded as skipped with the server's error code, never filled in. Pointer
  movement only runs under `AGENTCTL_LIVE_GUI=1`. There is no cloud comparison: that needs the same task run
  end to end against a hosted model.
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
- **Role profiles and argument invariants.** `policy.role` (built in: `readonly`, `qa`, `operator`,
  `admin`, or one defined under `[roles.<name>]`) narrows the visible tools by category, tier or name,
  can require consent, and can lower the denial budget; a role never widens anything. `[invariants]`
  refuses protected paths and denied domains anywhere in a call's arguments before the gate, including
  inside command lines, in `file:` URLs and through symlinks. It is a check on argument text, not
  containment; relative paths, shell expansion, encoding and DNS are documented as out of reach.
- **Compound native forms and extraction.** `ui_fill_form` sets text fields, checkboxes, switches and
  pop-ups in one call and can submit and check the result; `ui_extract` reads a native table, form or list
  into JSON.
- **Demo mode** (`policy.demo`, `--demo`, `--demo-speed cinematic|demo|snappy|instant`), for screencasts:
  the pointer glides along an eased curve, and browser pages show a pointer, click ripples and a typing
  label that masks secret fields. Off by default. A glide stops at the next waypoint when a human takes the
  mouse or the call is cancelled.
- **Browser forms, extraction and profiles.** `browser_fill_form` fills many fields and optionally submits
  in one call; `browser_extract` reads schema-shaped records from a page; `browser_profile` saves and
  restores cookies and storage, and `browser_connect` can start from a profile. `browser_wait` gains
  `dom_settled`, and `browser_assert` can wait for it first.
- **Branches and checkpoints.** `browser_branch` tries a path in a separate browser context seeded with the
  page's cookies and storage, then commits it to the visible tab or discards it (at most 8 at once,
  `AGENTCTL_MAX_BRANCHES`; their tabs are closed on discard and at shutdown). `browser_checkpoint` saves
  and rolls back URL, form fields, storage and cookies, and names any field it could not restore.
- **Scoped acting and htmx.** `browser_act` can resolve its target inside a CSS or XPath container, filter
  by text and pick by index; a container that matches nothing is an error. `browser_act` also gains
  `press` (Enter, Escape, Tab as real key events) and `secret`. `browser_wait htmx_settled` follows
  htmx's request and settle events.
- **Shadow DOM and canvas regions.** Snapshots descend into open shadow roots. A canvas that publishes its
  interactive regions (`__agentctl_regions` or `data-canvas-regions`) gets child nodes that are clicked
  with real mouse input; other canvases stay opaque.
- **Challenge handshake.** `browser_challenge` detects a CAPTCHA or one-time-code prompt, shows an overlay
  and waits for a person to clear it. It never tries to solve one.
- **Recording.** `browser_record` (and `agentctl record`, which launches its own browser unless given
  `--attach <port>`) captures clicks, typing, key presses and navigations into a `browser_flow` that
  replays. Password, PIN, one-time-code, card, token and API-key fields are recorded as a named
  `secret_ref`; their values are supplied at replay time (`browser_flow run` `secrets`, or
  `AGENTCTL_SECRET_<REF>` for `agentctl test`) and never stored or logged. While a recording runs, the
  tab's JavaScript dialogs are answered as `dialogs` says. A person recording by hand answers their own
  `confirm()`: in a visible browser the default is `human`, where the recorder only listens (Chrome shows
  the dialog natively and also announces it to the Page-domain client), and how the person answered
  becomes a `dialog` step. `accept` and `dismiss` have the recorder answer; a headless browser has nobody
  to answer, so `dismiss` (or the tab's `browser_dialog` policy) is its default and `human` is refused
  rather than left to hang the tab. A `dialog` step sets the tab's standing policy, so it is placed
  before the click, typing or key press that raised the dialog; alerts are not recorded (they have one
  way out) and a prompt's typed text is not kept, since it may be a secret, so replay accepts it empty.
  Recording with `dialogs: "accept"`, and running a flow with an accepting dialog step, ask for the same
  consent as `browser_dialog policy: "accept"`.
- **Safari, experimental.** `browser_connect` takes `launch.browser = "safari"` and drives Safari through
  `safaridriver`'s W3C WebDriver. Operations WebDriver cannot do (key presses, device emulation, branching,
  checkpoints, recording, network capture) return `UNSUPPORTED`. Needs a one-time `safaridriver --enable`;
  its live suite runs with `AGENTCTL_LIVE_SAFARI=1`.
- Release archives are also built for Linux on ARM64.

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
  locations (the engine speaks CDP, so Firefox remains unsupported; Safari is experimental, below). Each launch takes its own
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

- A running call could not be stopped: the per-call cancel token was never tripped, so the kill switch
  and a human taking the mouse only blocked the next call. A running call now checks the kill switch every
  50 ms and cancels its token, and `notifications/cancelled` reaches the call it names; the stdio loop
  keeps reading during a call so the cancel can arrive.
- `browser_wait navigation` returned on the page being left when a click started its navigation a moment
  later (a timer, a debounce), because that page still reports `complete`. It now waits for a loaded
  document that is not the one the action left, and a click that navigates nowhere within 2 s settles
  with `navigated: false`.
- `browser_wait navigation` gave up on a click whose handler navigated later than the fixed 2 s window
  (a slow analytics call before `location` changes), settling on the old page with `navigated: false`
  while the new one was still coming. `navigation_timeout_ms` (0 to 30000, default 2000) sets the window
  per call, and a `wait` flow step takes it too. `timeout_ms` still bounds the whole wait.
- `browser_navigate back` and `forward` did not mark the document they left the way `goto` and `reload`
  do, so a `wait navigation` after them had only the old page's `readyState` to go on. They now plant the
  marker, so the wait is for the history entry's page. An entry made by `history.pushState` or a fragment
  change keeps the document, and Chrome says so, so the marker is dropped and the wait settles at once
  instead of running out its timeout.
- `browser_checkpoint rollback` could succeed from Chrome's HTTP cache. It navigates to the saved URL, and
  for a page served with `max-age` Chrome answered from disk: with the server down the rollback reported
  `rolled_back: true` for a page nobody had fetched, and with the server up it could restore form state
  into an out-of-date copy. The load now bypasses the HTTP cache (for that load only), so an unreachable
  server fails the rollback with the navigation error and a changed page is the one you see. The result
  carries `cache_bypassed`, true when the rollback navigated. A rollback that finds the tab already on the
  checkpoint's URL loads nothing, as before.
- The CDP client could lose half a WebSocket frame when a read was cancelled by a timeout, corrupting the
  rest of the stream. Reads are now buffered and cancel-safe.
- `pty_spawn` with no `shell` picked the first allowed shell whether or not it existed, so on a machine
  without `/bin/zsh` every default spawn failed. It now prefers `$SHELL` when allowed and present, then
  the first allowed shell that exists, and names every candidate when none does.
- The `docs/tools.md` check now runs on both CI legs, since both build every engine.
- The HTTP transport could not be relied on to deliver `notifications/cancelled`, and it dropped a call
  that outlived the read timeout. Connections are now served concurrently, up to `max_connections` (64;
  the next one gets a 503 rather than queueing), each with the full origin, token and size checks, so a
  cancel POST reaches a `tools/call` POST that is still running. The 30 s read timeout now bounds only
  receiving the request, not the tool call.
- A request the client cancelled with `notifications/cancelled` was still answered. The MCP spec says
  the receiver should not respond, so neither transport now writes a reply for it (HTTP returns an empty
  `202`); the audit post-record is still written, including for a call cancelled before it started. A call
  stopped by the kill switch is still answered, since the client is still waiting.
- Human takeover detection was silently off on every Linux session. `pointer_position` always answered
  `None`, and the watcher said so only at info level. On X11 it now reads the pointer through `xdotool`
  and the watcher runs. Where that is impossible (Wayland, including XWayland, which only sees the
  pointer over X11 windows; no `DISPLAY`; no `xdotool`) it logs at warn level at startup, and `agentctl
  doctor` shows `human takeover` with the reason. Not tested on a live X11 session.
- On macOS, `keyboard_type` posted a whole string on one key event, and Apple documents that only the first
  20 UTF-16 code units of a string set on one event are used, so long text could be cut while the call
  reported the full count. Text now goes out in events of at most 20 units, never splitting a surrogate
  pair and, as far as a block-based table of combining marks, joiners, skin tones and flags allows, never
  a grapheme cluster. A takeover between pieces stops the typing and says how much was typed. The 200
  character live test is written but has not been run.
- `--demo-speed` accepted any string and quietly ran at the default speed, while the config file and the
  environment refused an unknown one. An unknown or missing value is now a usage error (exit 2) naming
  `cinematic, demo, snappy, instant, off`, and the browser showcase speed comes from the same preset as
  the pointer glide instead of a second copy of the mapping.

### Security

- **Browser snapshot `semantic_intent` and `bound_state` are sanitized and bounded.** Both come from the page
  (`data-intent`, `data-state`, React props, canvas region fields), so a hostile page could put quotes,
  newlines or a forged `@e9` line in an intent, or hand over a cyclic or 100 KB state. An intent is now
  reduced to `[A-Za-z0-9_.-]`, 48 characters (the rule the OS snapshot already used). `bound_state` goes
  through an injected serializer (depth 4, 20 keys or items, 200-character strings, cycles, functions and DOM
  nodes dropped) and a 2 KiB cap in Rust that replaces a larger value with `{"truncated":true,"bytes":N}`.
  Chrome, Safari and canvas regions share the Rust step. macOS and Linux nodes still carry neither field.
- **A page cannot forge recorded steps.** The recorder and its `__agentctl_rec` binding lived in the page's
  main world, so page script could call the binding and add clicks or typing that nobody made to a
  recording, or replace the function to read what was typed. They now live in an isolated world: the DOM
  is shared, so the recorder still sees real clicks and typing, but the binding exists only there and the
  page cannot reach it or the recorder's state. Page script can still dispatch synthetic DOM events, which
  the recorder sees like the agent's own `browser_act` clicks.
- **Config values are checked, not guessed.** A boolean was read as `value == "true"`, so
  `human_override = "yes"` or `"True"` silently turned the human-takeover stop off, and a known setting
  with the wrong type (`max_denials = "5"`) was dropped as an unknown key. Booleans must be `true` or
  `false`, and a known key of the wrong type is an error; only keys the loader does not know are
  tolerated.
- **Accessibility snapshots escape page text.** Element names and values were written into the
  snapshot's quoted fields verbatim, so a control named `Cancel"`, a newline and `@e9 button "Approve`
  showed the model a line for an element that does not exist. Quotes, backslashes and control characters
  are now escaped.
- **`browser_tabs open` runs the navigation policy.** Chrome loads the URL as it creates the tab, so
  opening a tab at a URL bypassed `browser.allowed_origins` and the private-address check that
  `browser_navigate` applies.
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
