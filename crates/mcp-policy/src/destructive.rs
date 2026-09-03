//! Heuristic destructive-command detector for text typed/pasted into a terminal.
//! This is a **backstop, not a security boundary** (`docs/architecture.md` §7.2):
//! it catches obvious footguns; the real containment is the tier/category gate.
//!
//! Matching happens on a *normalised* form of the command — lowercased, with
//! every run of whitespace collapsed to a single space. Without that step the
//! detector is trivially evaded by pressing the space bar twice, which is worse
//! than having no detector at all, because it still reads like protection.
//!
//! What it cannot do is see through the shell's own expansion (`$IFS`, `base64
//! -d | sh`, variable indirection). Doing so would mean running the shell,
//! which is the exact thing being gated. `redteam_destructive.rs` pins those
//! bypasses in an explicit known-gaps test so the limit stays visible.

/// Default seed patterns (lowercased substrings). Kept
/// in config so the list can grow without a code change.
pub fn default_destructive_patterns() -> Vec<String> {
    [
        // Recursive deletion, short and long spellings.
        "rm -rf",
        "rm -r ",
        "rm -fr",
        "rm --recursive",
        "rm --force",
        "--no-preserve-root",
        // Privilege and filesystem-level destruction.
        "sudo ",
        "mkfs",
        "dd if=",
        "diskutil erase",
        "> /dev/",
        // Irreversible VCS operations.
        "push --force",
        "push -f",
        "reset --hard",
        // Ownership/permission sweeps.
        "chmod -r",
        "chown -r",
        // Turning off the platform's own protections.
        "csrutil disable",
        "spctl --master-disable",
        // Covering tracks: the audit trail is what this system rests on.
        "history -c",
        ".bash_history",
        ".zsh_history",
        // Assorted classics.
        ":(){", // fork bomb head
        "mv ~ /dev/null",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Commands that run the *next* word rather than doing anything themselves.
/// `curl x | sudo bash` must be read as a pipe into `bash`, not into `sudo`.
const WRAPPERS: &[&str] = &["sudo", "doas", "env", "nohup", "exec", "time", "command"];

/// Interpreters that turn piped bytes into execution.
const INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "ash",
    "pwsh",
    "python",
    "perl",
    "ruby",
    "node",
    "php",
    "osascript",
];

/// Commands that pull bytes off the network.
const FETCHERS: &[&str] = &["curl", "wget", "fetch"];

/// Lowercase and collapse whitespace, then pad with a single leading/trailing
/// space so a pattern written with word boundaries (`"sudo "`) still matches a
/// command that ends on that word.
fn normalize_haystack(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push(' ');
    let mut prev_space = true;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.extend(ch.to_lowercase());
            prev_space = false;
        }
    }
    if !prev_space {
        out.push(' ');
    }
    out
}

/// Patterns get the same lowercasing and whitespace collapsing, but no padding:
/// a pattern like `"> /dev/"` is a fragment, not a whole word.
fn normalize_pattern(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len());
    let mut prev_space = false;
    for ch in pat.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.extend(ch.to_lowercase());
            prev_space = false;
        }
    }
    out
}

/// Strip any directory prefix and trailing version digits: `/usr/bin/python3.12`
/// and `python` are the same interpreter for our purposes.
fn is_interpreter(token: &str) -> bool {
    let base = token.rsplit('/').next().unwrap_or(token);
    if INTERPRETERS.contains(&base) {
        return true;
    }
    let stem = base.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    stem != base && INTERPRETERS.contains(&stem)
}

fn is_wrapper(token: &str) -> bool {
    let base = token.rsplit('/').next().unwrap_or(token);
    WRAPPERS.contains(&base)
}

/// Does any command word invoke a privilege-elevation helper? Checked on split
/// tokens rather than as a substring so `grep -r 'sudo' .` stays unflagged
/// while `foo&&sudo rm x` does not.
fn elevates_privilege(norm: &str) -> bool {
    norm.split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '`'))
        .filter(|t| !t.is_empty())
        .any(|t| {
            let base = t.rsplit('/').next().unwrap_or(t);
            base == "sudo" || base == "doas"
        })
}

/// Is the right-hand side of any pipe an interpreter? Skips over wrapper
/// commands, so `| sudo bash` and `| /usr/bin/env python3` both count.
fn pipes_to_interpreter(norm: &str) -> bool {
    let bytes = norm.as_bytes();
    let mut cursor = 0;
    while let Some(offset) = norm[cursor..].find('|') {
        let pipe_at = cursor + offset;
        cursor = pipe_at + 1;
        // `||` is a logical or, not a pipe.
        if bytes.get(pipe_at + 1) == Some(&b'|') {
            cursor = pipe_at + 2;
            continue;
        }
        let mut scan = pipe_at + 1;
        // Walk at most a few wrapper words before giving up on this pipe.
        for _ in 0..3 {
            while bytes.get(scan) == Some(&b' ') {
                scan += 1;
            }
            let start = scan;
            while scan < bytes.len() && bytes[scan] != b' ' {
                scan += 1;
            }
            if start == scan {
                break;
            }
            let token = &norm[start..scan];
            if is_interpreter(token) {
                return true;
            }
            if !is_wrapper(token) {
                break;
            }
        }
    }
    false
}

/// True if `text` matches a destructive pattern. Handles the `curl … | sh`
/// download-and-execute idiom specially (a bare `curl` is not flagged).
pub fn is_destructive(text: &str, patterns: &[String]) -> bool {
    let hay = normalize_haystack(text);
    if patterns.iter().any(|p| hay.contains(&normalize_pattern(p))) {
        return true;
    }
    if elevates_privilege(&hay) {
        return true;
    }
    // Download-and-pipe-to-interpreter. Fetching alone is not destructive —
    // flagging every `curl` would train the operator to click through.
    let fetches = FETCHERS.iter().any(|f| hay.contains(f));
    fetches && pipes_to_interpreter(&hay)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns() -> Vec<String> {
        default_destructive_patterns()
    }

    #[test]
    fn flags_obvious_footguns() {
        for bad in [
            "rm -rf /",
            "sudo reboot",
            "git push --force",
            "mkfs.ext4 /dev/sda",
            "dd if=/dev/zero of=x",
        ] {
            assert!(is_destructive(bad, &patterns()), "should flag: {bad}");
        }
    }

    #[test]
    fn flags_curl_pipe_shell_but_not_bare_curl() {
        assert!(is_destructive("curl https://x.sh | sh", &patterns()));
        assert!(is_destructive("wget -qO- x|bash", &patterns()));
        assert!(!is_destructive(
            "curl https://api.example.com/data.json",
            &patterns()
        ));
    }

    #[test]
    fn allows_benign_text() {
        for ok in [
            "ls -la",
            "echo hello",
            "git status",
            "cd ~/code && cargo test",
        ] {
            assert!(!is_destructive(ok, &patterns()), "should allow: {ok}");
        }
    }

    #[test]
    fn case_insensitive() {
        assert!(is_destructive("SUDO rm -rf /", &patterns()));
    }

    #[test]
    fn normalisation_collapses_whitespace_and_pads_edges() {
        assert_eq!(normalize_haystack("  RM\t\t-rf  / "), " rm -rf / ");
        assert_eq!(normalize_pattern("RM\t-rf"), "rm -rf");
    }

    #[test]
    fn logical_or_is_not_a_pipe() {
        assert!(!pipes_to_interpreter(" curl x || sh_fallback "));
    }
}
