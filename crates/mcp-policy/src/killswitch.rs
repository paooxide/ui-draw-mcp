use std::path::{Path, PathBuf};

/// The mandatory kill switch: the presence of a STOP file aborts activity. It is
/// polled before every gate decision and (by engines) inside long-running calls.
pub struct KillSwitch {
    file: PathBuf,
}

impl KillSwitch {
    pub fn new(file: impl Into<PathBuf>) -> Self {
        KillSwitch { file: file.into() }
    }

    /// True when the STOP file exists.
    pub fn tripped(&self) -> bool {
        self.file.exists()
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
}
