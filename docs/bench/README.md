# Latency benchmark

A reproducible measurement of how long `agentctl serve` takes to answer a handful of tool calls. It replaces
an earlier ad-hoc script (3 to 5 samples, one session, no warm-up, no variance, a hard-coded "cloud" column).
Every number here comes from calls the bench made during that run, and anyone can rerun it.

## Run it

```sh
cargo run --release --example bench -- --n 30
```

Flags: `--n` samples per operation (default 30, minimum 2), `--warmup` calls discarded first (default 5),
`--out-dir` where results go (default `docs/bench/results`), `--bin` to measure a specific `agentctl` binary.

Without `--bin` the bench first runs `cargo build -p agentctl --bin agentctl` with the same profile as itself,
because `cargo run --example` does not build the package's binary and a stale one would silently be measured.
Use `--release`: the server is what is being timed, and a debug build is several times slower.

It needs a Chrome or Chromium that `browser_connect` can launch (the same search list the server uses). The
display operations need a logged-in desktop session and, on macOS, Screen Recording permission for the
terminal you run it from.

`AGENTCTL_LIVE_GUI=1` adds `mouse_action` pointer movement. That moves your real cursor, so it is off unless
you ask for it and must not run unattended. The bench is an example, not a test: `cargo test` never runs it.

Output, named `<date>-<os>-<short-hash>`, in `docs/bench/results/`:

- `.json`: raw per-call samples (client and server), the summary statistics, and the environment (git commit
  and whether tracked files were modified, OS and version, CPU, display resolution, `list_displays` output,
  Chrome version, rustc).
- `.md`: the table.

## What is measured

The bench writes a temporary config (passed through `AGENTCTL_CONFIG`) per session, with
`policy.mode = "autonomous"` so nothing can prompt, an audit directory in a temp dir, and only the categories
that session needs:

| session | categories | operations |
|---|---|---|
| native | vision, window | `ping`, `list_windows`, `capture_screen`, `ocr_region` |
| browser | browser (loopback allowed) | `browser_snapshot`, `browser_act` |
| gui (opt-in) | input | `mouse_action` move |

Each session starts its own `agentctl serve`, does the MCP `initialize` handshake, runs each operation for
`--warmup` discarded calls and then `--n` timed calls, and shuts the server down.

| operation | exact call |
|---|---|
| `ping` | JSON-RPC method `ping` |
| `list_windows` | `list_windows {}` |
| `capture_screen` | `capture_screen {"force": true}`, default detail tier |
| `ocr_region` | `ocr_region` on region x=0 y=0 w=400 h=100 |
| `browser_snapshot` | `browser_snapshot {mode: "dom"}` on a fixed page the bench serves from 127.0.0.1 |
| `browser_act` | `browser_act` click on `#go` on that page |
| `mouse_action_move` | `mouse_action {type: "move"}` alternating between two points 40 px apart |

`capture_screen` passes `force: true`. Without it the server compares each frame to the last and, when they
match, returns `unchanged` instead of re-sending the image, so repeated samples would mostly measure the dedup
path rather than a capture.

Results are written exactly as observed. If an operation fails, for example because Screen Recording or
Accessibility permission is missing, it is recorded as `"skipped": "<reason>"` with the server's error code,
and the run continues. There is no fallback value.

## What is inside each timed region

Two clocks run on every call.

**Client round trip (`client_ms`).** A monotonic clock starts just before the request line is written to the
child's stdin and stops when the response line has been read from its stdout. Inside: pipe write and read, the
server's JSON-RPC parsing, the policy gate, the tool itself, result serialisation (including a base64 PNG for
`capture_screen`) and the scheduler hops between the two processes. Outside: building the request JSON,
parsing the response, process start-up and the `initialize` handshake.

**Server side (`server_ms`).** The `latency_ms` field on each `post` record in the session's audit JSONL. The
bench reads the file after the server exits and joins records to calls by position, checking the tool name at
each position. If the counts or names do not line up, the server column is `null` with a note rather than a
guess. Inside: the gate decision and the tool. Outside: pipe I/O and the JSON-RPC layer around it. Two caveats:
`latency_ms` is a whole number of milliseconds, so anything under a millisecond reads 0 and the difference
between the two columns carries up to a millisecond of rounding; and `ping` is a protocol method the server
does not audit, so it has a client column only.

The gap between the two is a rough measure of IPC and framing overhead, not an exact one.

## Statistics

For each column: min, median, p95, max and the sample standard deviation (n-1), in milliseconds. p95 is the
nearest-rank value, so at n=30 it is the second-largest sample. Treat it as "about the worst case seen", not
a stable tail estimate. Look at the raw samples in the JSON when a median and p95 disagree sharply.

## Warm versus cold

These are warm numbers. Each operation first makes 5 discarded calls, so the first-call costs are excluded:
lazy initialisation inside the server, OS caches, the browser's first attach and paint, and OCR model load.
Each session also runs against a server that has already started. Cold-start cost (spawning the server,
launching Chrome, the first capture after a permission grant) is real and matters to a user, but it is a
different quantity. To see it, run with `--warmup 0 --n 2` and read the raw samples, understanding that the
first sample is then the only cold one.

Other things that move the numbers: what is on screen (capture and OCR cost track pixel area and content),
how many windows are open (`list_windows`), machine load, power state and thermal throttling. The environment
block records what it can; it does not record load. Compare runs only on the same machine in the same state.

## Why this is not comparable to a cloud model turn

These are the costs of tool calls on a local machine. A model-driven step also includes sending the context
to a hosted model, queueing and generating tokens, network round trips, and the model deciding what to do,
and those usually dwarf the numbers here. They vary with model, prompt size, provider load and region, none
of which this bench controls or observes. The bench has no comparison column and will not grow one from
constants.

A fair comparison needs the same task run end to end, against a real hosted model, over many runs. This
bench does not do that.

## Adding an operation

1. In `agentctl/examples/bench.rs`, add a `(name, Call::Tool("tool_name", json!({...})))` to the plan of the
   session whose categories it needs. If it needs a category that no session enables, add a session, or add
   the category to an existing one only if the operation belongs with it.
2. If it needs setup that must not be timed (navigating a page, creating a file), do it with
   `setup`-style calls before the plan, as `browser_session` does. Those calls are in the audit log and are
   accounted for when the server column is joined.
3. If it is read-only, use fixed arguments and keep the fixture local and deterministic. If it moves the
   pointer, types, or otherwise touches the real desktop, gate it behind `AGENTCTL_LIVE_GUI=1` as
   `mouse_action_move` is.
4. Add the name to `OP_ORDER` so it lands in the right table row.
5. Add a row to the operation table above.
6. Run it and check the JSON: failures must appear as `skipped` with an error code, and the sample count
   must equal `--n`.

## Reading a result

Open the `.md` for the table and the `.json` for everything. A results file is one run on one machine; it is
evidence of what that run saw, not a property of the software. When quoting a number, quote the date, commit,
machine, and `n`, and say which column it comes from.
