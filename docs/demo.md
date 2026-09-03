# Driving agentctl with a real model

The test suite proves the protocol works. It does not prove a model can read
107 tool descriptions and get something done with them — and that turns out to
be a different question, which is why this exists. Two routes, one transcript
format, so the results are comparable.

## Route A — the built-in Gemini bridge

`agentctl bridge` is a complete MCP client. It spawns `agentctl serve` as a
child process, hands the tool list to Gemini as function declarations, and runs
the call/response cycle until the task is done.

### The key

Never on the command line — an argument is visible in `ps` to every process on
the machine, and this server hands an agent a process list. Three places are
read, in order:

1. `GEMINI_API_KEY` in the environment
2. `./.env`, as `GEMINI_API_KEY=...`
3. `~/.agentctl/gemini.key`

The two files must be `chmod 600`; a wider mode is refused with the command to
fix it. `.env` and `*.key` are in `.gitignore`.

```sh
printf 'GEMINI_API_KEY=%s\n' "$KEY" > .env && chmod 600 .env
```

### Running it

```sh
agentctl bridge --list-models          # what this key can actually drive
agentctl bridge --task "What application windows are open right now?"
```

| Flag | Default | |
|---|---|---|
| `--task` | required | what to do |
| `--model` | `gemini-2.5-flash` | any name `--list-models` reports; `GEMINI_MODEL` also works |
| `--max-turns` | 12 | model round trips before it gives up |
| `--mode` | `AUTO` | `ANY` forces a tool call every turn |
| `--thinking-level` | unset | `low`/`high` on models that accept it |
| `--config` | operator's | a config file for the spawned server, so a demo need not touch `~/.agentctl/config.toml` |
| `--record` | — | write the transcript here |
| `--system` | built-in | replace the system instruction |

Tool calls are traced to stderr as they happen (`→` the call, `←` what came
back), so a run is watchable rather than a wait followed by a verdict.

### What it is worth beyond the demo

The declarations are built by a sanitizer that every tool descriptor is
asserted to be a fixed point of (`agentctl/tests/bridge_contract.rs`). Gemini
does not reject one bad declaration — it rejects the request, so a single tool
that grows a `pattern` or an `additionalProperties` takes all 107 down with it.
That failure now happens in CI instead of in front of whoever is running the
demo.

Two real defects came out of the first two runs, both of which every test in
the workspace had passed over:

- `expect` was declared *beside* the arguments instead of among them, on all
  five action tools. Valid JSON, and invisible: act-and-confirm could only be
  used by someone who had read the source.
- `list_apps` returned every process on the machine and `list_windows` with no
  app returned only the focused one, so "what is open?" cost sixteen calls of
  guessing application names and still ran out of turns. It is one call now.

## Route B — Claude Code, Cursor, or any other client

These drive the server without cooperating with us, which is the point: nothing
about the recording depends on the client.

```sh
claude mcp add agentctl -- /path/to/agentctl serve
```

Then give it the task in the client. Afterwards, rebuild the session record
from the audit log — every call is already there, with its redacted arguments,
the policy's decision, and the latency:

```sh
agentctl transcript --from-audit ~/.agentctl/audit/sess-<pid>-<ts>.jsonl \
    --client claude-code --task "write a note in TextEdit" \
    --out docs/fixtures/claude-code.json
```

`docs/clients.md` covers configuration for Claude Desktop, Cursor and the
Gemini CLI.

## The transcript format

`agentctl-demo-transcript/1`: client, model, task, and a turn per message —
`user`, `model`, or `tool` with its arguments, the policy decision, and a
bounded excerpt of the result. Anything in `docs/fixtures/` is checked in CI
against the live descriptor list, so a recording that names a tool we have
since renamed fails the build instead of quietly misleading a reader.

Two fixtures describe the *same* browser session, produced independently:
`gemini-bridge.json` recorded live by the bridge, and `from-audit.json`
reconstructed afterwards from `~/.agentctl/audit/<session>.jsonl`. Their tool
sequences are identical, which is the evidence that Route B needs no
cooperation from the client — and the reconstruction carries something the live
recording cannot, the policy decision on each call.

`docs/fixtures/gemini-bridge.json` is a real run: connect a headless browser,
fill a form, read the result back off the page, and disconnect. It includes a
`POLICY_DENIED` — the model reached for `browser_eval`, which is Dangerous-tier
and not enabled, and carried on without it. That is what the gate looks like
from the agent's side, and it is worth leaving in.
