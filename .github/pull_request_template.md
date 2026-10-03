## What and why

<!-- What changed, and what problem it solves. If a real failure prompted it, describe the failure. -->

## Checks

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `AGENTCTL_SKIP_LIVE=1 cargo test --workspace`
- [ ] New behaviour has a test (pure logic as a free function; OS behaviour against the real OS)
- [ ] `docs/tools.md` regenerated if any tool descriptor changed
- [ ] `CHANGELOG.md` updated under `Unreleased` if behaviour changed; `config.example.toml` if an operator can set it
- [ ] New tool schemas stay in the Gemini-safe subset (no `pattern`/`format`/`additionalProperties`)

## Security tier touched?

- [ ] **No** — this change adds no dangerous tool and touches no gate, jail, guard, consent path, kill
      switch, redactor, audit sink, or `dispatch_call`.
- [ ] **Yes** — describe what was considered below.

<!--
If yes: which control is affected, what an adversarial agent could now attempt that it could not before,
and which test pins the new boundary. Update the relevant documented_known_bypasses test if the limits of
a heuristic changed.
-->

## Live verification

<!-- Which live suites were run, on what. "AGENTCTL_LIVE_GUI=1 cargo test -p agentctl --test live_desktop
     on macOS 15, Accessibility granted" beats "tested locally". -->
