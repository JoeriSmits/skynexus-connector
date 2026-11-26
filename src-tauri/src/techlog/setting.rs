//! The "Read TechLog" switch. The choice is kept as a marker file in the
//! app's config folder: the file present means on, no file means off (the
//! default). Nothing is read from TechLog until the user turns it on.

use std::path::Path;

pub const SETTING_FILE: &str = "read-techlog";

pub fn is_on(config_dir: &Path) -> bool {
    config_dir.join(SETTING_FILE).is_file()
}

pub fn set(config_dir: &Path, on: bool) -> std::io::Result<()> {
    let file = config_dir.join(SETTING_FILE);
    if on {
        std::fs::create_dir_all(config_dir)?;
        std::fs::write(file, "on\n")
    } else {
        match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("skynexus-techlog-setting-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_missing_file_or_folder_means_off() {
        let d = temp("missing");
        assert!(!is_on(&d));
        std::fs::create_dir_all(&d).unwrap();
        assert!(!is_on(&d));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn on_creates_the_folder_and_off_removes_the_file() {
        let d = temp("toggle");
        set(&d, true).unwrap();
        assert!(is_on(&d));
        set(&d, true).unwrap();
        assert!(is_on(&d));
        set(&d, false).unwrap();
        assert!(!is_on(&d));
        set(&d, false).unwrap();
        assert!(!is_on(&d));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
