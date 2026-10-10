# agentctl errors: what they mean and what to tell the user

Each row says what to report. The setting named is for the user to change in their own config, if they
choose to; you never change it.

| Error | Meaning | Tell the user |
|---|---|---|
| `POLICY_DENIED: category 'x' is not enabled` | That whole area of tools is off | The task needs the `x` category; they can add it to `policy.categories` |
| `POLICY_DENIED: dangerous tool 'x' is not enabled` | A high-impact tool needs naming individually | The task needs `x`; they can add it to `policy.enable` if they accept the risk |
| `POLICY_DENIED` naming `fs.roots`, `fs.deny` or a path | The file is outside the allowed folders, or is protected | Which path was refused. Do not try a parent folder, a copy, an archive or a shell instead |
| `POLICY_DENIED` naming a host or origin | The network or browser allowlist does not include it | Which host; they can add it to the relevant allowlist |
| `CONSENT_REQUIRED … no consent channel` | The server runs unattended, so nobody can approve the action | The action needs approval; they can run it interactively or decide to allow it |
| Consent denied, or the dialog timed out | The human said no, or was not there | That it was declined. Do not ask again in a loop |
| `PERM_DENIED` from every UI tool (macOS) | Accessibility is not granted to the app that launched agentctl | Grant Accessibility to the terminal, Claude Desktop or Cursor, then restart it |
| `capture_screen` shows only the wallpaper (macOS) | Screen Recording is not granted | Grant Screen Recording to the launching app, then restart it |
| `PERM_DENIED` from input tools (Linux) | The remote-desktop portal grant has not been approved | Approve the desktop's "allow remote interaction" dialog once |
| `TIMEOUT: kill switch engaged` | The user created the STOP file | Nothing to fix: stop the task and report where you got to |
| `NOT_FOUND` with candidates | No element matched | Usually nothing: pick from the candidates or observe again |
| Tools missing from the tool list | Their category is disabled | Which capability is missing; `agentctl tools --all` lists everything |

`agentctl doctor`, run by the user in a terminal, reports the OS, permissions, the loaded config, the
enabled categories and the kill-switch state. The audit log under `~/.agentctl/audit/` records every
call with its policy decision; the user can read it, you should not.
