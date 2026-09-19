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
| Pinned-target raise-or-refuse | `mcp-macos::ensure_target_frontmost` | Keystrokes landing in the wrong window. Refuses to type if it cannot raise the intended app. |
| Frame cap | `mcp-core::DEFAULT_MAX_FRAME_BYTES` | Memory exhaustion before any policy runs. |
| HTTP: loopback bind, bearer token, `Origin` refusal | `mcp-core::http` | Network reachability, unauthenticated access, and drive-by requests from the operator's own browser. |
| Protected package set | `mcp-pkg` | Uninstalling the agent, the package manager, or security tooling. Not configurable. |
| Audit log | `mcp-policy::AuditSink` | Nothing, but it is how you find out what happened. |

Full mapping to OWASP categories is in [`architecture.md`](architecture.md) §8.

## 6. Abuse cases by category

| Category | Abuse case | Mitigation | Residual risk |
|---|---|---|---|
| vision | Screenshot a password manager; read a secure field | Secure fields are marked and their values never returned; capture needs the Screen Recording grant | A screenshot of a *visible* secret is still a screenshot. Vision is a read of the screen; that is what it is for. |
| input | Type `rm -rf /` into a terminal; click "Allow" on a system dialog | Destructive-keystroke gate on the live input target; consent dialogs are separate native windows the agent cannot see | The gate is a backstop, not containment (§7) |
| window | Close an app with unsaved work; drive an app not on the allowlist | `allowed_apps`; `close_app` is gated | Data loss inside an allowed app is possible by design |
| desktop | Speak a file's contents aloud to exfiltrate it; power off mid-task | `play_audio` is bounded by the filesystem roots; `power_control` is dangerous-tier | `notify_user` text is attacker-controlled and could mimic a system prompt (§7) |
| browser | Navigate to cloud metadata; read cookies; `eval` arbitrary JS | `browser_eval`/`browser_cookies`/`browser_network` are dangerous-tier; cookie values redacted; JS dialogs dismissed by default | `browser_navigate` is origin-prefix checked, **not** IP-guarded (§7) |
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

**On Linux the override has no sensor at all.**
Wayland gives a client no way to read the pointer's position, so `pointer_position` answers `None`, which
the watcher treats as no evidence rather than as interference. Reaching for the mouse does not stop the
agent on Linux. The STOP file, the consent dialog and the portal grant (revocable from the desktop's
settings) are the controls.

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
