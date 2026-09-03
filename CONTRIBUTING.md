# Contributing

## Build and check

```sh
cargo build --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
AGENTCTL_SKIP_LIVE=1 cargo test --workspace
```

All four must be clean before a change is proposed. CI runs them on macOS and Linux, plus the hardening
suites and `cargo deny`.

## Definition of done

A change is finished when:

- `fmt`, `clippy -D warnings` and the full test suite pass.
- New behaviour has a test. Pure logic is tested as a free function; anything touching an OS is tested
  against the real one.
- A new or changed tool schema stays inside the Gemini-safe JSON-Schema subset: no `pattern`, no `format`,
  no `additionalProperties`.
- `docs/tools.md` is regenerated if any descriptor changed:
  `cargo run -q -p agentctl -- tools --markdown --all > docs/tools.md`. CI diffs it.
- Anything an operator must know is in `config.example.toml`, and anything a future maintainer must know is
  in the commit message and `CHANGELOG.md`.

## No fake backends

This is the rule that shapes the test suite. The product's whole value is real GUI and browser control, so
a stubbed backend gives false confidence: it proves the code calls the API, not that the API does what was
assumed. Every bug found in this repository so far (the missing focused-application fallback, the unset
mouse click-state, inherited modifier flags, a discarded `app` argument) passed a plausible unit test and
failed on a real machine.

So: test pure logic as free functions, and test everything else against the real thing. Where a real check
needs a permission that may not be granted, assert that the call *returns an answer* (success or
`PERM_DENIED`) rather than faking success.

Live tests are gated. `AGENTCTL_SKIP_LIVE=1` skips those that drive a real browser, package manager or
desktop. GUI suites additionally require `AGENTCTL_LIVE_GUI=1`, because they steal window focus and
synthesise keystrokes. A plain `cargo test` on a machine somebody is using must never do that. Guard on
the frontmost application before every synthetic keystroke, and act on a scratch document rather than the
developer's own work.

## Security tier touched?

Any of the following needs an explicit security review pass on the change, and a note in the pull request
saying what was considered:

- Adding a `dangerous`-tier tool, or moving an existing tool to a lower tier.
- Any change under `crates/mcp-policy` or `crates/mcp-sec`.
- Any change to the filesystem jail, the SSRF guard, the destructive-command gate, the consent channel, the
  kill switch, redaction, or the audit sink.
- Any change to `Server::dispatch_call`, which is the single path to an engine.

When a change alters what a heuristic can catch, update the corresponding `documented_known_bypasses` test.
Those tests assert that a gap *still exists*, so the limits of a heuristic stay visible instead of being
quietly mistaken for containment.

## Commit messages

Imperative subject line, then a body that explains **why** rather than what: the diff already says what.
When a change was prompted by a real failure, say what the failure was and how it showed up; that is the
part nobody can reconstruct later.

No trailer block. The message ends on its last content line.

## Releasing

1. Update `CHANGELOG.md`: move `Unreleased` entries under the new version.
2. Bump `version` in the workspace `Cargo.toml`, and commit the re-synced `Cargo.lock`.
3. Tag `vX.Y.Z` and push the tag. The release workflow builds per-platform archives, writes `SHA256SUMS`,
   and opens a draft release.
4. Check the asset list, then publish the draft.
