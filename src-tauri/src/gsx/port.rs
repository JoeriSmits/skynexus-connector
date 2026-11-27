//! Where GSX's local Remote API listens.

use std::path::{Path, PathBuf};

/// Port used when the ini has no (valid) `remote_server_port`.
pub const DEFAULT_PORT: u16 = 8744;

/// `%APPDATA%\Virtuali\CouatlAddons.ini`. On other platforms (or without
/// APPDATA) this is a relative path that will simply not exist.
pub fn ini_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    base.join("Virtuali").join("CouatlAddons.ini")
}

/// The `remote_server_port` in `ini`, or 8744 when the file or key is
/// missing or unparsable.
pub fn remote_port(ini: &Path) -> u16 {
    std::fs::read_to_string(ini)
        .ok()
        .and_then(|text| parse_port(&text))
        .unwrap_or(DEFAULT_PORT)
}

fn parse_port(text: &str) -> Option<u16> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if !key.trim().eq_ignore_ascii_case("remote_server_port") {
            return None;
        }
        value.trim().parse::<u16>().ok().filter(|p| *p != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ini(name: &str, body: Option<&str>) -> PathBuf {
        let p = std::env::temp_dir().join(format!("gsx-port-{}-{name}.ini", std::process::id()));
        let _ = std::fs::remove_file(&p);
        if let Some(b) = body {
            std::fs::write(&p, b).unwrap();
        }
        p
    }

    #[test]
    fn reads_the_port() {
        let p = ini("ok", Some("[General]\nremote_server_port = 9001\n"));
        assert_eq!(remote_port(&p), 9001);
    }

    #[test]
    fn missing_file_is_default() {
        assert_eq!(remote_port(&ini("nofile", None)), 8744);
    }

    #[test]
    fn missing_key_is_default() {
        let p = ini("nokey", Some("[General]\nother = 1\n"));
        assert_eq!(remote_port(&p), 8744);
    }

    #[test]
    fn unparsable_is_default() {
        let p = ini("bad", Some("remote_server_port = abc\n"));
        assert_eq!(remote_port(&p), 8744);
    }
}
