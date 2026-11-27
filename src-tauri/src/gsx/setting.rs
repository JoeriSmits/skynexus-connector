//! The "Read GSX" switch. On by default: the choice is kept as a marker
//! file in the app's config folder, present only while the user turned
//! reading off.

use std::path::Path;

pub const OFF_FILE: &str = "read-gsx-off";

pub fn is_on(config_dir: &Path) -> bool {
    !config_dir.join(OFF_FILE).is_file()
}

pub fn set(config_dir: &Path, on: bool) -> std::io::Result<()> {
    let file = config_dir.join(OFF_FILE);
    if on {
        match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        std::fs::create_dir_all(config_dir)?;
        std::fs::write(file, "off\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("skynexus-gsx-setting-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_missing_file_or_folder_means_on() {
        let d = temp("missing");
        assert!(is_on(&d));
        std::fs::create_dir_all(&d).unwrap();
        assert!(is_on(&d));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn off_writes_the_marker_and_on_removes_it() {
        let d = temp("toggle");
        set(&d, false).unwrap();
        assert!(!is_on(&d));
        set(&d, false).unwrap();
        set(&d, true).unwrap();
        assert!(is_on(&d));
        set(&d, true).unwrap();
        assert!(is_on(&d));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
