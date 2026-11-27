//! Is GSX Pro on this PC, running, and does it speak the Remote API v2?
//! One code answers that, for the SkyNexus tray and the EFB.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The lowest Remote API protocol we speak.
pub const MIN_PROTOCOL: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GsxCode {
    #[serde(rename = "GSX_NOT_INSTALLED")]
    NotInstalled,
    #[serde(rename = "GSX_NOT_RUNNING")]
    NotRunning,
    #[serde(rename = "GSX_NO_API")]
    NoApi,
    #[serde(rename = "GSX_AUTH")]
    Auth,
    #[serde(rename = "GSX_OK")]
    Ok,
}

/// What the local WebSocket said, already parsed (parsing is the client's job).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hello {
    /// Connection refused or timed out.
    Refused,
    Valid {
        protocol: u32,
        engine: String,
        auth_required: bool,
    },
    /// Connected, but not a hello we understand.
    Invalid,
}

/// `installed`: the add-ons ini or the registry root exists. `couatl` is
/// `L:FSDT_GSX_COUATL_STARTED`, None when unreadable (not MSFS, or the
/// L:var definition was refused).
pub fn classify(installed: bool, couatl: Option<bool>, hello: Hello) -> GsxCode {
    if !installed {
        return GsxCode::NotInstalled;
    }
    if couatl == Some(false) {
        return GsxCode::NotRunning;
    }
    match hello {
        Hello::Refused if couatl.is_none() => GsxCode::NotRunning,
        Hello::Refused | Hello::Invalid => GsxCode::NoApi,
        Hello::Valid { protocol, .. } if protocol < MIN_PROTOCOL => GsxCode::NoApi,
        Hello::Valid {
            auth_required: true, ..
        } => GsxCode::Auth,
        Hello::Valid { .. } => GsxCode::Ok,
    }
}

/// GSX is installed when `CouatlAddons.ini` exists or the registry holds
/// `HKCU\SOFTWARE\FSDreamTeam` value `root`.
pub fn installed(ini: &Path, registry_root: Option<String>) -> bool {
    ini.exists() || registry_root.is_some_and(|r| !r.trim().is_empty())
}

/// `%APPDATA%\Virtuali\CouatlAddons.ini`; None off Windows.
pub fn ini_path() -> Option<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(Path::new(&appdata).join("Virtuali").join("CouatlAddons.ini"))
}

/// `HKCU\SOFTWARE\FSDreamTeam` value `root`; None off Windows.
#[cfg(windows)]
pub fn registry_root() -> Option<String> {
    use std::ffi::c_void;

    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
    const RRF_RT: u32 = 0x0000_0002 | 0x0000_0004 | 0x1000_0000; // SZ | EXPAND_SZ | NOEXPAND
    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            hkey: isize,
            subkey: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut c_void,
            len: *mut u32,
        ) -> i32;
    }
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (sub, val) = (wide("SOFTWARE\\FSDreamTeam"), wide("root"));
    let mut bytes = 0u32;
    // SAFETY: NUL-terminated wide strings; a null data pointer asks for the size.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if rc != 0 || bytes < 2 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    // SAFETY: `buf` holds at least `bytes` bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    let s = String::from_utf16_lossy(&buf[..end]);
    (!s.trim().is_empty()).then_some(s)
}

#[cfg(not(windows))]
pub fn registry_root() -> Option<String> {
    None
}

/// Installed on this PC, by the ini and the registry.
pub fn installed_here() -> bool {
    installed(&ini_path().unwrap_or_default(), registry_root())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(protocol: u32, auth_required: bool) -> Hello {
        Hello::Valid {
            protocol,
            engine: "couatl".into(),
            auth_required,
        }
    }

    #[test]
    fn not_installed_beats_everything() {
        let c = classify(false, Some(true), valid(2, false));
        assert_eq!(c, GsxCode::NotInstalled);
    }

    #[test]
    fn couatl_stopped_is_not_running() {
        let c = classify(true, Some(false), valid(2, false));
        assert_eq!(c, GsxCode::NotRunning);
    }

    #[test]
    fn running_without_a_socket_is_no_api() {
        let c = classify(true, Some(true), Hello::Refused);
        assert_eq!(c, GsxCode::NoApi);
        let c = classify(true, Some(true), Hello::Invalid);
        assert_eq!(c, GsxCode::NoApi);
    }

    #[test]
    fn protocol_one_is_no_api() {
        let c = classify(true, Some(true), valid(1, false));
        assert_eq!(c, GsxCode::NoApi);
    }

    #[test]
    fn auth_required_is_auth() {
        let c = classify(true, Some(true), valid(2, true));
        assert_eq!(c, GsxCode::Auth);
    }

    #[test]
    fn protocol_two_is_ok() {
        let c = classify(true, Some(true), valid(2, false));
        assert_eq!(c, GsxCode::Ok);
        let c = classify(true, Some(true), valid(3, false));
        assert_eq!(c, GsxCode::Ok);
    }

    #[test]
    fn unreadable_couatl_with_refused_is_not_running() {
        let c = classify(true, None, Hello::Refused);
        assert_eq!(c, GsxCode::NotRunning);
    }

    #[test]
    fn unreadable_couatl_still_trusts_a_valid_hello() {
        let c = classify(true, None, valid(2, false));
        assert_eq!(c, GsxCode::Ok);
    }

    #[test]
    fn codes_serialize_as_the_wire_strings() {
        use GsxCode::*;
        let all = [
            (NotInstalled, "GSX_NOT_INSTALLED"),
            (NotRunning, "GSX_NOT_RUNNING"),
            (NoApi, "GSX_NO_API"),
            (Auth, "GSX_AUTH"),
            (Ok, "GSX_OK"),
        ];
        for (code, wire) in all {
            let s = serde_json::to_string(&code).unwrap();
            assert_eq!(s, format!("\"{wire}\""));
            assert_eq!(serde_json::from_str::<GsxCode>(&s).unwrap(), code);
        }
    }

    #[test]
    fn installed_by_ini_or_registry() {
        let dir = std::env::temp_dir().join("gsx-detect-test");
        std::fs::create_dir_all(&dir).unwrap();
        let ini = dir.join("CouatlAddons.ini");
        std::fs::write(&ini, "x").unwrap();
        assert!(installed(&ini, None));
        let missing = dir.join("nope.ini");
        assert!(!installed(&missing, None));
        assert!(installed(&missing, Some("C:\\GSX".into())));
        assert!(!installed(&missing, Some("  ".into())));
    }
}
