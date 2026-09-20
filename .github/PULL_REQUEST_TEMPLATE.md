<!-- Thanks for contributing. Keep the subject imperative and explain *why* in the body. -->

## What and why

<!-- What does this change do, and what problem or failure prompted it? The diff shows the what;
     say the why. If it fixes an issue, link it (e.g. Closes #123). -->

## Definition of done

- [ ] `cargo fmt --all --check` is clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean
- [ ] `AGENTCTL_SKIP_LIVE=1 cargo test --workspace` passes
- [ ] New behaviour has a test (pure logic as a free function; anything touching an OS against the real one — no fake backends)
- [ ] `docs/tools.md` regenerated if any tool descriptor changed (`cargo run -q -p agentctl -- tools --markdown --all > docs/tools.md`)
- [ ] `CHANGELOG.md` updated under `Unreleased` if behaviour changed
- [ ] New tool schema stays in the Gemini-safe subset (no `pattern`, `format`, `additionalProperties`)

## Security tier

<!-- Required if this touches a dangerous-tier tool, mcp-policy/mcp-sec, the fs jail, SSRF guard,
     destructive-command gate, consent channel, kill switch, redaction, the audit sink, or
     Server::dispatch_call. Say what you considered. Delete this section if none apply. -->
