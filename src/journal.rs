use anyhow::Result;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Append-only event journal. Lives outside the repo so writes never
/// perturb the repository epoch fingerprint.
pub struct Journal {
    f: File,
}

impl Journal {
    pub fn open(run_dir: &Path) -> Result<Self> {
        Self::open_named(run_dir, "events")
    }

    /// Named journal within the same run dir (per-scenario logs).
    pub fn open_named(run_dir: &Path, name: &str) -> Result<Self> {
        std::fs::create_dir_all(run_dir)?;
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join(format!("{name}.jsonl")))?;
        Ok(Self { f })
    }

    /// Path of a named journal file (for replay/reconstruction).
    pub fn path_of(run_dir: &Path, name: &str) -> PathBuf {
        run_dir.join(format!("{name}.jsonl"))
    }

    pub fn log(&mut self, kind: &str, data: Value) {
        let ev = json!({
            "ts_unix": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            "type": kind,
            "data": data,
        });
        let _ = self.f.write_all(serde_json::to_string(&ev).unwrap_or_default().as_bytes());
        let _ = self.f.write_all(b"\n");
        let _ = self.f.flush();
    }
}
