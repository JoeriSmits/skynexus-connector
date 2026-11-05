//! The log file. Release builds on Windows have no console, so everything
//! the connector logs also goes to `connector.log` in the app's log folder
//! (the folder Tauri uses too: `%LOCALAPPDATA%\nl.skynexus.connector\logs` on
//! Windows, `~/Library/Logs/nl.skynexus.connector` on macOS). At start-up a
//! file over [`ROTATE_AT`] moves to `connector.1.log` and the oldest of
//! [`KEEP`] goes. "Open logs" in the tray writes `diagnostics.txt` next to
//! it and opens the folder. Nothing secret is logged: no credential, no
//! ticket, no frame contents.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The app identifier in tauri.conf.json, which names the folders.
pub const IDENTIFIER: &str = "nl.skynexus.connector";
pub const LOG_FILE: &str = "connector.log";
pub const DIAGNOSTICS_FILE: &str = "diagnostics.txt";
/// Rotate a log that grew past this at start-up.
pub const ROTATE_AT: u64 = 5 * 1024 * 1024;
/// Log files kept: the current one and `connector.1.log` to `connector.{KEEP-1}.log`.
pub const KEEP: usize = 3;

/// The log folder for this platform, from the environment. None when the
/// environment does not say (then only stderr gets the log).
pub fn log_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(IDENTIFIER).join("logs"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Logs").join(IDENTIFIER))
    } else {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share").join(IDENTIFIER).join("logs"))
    }
}

fn numbered(dir: &Path, n: usize) -> PathBuf {
    if n == 0 {
        dir.join(LOG_FILE)
    } else {
        dir.join(format!("connector.{n}.log"))
    }
}

/// Moves a log over `limit` bytes aside, keeping `keep` files in all.
pub fn rotate(dir: &Path, limit: u64, keep: usize) -> io::Result<()> {
    let current = numbered(dir, 0);
    let big = std::fs::metadata(&current).is_ok_and(|m| m.len() > limit);
    if !big || keep < 2 {
        return Ok(());
    }
    let _ = std::fs::remove_file(numbered(dir, keep - 1));
    for n in (0..keep - 1).rev() {
        let from = numbered(dir, n);
        if from.exists() {
            std::fs::rename(&from, numbered(dir, n + 1))?;
        }
    }
    Ok(())
}

/// Opens the log file for appending, rotating it first. None on any error:
/// a log file must never stop the connector from starting.
pub fn open(dir: &Path) -> Option<File> {
    std::fs::create_dir_all(dir).ok()?;
    let _ = rotate(dir, ROTATE_AT, KEEP);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))
        .ok()
}

/// Writes every log line to stderr and, when there is one, the log file.
pub struct Tee {
    file: Option<Mutex<File>>,
}

impl Tee {
    pub fn new(file: Option<File>) -> Self {
        Tee {
            file: file.map(Mutex::new),
        }
    }
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        if let Some(f) = &self.file {
            if let Ok(mut f) = f.lock() {
                let _ = f.write_all(buf);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(f) = &self.file {
            if let Ok(mut f) = f.lock() {
                let _ = f.flush();
            }
        }
        Ok(())
    }
}

/// The last `n` lines of the current log file.
pub fn tail(dir: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(dir.join(LOG_FILE)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Writes `diagnostics.txt` (the given summary, then the last 300 log lines)
/// and returns its path.
pub fn write_diagnostics(dir: &Path, summary: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(DIAGNOSTICS_FILE);
    let text = format!("{summary}\n\n--- last log lines ---\n{}\n", tail(dir, 300));
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("snx-logs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn identifier_matches_the_tauri_config() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["identifier"], IDENTIFIER);
    }

    #[test]
    fn rotates_only_a_big_log_and_keeps_three() {
        let d = temp("rotate");
        std::fs::write(d.join(LOG_FILE), "small").unwrap();
        rotate(&d, 10, 3).unwrap();
        assert!(!d.join("connector.1.log").exists(), "under the limit: left alone");

        for round in 0..4 {
            std::fs::write(d.join(LOG_FILE), format!("round {round} with enough text")).unwrap();
            rotate(&d, 10, 3).unwrap();
        }
        assert!(
            !d.join(LOG_FILE).exists(),
            "moved aside, the next open starts a new one"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("connector.1.log")).unwrap(),
            "round 3 with enough text"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("connector.2.log")).unwrap(),
            "round 2 with enough text"
        );
        assert!(!d.join("connector.3.log").exists(), "three files at most");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tee_appends_to_the_file() {
        let d = temp("tee");
        let mut t = Tee::new(open(&d));
        writeln!(t, "first").unwrap();
        writeln!(t, "second").unwrap();
        t.flush().unwrap();
        drop(t);
        let mut t = Tee::new(open(&d));
        writeln!(t, "third").unwrap();
        drop(t);
        assert_eq!(tail(&d, 2), "second\nthird");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn diagnostics_hold_the_summary_and_the_log_tail() {
        let d = temp("diag");
        std::fs::write(d.join(LOG_FILE), "a\nb\nc\n").unwrap();
        let p = write_diagnostics(&d, "SkyNexus Connector 0.1.0").unwrap();
        let text = std::fs::read_to_string(p).unwrap();
        assert!(text.starts_with("SkyNexus Connector 0.1.0\n"));
        assert!(text.ends_with("a\nb\nc\n"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_folder_gives_no_file_and_no_panic() {
        let mut t = Tee::new(None);
        writeln!(t, "only stderr").unwrap();
        assert_eq!(tail(Path::new("/definitely/not/here"), 5), "");
    }
}
