# Security

## What this software is

`agentctl` hands a remote AI agent structured control of a real computer: keyboard, mouse, windows, files,
processes, network and a browser. It is designed on the assumption that the driving agent may be
**prompt-injected or outright adversarial**, because a model that reads a web page, a document or a
terminal buffer is reading text an attacker may have written.

The security of the system therefore does not rest on the model behaving well. It rests on the policy
layer, which is the only path to an engine.

## Running it responsibly

- **Run as an ordinary user. Never as root.** `privilege_run` is deliberately not implemented: root defeats
  every other control in the system.
- **Open only what the task needs.** Every engine is closed by default: no filesystem roots, no runnable
  commands, no reachable hosts, no keychain services. Widen deliberately, and narrow again afterwards.
- **Prefer `interactive` mode when a human is present.** In `autonomous` mode there is nobody to ask, so
  anything requiring consent is denied rather than allowed. Safe, but it means unattended runs cannot do
  high-impact work at all, which is the intended trade.
- **Run it from a different terminal than it drives.** Synthetic keystrokes go to whatever is frontmost.
- **Treat the audit log as evidence.** `~/.agentctl/audit/<session>.jsonl` records every call, before and
  after, with the policy decision. It is the record of what the agent actually did.
- **Know where the brake is.** Creating `~/.agentctl/STOP` aborts in-flight work and is checked before
  every policy decision.

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x | Yes |
| < 0.1 | No |

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Use GitHub's private vulnerability reporting on this repository: **Security → Report a vulnerability**.
Include the version or commit, your configuration (with secrets removed), and the smallest reproduction you
have.

Expect an acknowledgement within a few days and an assessment of severity and fix timeline after that.
There is no bounty programme. Credit is given in the changelog unless you would rather not be named.

## What counts as a vulnerability

Anything that lets a driving agent, a web page or a local process reach past a control the server claims to
provide:

- **Policy bypass**: reaching an engine whose category is disabled, or a dangerous tool not named in
  `policy.enable`, or executing without the consent that a tool declared it needed.
- **Containment escape**: reading or writing outside the configured filesystem roots, reaching a denied
  credential path, or getting an SSRF-guarded request to a private or metadata address.
- **Secret leakage**: any path that returns a plaintext credential, an unredacted secure text field, or a
  cookie value to the agent or into the audit log.
- **Consent bypass**: anything that lets the agent see, pre-fill, suppress or answer its own consent
  prompt, or that makes a dialog misrepresent what is being approved.
- **Kill-switch or audit tampering**: anything that lets the agent disable the STOP file, or write,
  truncate or forge audit records.
- **Transport**: authentication bypass on the HTTP transport, request smuggling, or a resource-exhaustion
  path that runs before the policy gate.

## What is a known limitation, not a vulnerability

These are documented, asserted by tests that *prove the gap still exists*, and covered in
[`docs/threat-model.md`](docs/threat-model.md). Reports of them are welcome as improvements, but they are
not treated as new findings:

- **The destructive-command gate is a backstop, not containment.** It matches normalised text and cannot
  see through shell expansion: `base64 -d | sh`, `$IFS` padding, variable indirection. Seeing through
  those means running the shell, which is the thing being gated. The real control is the argv allowlist
  with `allow_shell = false`.
- **Hard links are invisible to path resolution.** A hard link has no target to follow, so a link created
  inside a root by some other actor resolves as in-root. The engine exposes no hard-link primitive.
- **`browser_navigate` is origin-prefix checked, not IP-guarded.** Restrict it with
  `browser.allowed_origins`; a browser is a general-purpose network client.
- **Anything granted is genuinely granted.** Enabling `terminal` with `allow_shell = true`, or naming
  `browser_eval` in `policy.enable`, is a decision to allow arbitrary code execution. The server will do
  what it was configured to do.
- **A compromised MCP client is game over.** It owns the stdio pipe and holds the operating system
  permissions. The trust boundary is around the client, not inside it.
