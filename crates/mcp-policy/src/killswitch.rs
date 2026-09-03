use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// The mandatory kill switch: the presence of a STOP file aborts activity. It is
/// polled before every gate decision and (by engines) inside long-running calls.
///
/// It can also be tripped from inside the process — by the pointer watcher when
/// a human takes over the mouse. That path sets an in-memory flag *first* and
/// writes the file afterwards, because the flag cannot fail: a full disk or a
/// read-only home must not be the reason a stop does not stop.
pub struct KillSwitch {
    file: PathBuf,
    tripped_here: AtomicBool,
    reason: Mutex<Option<String>>,
}

impl KillSwitch {
    pub fn new(file: impl Into<PathBuf>) -> Self {
        KillSwitch {
            file: file.into(),
            tripped_here: AtomicBool::new(false),
            reason: Mutex::new(None),
        }
    }

    /// True when the STOP file exists, or something in this process tripped it.
    pub fn tripped(&self) -> bool {
        self.tripped_here.load(Ordering::SeqCst) || self.file.exists()
    }

    /// Trip the switch and record why.
    ///
    /// Writing the file matters as much as the flag: it makes the stop survive
    /// a restart, so an agent cannot be resumed by relaunching it, and it makes
    /// the reason visible to `agentctl doctor` and to whoever finds the machine.
    pub fn trip(&self, reason: &str) {
        self.tripped_here.store(true, Ordering::SeqCst);
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason.to_string());
        if let Some(parent) = self.file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let line = format!("{} {reason}\n", crate::now_ms());
        if let Err(e) = std::fs::write(&self.file, line) {
            tracing::error!(
                file = %self.file.display(),
                error = %e,
                "kill switch tripped but the STOP file could not be written; \
                 this session is stopped, but a restart would not be"
            );
        }
    }

    /// Why the switch is tripped, if known.
    pub fn reason(&self) -> Option<String> {
        if let Some(r) = self
            .reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Some(r);
        }
        // A STOP file written by an earlier session, or by a human with `touch`.
        std::fs::read_to_string(&self.file)
            .ok()
            .and_then(|s| s.lines().next().map(str::to_string))
            .filter(|s| !s.trim().is_empty())
    }

    pub fn file(&self) -> &Path {
        &self.file
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_stop_file() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let ks = KillSwitch::new(&stop);
        assert!(!ks.tripped());
        std::fs::write(&stop, b"").unwrap();
        assert!(ks.tripped());
    }

    #[test]
    fn tripping_records_the_reason_and_persists_it() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let ks = KillSwitch::new(&stop);
        ks.trip("pointer moved by human");
        assert!(ks.tripped());
        assert!(ks.reason().unwrap().contains("pointer moved by human"));
        // Persisted, so restarting the server does not resume the agent.
        let onto_disk = std::fs::read_to_string(&stop).unwrap();
        assert!(onto_disk.contains("pointer moved by human"));
        let fresh = KillSwitch::new(&stop);
        assert!(fresh.tripped());
        assert!(fresh.reason().unwrap().contains("pointer moved by human"));
    }

    /// The in-memory flag must not depend on the filesystem. A stop that fails
    /// because the disk is full is not a stop.
    #[test]
    fn a_trip_holds_even_when_the_file_cannot_be_written() {
        let ks = KillSwitch::new("/this/path/does/not/exist/and/cannot/be/made/STOP");
        ks.trip("test");
        assert!(ks.tripped(), "the flag must not depend on the write");
        assert_eq!(ks.reason().as_deref(), Some("test"));
    }
}
