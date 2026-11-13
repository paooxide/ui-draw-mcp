//! Bookkeeping for keys held down across calls.
//!
//! A key left down on someone's real machine is the worst thing this engine
//! can do: a latched Shift or Command turns everything they type afterwards
//! into something else. So the rule is that nothing is ever held without a
//! recorded deadline, the number of held keys is capped, and every path that
//! ends a session (an explicit `up`, an error, shutdown, the kill switch, a
//! human taking the mouse) asks this ledger what is still down.
//!
//! This is only the ledger: pure, clock passed in, so every decision here is
//! testable without pressing anything. The module owns the watchdog that acts
//! on it.

/// Most keys that may be held at once. Enough for any chord plus a spare;
/// small enough that a runaway agent cannot pin half a keyboard.
pub const MAX_HELD_KEYS: usize = 6;
/// Longest a key stays down without being re-sent `down`, after which it is
/// released automatically.
pub const HOLD_MAX_MS: u64 = 30_000;

/// Milliseconds on a clock that never jumps: a wall-clock step backwards must
/// not extend how long a key stays down.
pub fn mono_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// The keys currently held and when each is released regardless.
#[derive(Debug)]
pub struct HeldKeys {
    entries: Vec<(String, u64)>,
    ttl_ms: u64,
}

impl Default for HeldKeys {
    fn default() -> Self {
        HeldKeys::with_ttl(HOLD_MAX_MS)
    }
}

impl HeldKeys {
    /// A ledger whose keys are released `ttl_ms` after they were last held.
    pub fn with_ttl(ttl_ms: u64) -> Self {
        HeldKeys {
            entries: Vec::new(),
            ttl_ms,
        }
    }

    /// How long a key stays down unless it is re-sent `down`.
    pub fn ttl_ms(&self) -> u64 {
        self.ttl_ms
    }

    /// Whether `keys` fit: the keys already held count once, so re-sending
    /// `down` for a held key is never refused for being over the cap.
    pub fn check_room(&self, keys: &[String]) -> Result<(), String> {
        let new = keys
            .iter()
            .enumerate()
            .filter(|(i, k)| !self.holds(k) && !keys[..*i].contains(k))
            .count();
        if self.entries.len() + new > MAX_HELD_KEYS {
            return Err(format!(
                "at most {MAX_HELD_KEYS} keys may be held at once and {} already are ({}); \
                 release some with action \"up\" first",
                self.entries.len(),
                self.names().join(", ")
            ));
        }
        Ok(())
    }

    /// Record `key` as held until `now_ms` plus the ledger's time limit. Returns whether it
    /// was newly held; a key already held only has its deadline renewed, so a
    /// long task can keep Shift down by re-sending `down`, and nothing is
    /// pressed twice.
    pub fn hold(&mut self, key: &str, now_ms: u64) -> bool {
        let deadline = now_ms.saturating_add(self.ttl_ms);
        match self.entries.iter_mut().find(|(k, _)| k == key) {
            Some(e) => {
                e.1 = deadline;
                false
            }
            None => {
                self.entries.push((key.to_string(), deadline));
                true
            }
        }
    }

    /// Forget `key`. Returns whether it was held.
    pub fn release(&mut self, key: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(k, _)| k != key);
        self.entries.len() != before
    }

    /// Forget every key and return them, oldest first.
    pub fn take_all(&mut self) -> Vec<String> {
        self.entries.drain(..).map(|(k, _)| k).collect()
    }

    /// Forget and return the keys whose deadline has passed.
    pub fn take_expired(&mut self, now_ms: u64) -> Vec<String> {
        let mut out = Vec::new();
        self.entries.retain(|(k, deadline)| {
            let expired = now_ms >= *deadline;
            if expired {
                out.push(k.clone());
            }
            !expired
        });
        out
    }

    pub fn holds(&self, key: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Held keys, oldest first.
    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|(k, _)| k.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_held_key_is_recorded_once_and_its_deadline_renewed() {
        let mut h = HeldKeys::default();
        assert!(h.hold("shift", 1_000));
        assert!(!h.hold("shift", 20_000), "second down presses nothing");
        assert_eq!(h.names(), ["shift"]);
        // The renewal moved the deadline: not expired at the original one.
        assert!(h.take_expired(1_000 + HOLD_MAX_MS).is_empty());
        assert_eq!(h.take_expired(20_000 + HOLD_MAX_MS), ["shift"]);
        assert!(h.is_empty());
    }

    #[test]
    fn a_ledger_can_be_given_a_shorter_limit() {
        let mut h = HeldKeys::with_ttl(50);
        h.hold("shift", 100);
        assert_eq!(h.ttl_ms(), 50);
        assert!(h.take_expired(149).is_empty());
        assert_eq!(h.take_expired(150), ["shift"]);
        assert_eq!(HeldKeys::default().ttl_ms(), HOLD_MAX_MS);
    }

    #[test]
    fn only_the_keys_past_their_deadline_expire() {
        let mut h = HeldKeys::default();
        h.hold("shift", 0);
        h.hold("a", 10_000);
        assert!(h.take_expired(HOLD_MAX_MS - 1).is_empty());
        assert_eq!(h.take_expired(HOLD_MAX_MS), ["shift"]);
        assert!(h.holds("a") && !h.holds("shift"));
        assert_eq!(h.take_expired(10_000 + HOLD_MAX_MS), ["a"]);
    }

    #[test]
    fn the_cap_counts_new_keys_only() {
        let mut h = HeldKeys::default();
        let six = keys(&["cmd", "shift", "opt", "ctrl", "a", "b"]);
        assert!(h.check_room(&six).is_ok());
        for k in &six {
            h.hold(k, 0);
        }
        assert!(h.check_room(&keys(&["shift", "a"])).is_ok(), "already held");
        let e = h.check_room(&keys(&["c"])).unwrap_err();
        assert!(e.contains("at most 6") && e.contains("shift"), "{e}");
        // Duplicates in one request are one key.
        let mut h = HeldKeys::default();
        h.hold("a", 0);
        assert!(h.check_room(&keys(&["b", "b", "c", "d", "e", "f"])).is_ok());
        assert!(h
            .check_room(&keys(&["b", "c", "d", "e", "f", "g"]))
            .is_err());
    }

    #[test]
    fn release_and_take_all_leave_nothing_behind() {
        let mut h = HeldKeys::default();
        h.hold("shift", 0);
        h.hold("a", 0);
        assert!(h.release("shift"));
        assert!(!h.release("shift"), "not held any more");
        h.hold("b", 0);
        assert_eq!(h.take_all(), ["a", "b"]);
        assert!(h.is_empty() && h.take_all().is_empty());
    }
}
