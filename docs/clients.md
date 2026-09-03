# Connecting a client

`agentctl` speaks MCP over stdio. A client launches it as a child process, lists its tools, and calls them.
This page covers the four clients it has been set up against, plus the two things that go wrong first:
macOS permissions, and running the server from the terminal it is driving.

---

## Before any client

1. **Install it and find the path.** `which agentctl` — the client config needs an absolute path, because a
   GUI application does not inherit your shell's `PATH`.
2. **Run `agentctl doctor`.** It prints the OS, the protocol version, the kill-switch path, the audit
   directory, which config file was loaded, the enabled categories, the consent channel, and the macOS
   permission state. Everything below assumes it looks right.
3. **Decide what to enable.** The defaults are `vision`, `input` and `window`, with no dangerous tools, no
   filesystem roots, no runnable commands and no reachable hosts. Copy `config.example.toml` to
   `~/.agentctl/config.toml` and open only what the task needs.
4. **Do not run the server from the terminal it will drive.** Keystrokes go to whatever is frontmost, so a
   server driving your terminal can type into its own console. Use a separate terminal, or drive a
   different application.

---

## macOS permissions: the grant goes to the *client*, not to agentctl

This is the single most common failure, and the symptom does not point at the cause.

macOS attributes Accessibility and Screen Recording to the **responsible process** — for a child process,
that is the application that spawned it. `agentctl` is always a child of the client. So the permission must
be granted to:

| If you launch agentctl from… | Grant Accessibility to |
|---|---|
| Claude Code in Terminal.app | **Terminal** |
| Claude Code in iTerm2 | **iTerm** |
| Claude Code in VS Code's terminal | **Visual Studio Code** |
| Claude Desktop | **Claude** |
| Cursor | **Cursor** |
| `cargo test` in a terminal | that terminal |

Grant it in **System Settings → Privacy & Security → Accessibility**, then **restart the client** — the
permission is read at process start.

**Symptoms without it:** every `get_ui_tree`, `ui_action` and window tool returns `PERM_DENIED`.
`capture_screen` is worse: without Screen Recording it returns the desktop wallpaper instead of your
windows, with no error, which is why `doctor` preflights it rather than waiting for a failure.

`agentctl` never triggers the permission prompt itself — `AXIsProcessTrusted` is called in its
non-prompting form, so nothing pops a dialog behind your back. Screen Recording prompts once, on the first
capture.

To make macOS ask again after you have already answered: `tccutil reset Accessibility` (this clears the
grant for *every* application, so you will be re-prompted broadly).

---

## Claude Code

```sh
claude mcp add agentctl -- /usr/local/bin/agentctl serve
```

Add `--scope user` to make it available in every project rather than the current one. Check with
`claude mcp list`, and `/mcp` inside a session to see the tools.

Or commit it to the project as `.mcp.json`:

```json
{
  "mcpServers": {
    "agentctl": {
      "command": "/usr/local/bin/agentctl",
      "args": ["serve"],
      "env": { "AGENTCTL_CONFIG": "/Users/you/.agentctl/config.toml" }
    }
  }
}
```

Accessibility goes to the terminal application running `claude`.

---

## Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "agentctl": {
      "command": "/usr/local/bin/agentctl",
      "args": ["serve"]
    }
  }
}
```

Quit and reopen Claude Desktop. Grant Accessibility to **Claude**.

Consent dialogs appear as native macOS alerts in front of the Claude window. They are raised by `agentctl`,
not by Claude, and the model cannot see or answer them.

---

## Cursor

Project-scoped in `.cursor/mcp.json`, or global in `~/.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "agentctl": {
      "command": "/usr/local/bin/agentctl",
      "args": ["serve"]
    }
  }
}
```

Grant Accessibility to **Cursor**.

Note that Cursor is in the default `terminal_apps` list, along with VS Code, Zed, Windsurf, the JetBrains
IDEs, Xcode, Sublime and Emacs. Text typed into any of them is screened for destructive shell commands,
because an editor's integrated terminal runs the same shell and the accessibility API cannot tell an editor
pane from a terminal pane. Over-triggering on a source file that merely contains `rm -rf` is the safe
direction: the server asks, it does not refuse.

---

## Gemini CLI

Edit `~/.gemini/settings.json`:

```json
{
  "mcpServers": {
    "agentctl": {
      "command": "/usr/local/bin/agentctl",
      "args": ["serve"],
      "timeout": 60000
    }
  }
}
```

`/mcp` lists the connected tools. Tool schemas are already restricted to the JSON-Schema subset Gemini
accepts — no `pattern`, `format` or `additionalProperties` — so `tools/list` maps mechanically onto
`functionDeclarations`.

Client-side discipline worth knowing: echo model turns back verbatim so `thoughtSignature` survives
multi-turn tool use, and do not send `temperature`, `topP` or `topK`.

---

## HTTP transport (optional)

```sh
agentctl serve --http
```

The bearer token is generated and printed to stderr unless `http.token` is set in the config. Pass it as
`Authorization: Bearer <token>`.

It binds loopback only and refuses to bind anything else, rather than warning: plaintext HTTP off loopback
would put the token on the wire. Requests carrying an `Origin` header are refused by default, because any
web page can make a browser POST to `127.0.0.1` — a local port is not a private one, and a real MCP client
sends no `Origin`. Only the JSON subset of Streamable HTTP is implemented; `GET` says so rather than
leaving a client waiting on a stream that will never open.

One transport at a time. Serving both would mean an unauthenticated stdio peer and an authenticated network
peer sharing one session's consent budget and audit stream, which makes the log ambiguous about who asked
for what.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `PERM_DENIED` from every UI tool | Accessibility not granted to the client app | Grant it to the client, then restart the client |
| `capture_screen` returns the wallpaper | Screen Recording not granted | Grant it to the client, then restart |
| `POLICY_DENIED: category 'x' is not enabled` | The category is off | Add it to `policy.categories` |
| `POLICY_DENIED: dangerous tool 'x' is not enabled` | Dangerous tools need naming | Add the tool to `policy.enable` |
| `CONSENT_REQUIRED … no consent channel` | Autonomous mode has nobody to ask | Use `mode = "interactive"`, or pre-approve in `policy.enable` |
| `TIMEOUT: kill switch engaged` | `~/.agentctl/STOP` exists | Delete it |
| Tools missing from `tools/list` | Their category is disabled | `agentctl tools` shows what an agent is offered; `--all` shows everything |
| Startup fails with a config error | The config exists but does not parse | Fix it; the server refuses to fall back to wider defaults |
| The agent types into your terminal | Server driving the app it runs in | Run it from a different terminal |
| Client sees no tools at all | Wrong binary path, or `PATH` not inherited | Use an absolute path in the client config |

The audit log at `~/.agentctl/audit/<session>.jsonl` records every call with its policy decision, which is
usually the fastest way to see what the agent actually asked for.
