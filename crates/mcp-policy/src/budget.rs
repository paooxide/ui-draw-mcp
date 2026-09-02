use std::sync::atomic::{AtomicUsize, Ordering};

/// Anti-spin counter: if the agent keeps proposing denied calls, the session is
/// aborted. Counted atomically so concurrent denials can't miscount (TOCTOU).
pub struct DenialBudget {
    count: AtomicUsize,
    max: usize,
}

impl DenialBudget {
    pub fn new(max: usize) -> Self {
        DenialBudget {
            count: AtomicUsize::new(0),
            max,
        }
    }

    /// Record one denial. Returns `true` if the budget is now exhausted (the
    /// session should abort). With `max == 0` the budget is disabled.
    pub fn record(&self) -> bool {
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        self.max != 0 && n >= self.max
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trips_at_max() {
        let b = DenialBudget::new(3);
        assert!(!b.record()); // 1
        assert!(!b.record()); // 2
        assert!(b.record()); // 3 -> exhausted
        assert_eq!(b.count(), 3);
    }

    #[test]
    fn zero_max_never_trips() {
        let b = DenialBudget::new(0);
        for _ in 0..100 {
            assert!(!b.record());
        }
    }

    #[test]
    fn concurrent_denials_count_exactly() {
        use std::sync::Arc;
        let b = Arc::new(DenialBudget::new(0));
        let mut hs = Vec::new();
        for _ in 0..8 {
            let b = b.clone();
            hs.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    b.record();
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(b.count(), 8000);
    }
}
