//! Start and stop with MSFS.
//!
//! MSFS 2020 and 2024 read `exe.xml` at start-up and launch every add-on
//! program listed in it (FSUIPC, GSX and Fenix use it the same way). The
//! connector keeps one entry there, named [`ENTRY_NAME`], that starts it with
//! [`LAUNCH_FLAG`]; started that way it quits again once MSFS has closed
//! ([`QuitWatch`]).
//!
//! Only that entry is ever added, updated or removed. The rest of the file
//! stays byte for byte as it was (other add-ons, comments, line endings, a
//! BOM), the previous version is kept as `exe.xml.skynexus-backup` before
//! each change, and a file without a `</SimBase.Document>` end tag is left
//! alone. "Start with MSFS" in the tray turns it off; the choice is kept in
//! [`SETTING_FILE`] in the app's config folder. Without that file the entry
//! is on (the default).

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const ENTRY_NAME: &str = "SkyNexus Connector";
pub const LAUNCH_FLAG: &str = "--launched-by-msfs";
pub const BACKUP_EXTENSION: &str = "xml.skynexus-backup";
pub const SETTING_FILE: &str = "start-with-msfs";

/// How long MSFS may stay away after it was connected before a connector it
/// started quits: long enough for a SimConnect hiccup, short enough that
/// closing the sim visibly closes the connector too.
pub const QUIT_GRACE: Duration = Duration::from_secs(30);

const END_TAG: &str = "</SimBase.Document>";
const BOM: &str = "\u{feff}";

/// The folders MSFS keeps `exe.xml` in: 2020 and 2024, Steam and Microsoft Store.
pub fn exe_xml_dirs(appdata: &Path, local_appdata: &Path) -> [PathBuf; 4] {
    let store = |package: &str| local_appdata.join("Packages").join(package).join("LocalCache");
    [
        appdata.join("Microsoft Flight Simulator"),
        store("Microsoft.FlightSimulator_8wekyb3d8bbwe"),
        appdata.join("Microsoft Flight Simulator 2024"),
        store("Microsoft.Limitless_8wekyb3d8bbwe"),
    ]
}

/// `exe.xml` of each MSFS install on this computer.
pub fn installed_exe_xmls() -> Vec<PathBuf> {
    let (Some(appdata), Some(local)) = (std::env::var_os("APPDATA"), std::env::var_os("LOCALAPPDATA")) else {
        return Vec::new();
    };
    exe_xmls_in(&exe_xml_dirs(Path::new(&appdata), Path::new(&local)))
}

/// The `exe.xml` paths worth touching in these folders: one that exists, or
/// one MSFS would read because the folder holds its `UserCfg.opt`. A folder
/// left behind by an uninstalled MSFS (no `UserCfg.opt`) gets no new file.
pub fn exe_xmls_in(dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .map(|d| d.join("exe.xml"))
        .filter(|f| f.is_file() || f.with_file_name("UserCfg.opt").is_file())
        .collect()
}

/// Whether the entry is wanted: on unless the setting file says `off`.
pub fn wanted(setting: Option<&str>) -> bool {
    setting.is_none_or(|s| s.trim() != "off")
}

pub fn read_setting(config_dir: &Path) -> Option<String> {
    std::fs::read_to_string(config_dir.join(SETTING_FILE)).ok()
}

pub fn write_setting(config_dir: &Path, on: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    std::fs::write(config_dir.join(SETTING_FILE), if on { "on\n" } else { "off\n" })
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The exe path as MSFS should see it: no `\\?\` verbatim prefix.
pub fn display_exe(exe: &Path) -> String {
    let s = exe.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// Our `<Launch.Addon>` block, indented like MSFS writes its own.
pub fn entry_block(exe: &Path, nl: &str) -> String {
    [
        "  <Launch.Addon>".to_string(),
        format!("    <Name>{ENTRY_NAME}</Name>"),
        "    <Disabled>False</Disabled>".to_string(),
        "    <ManualLoad>False</ManualLoad>".to_string(),
        format!("    <Path>{}</Path>", escape(&display_exe(exe))),
        format!("    <CommandLine>{LAUNCH_FLAG}</CommandLine>"),
        "    <NewConsole>False</NewConsole>".to_string(),
        "  </Launch.Addon>".to_string(),
    ]
    .iter()
    .map(|l| format!("{l}{nl}"))
    .collect()
}

/// A whole `exe.xml` with only our entry, for an install that has none yet.
pub fn new_document(exe: &Path, nl: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>{nl}\
         <SimBase.Document Type=\"Launch\" version=\"1,0\">{nl}\
         \x20 <Descr>Launch</Descr>{nl}\
         \x20 <Filename>EXE.xml</Filename>{nl}\
         \x20 <Disabled>False</Disabled>{nl}\
         \x20 <Launch.ManualLoad>False</Launch.ManualLoad>{nl}\
         {}{END_TAG}{nl}",
        entry_block(exe, nl)
    )
}

fn newline_of(xml: &str) -> &'static str {
    if xml.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn line_start(xml: &str, at: usize) -> usize {
    xml[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// The bytes of our block: from the start of its first line through the
/// newline after `</Launch.Addon>`.
fn find_entry(xml: &str) -> Option<Range<usize>> {
    const OPEN: &str = "<Launch.Addon>";
    const CLOSE: &str = "</Launch.Addon>";
    let name = format!("<Name>{ENTRY_NAME}</Name>");
    let mut from = 0;
    while let Some(i) = xml[from..].find(OPEN) {
        let open = from + i;
        let close = open + xml[open..].find(CLOSE)? + CLOSE.len();
        if xml[open..close].contains(&name) {
            let start = line_start(xml, open);
            let rest = &xml[close..];
            let end = close
                + if rest.starts_with("\r\n") {
                    2
                } else if rest.starts_with('\n') {
                    1
                } else {
                    0
                };
            // Only take the indentation if nothing else sits on that line.
            let start = if xml[start..open].trim().is_empty() {
                start
            } else {
                open
            };
            return Some(start..end);
        }
        from = close;
    }
    None
}

/// `xml` with our entry pointing at `exe`: None when it already does.
/// Refuses a file that has no `</SimBase.Document>` end tag.
pub fn with_entry(xml: &str, exe: &Path) -> Result<Option<String>, String> {
    let block = entry_block(exe, newline_of(xml));
    if let Some(r) = find_entry(xml) {
        if xml[r.clone()] == block {
            return Ok(None);
        }
        let mut out = xml.to_string();
        out.replace_range(r, &block);
        return Ok(Some(out));
    }
    let end = xml
        .rfind(END_TAG)
        .ok_or_else(|| format!("no {END_TAG} end tag; left alone"))?;
    let at = line_start(xml, end);
    let at = if xml[at..end].trim().is_empty() { at } else { end };
    let mut out = xml.to_string();
    out.insert_str(at, &block);
    Ok(Some(out))
}

/// `xml` without our entry: None when it has none.
pub fn without_entry(xml: &str) -> Option<String> {
    let r = find_entry(xml)?;
    let mut out = xml.to_string();
    out.replace_range(r, "");
    Some(out)
}

pub fn has_entry(xml: &str) -> bool {
    find_entry(xml).is_some()
}

fn split_bom(raw: &str) -> (&str, &str) {
    match raw.strip_prefix(BOM) {
        Some(rest) => (BOM, rest),
        None => ("", raw),
    }
}

/// Keeps the current file as the backup, then replaces it (temp file and rename).
fn replace_file(path: &Path, current: Option<&str>, next: &str) -> std::io::Result<()> {
    if let Some(cur) = current {
        std::fs::write(path.with_extension(BACKUP_EXTENSION), cur)?;
    }
    let tmp = path.with_extension("xml.skynexus-tmp");
    std::fs::write(&tmp, next)?;
    std::fs::rename(&tmp, path)
}

/// Adds or updates our entry in one `exe.xml`. Ok(true) when the file changed.
pub fn register_in(path: &Path, exe: &Path) -> Result<bool, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let (bom, xml) = split_bom(&raw);
            match with_entry(xml, exe)? {
                None => Ok(false),
                Some(next) => replace_file(path, Some(&raw), &format!("{bom}{next}"))
                    .map(|()| true)
                    .map_err(|e| e.to_string()),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => replace_file(path, None, &new_document(exe, "\r\n"))
            .map(|()| true)
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Removes our entry from one `exe.xml`. Ok(true) when the file changed;
/// a missing file is fine.
pub fn unregister_in(path: &Path) -> Result<bool, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let (bom, xml) = split_bom(&raw);
    match without_entry(xml) {
        None => Ok(false),
        Some(next) => replace_file(path, Some(&raw), &format!("{bom}{next}"))
            .map(|()| true)
            .map_err(|e| e.to_string()),
    }
}

/// Brings every MSFS install in line with the setting, logging what changed.
/// Returns whether the entry is on.
pub fn apply(config_dir: &Path, exe: &Path) -> bool {
    let on = wanted(read_setting(config_dir).as_deref());
    for path in installed_exe_xmls() {
        let result = if on {
            register_in(&path, exe)
        } else {
            unregister_in(&path)
        };
        match result {
            Ok(true) if on => log::info!("start with MSFS: entry written to {}", path.display()),
            Ok(true) => log::info!("start with MSFS: entry removed from {}", path.display()),
            Ok(false) => {}
            Err(e) => log::warn!("start with MSFS: {}: {e}", path.display()),
        }
    }
    on
}

/// Turns the entry on or off for good: the setting, then every install.
pub fn set(config_dir: &Path, exe: &Path, on: bool) -> bool {
    if let Err(e) = write_setting(config_dir, on) {
        log::warn!("start with MSFS: cannot save the setting: {e}");
    }
    apply(config_dir, exe)
}

/// Decides when a connector that MSFS started should quit: only after MSFS
/// was connected at least once, and then stayed away for [`QUIT_GRACE`].
/// Before the first connection it waits as long as it takes (MSFS starts
/// add-ons early, long before SimConnect answers).
#[derive(Debug, Default)]
pub struct QuitWatch {
    seen: bool,
    gone_since: Option<Instant>,
}

impl QuitWatch {
    pub fn update(&mut self, connected: bool, now: Instant) {
        if connected {
            self.seen = true;
            self.gone_since = None;
        } else if self.seen && self.gone_since.is_none() {
            self.gone_since = Some(now);
        }
    }

    pub fn should_quit(&self, now: Instant) -> bool {
        self.gone_since
            .is_some_and(|since| now.duration_since(since) >= QUIT_GRACE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSFS_FILE: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
<SimBase.Document Type=\"Launch\" version=\"1,0\">\r\n\
  <Descr>Launch</Descr>\r\n\
  <Filename>EXE.xml</Filename>\r\n\
  <Disabled>False</Disabled>\r\n\
  <Launch.ManualLoad>False</Launch.ManualLoad>\r\n\
  <Launch.Addon>\r\n\
    <Name>FSUIPC7</Name>\r\n\
    <Disabled>False</Disabled>\r\n\
    <Path>E:\\Programs\\FSUIPC7\\FSUIPC7.exe</Path>\r\n\
    <CommandLine>-auto</CommandLine>\r\n\
  </Launch.Addon>\r\n\
  <Launch.Addon>\r\n\
    <Name>Couatl</Name>\r\n\
    <Path>E:\\Programs\\GSX\\couatl64_boot.exe</Path>\r\n\
  </Launch.Addon>\r\n\
</SimBase.Document>\r\n";

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Users\Pilot\AppData\Local\SkyNexus Connector\skynexus-connector.exe")
    }

    #[test]
    fn adds_the_entry_and_keeps_everything_else() {
        let out = with_entry(MSFS_FILE, &exe()).unwrap().unwrap();
        assert!(has_entry(&out));
        assert!(out.contains("<CommandLine>--launched-by-msfs</CommandLine>\r\n"));
        assert!(out.contains(r"<Path>C:\Users\Pilot\AppData\Local\SkyNexus Connector\skynexus-connector.exe</Path>"));
        assert!(
            out.ends_with("  </Launch.Addon>\r\n</SimBase.Document>\r\n"),
            "before the end tag"
        );
        assert!(!out.replace("\r\n", "").contains('\n'), "CRLF throughout");
        assert_eq!(
            without_entry(&out).unwrap(),
            MSFS_FILE,
            "removing gives the file back byte for byte"
        );
    }

    #[test]
    fn writing_twice_changes_nothing() {
        let once = with_entry(MSFS_FILE, &exe()).unwrap().unwrap();
        assert_eq!(with_entry(&once, &exe()).unwrap(), None);
        assert_eq!(once.matches(&format!("<Name>{ENTRY_NAME}</Name>")).count(), 1);
    }

    #[test]
    fn a_moved_exe_updates_the_entry_in_place() {
        let once = with_entry(MSFS_FILE, &exe()).unwrap().unwrap();
        let moved = with_entry(&once, Path::new(r"D:\Apps\skynexus-connector.exe"))
            .unwrap()
            .unwrap();
        assert_eq!(moved.matches(&format!("<Name>{ENTRY_NAME}</Name>")).count(), 1);
        assert!(moved.contains(r"<Path>D:\Apps\skynexus-connector.exe</Path>"));
        assert!(!moved.contains("Pilot"));
        assert_eq!(without_entry(&moved).unwrap(), MSFS_FILE);
    }

    #[test]
    fn other_entries_are_never_touched() {
        let with_ours = with_entry(MSFS_FILE, &exe()).unwrap().unwrap();
        let other = "  <Launch.Addon>\r\n    <Name>SkyNexus Connector Beta</Name>\r\n  </Launch.Addon>\r\n";
        let xml = MSFS_FILE.replace("</SimBase.Document>", &format!("{other}</SimBase.Document>"));
        assert!(!has_entry(&xml), "a different name is not ours");
        assert_eq!(without_entry(&xml), None);
        assert!(with_ours.contains("<Name>FSUIPC7</Name>") && with_ours.contains("<Name>Couatl</Name>"));
    }

    #[test]
    fn unix_newlines_stay_unix() {
        let lf = MSFS_FILE.replace("\r\n", "\n");
        let out = with_entry(&lf, &exe()).unwrap().unwrap();
        assert!(!out.contains('\r'));
        assert_eq!(without_entry(&out).unwrap(), lf);
    }

    #[test]
    fn a_file_it_does_not_understand_is_left_alone() {
        assert!(with_entry("<SimBase.Document>", &exe()).is_err());
        assert!(with_entry("", &exe()).is_err());
    }

    #[test]
    fn paths_are_escaped_and_lose_the_verbatim_prefix() {
        let block = entry_block(Path::new(r"\\?\C:\A&B <x>\c.exe"), "\n");
        assert!(block.contains(r"<Path>C:\A&amp;B &lt;x&gt;\c.exe</Path>"));
    }

    #[test]
    fn a_new_document_holds_only_our_entry() {
        let doc = new_document(&exe(), "\r\n");
        assert!(has_entry(&doc));
        assert!(doc.starts_with("<?xml") && doc.ends_with("</SimBase.Document>\r\n"));
        assert!(doc.contains("  <Launch.ManualLoad>False</Launch.ManualLoad>\r\n"));
        assert_eq!(without_entry(&doc).unwrap().matches("Launch.Addon").count(), 0);
    }

    #[test]
    fn on_unless_switched_off() {
        assert!(wanted(None));
        assert!(wanted(Some("on\n")));
        assert!(!wanted(Some("off\n")));
    }

    #[test]
    fn msfs_folders() {
        let dirs = exe_xml_dirs(Path::new(r"C:\R"), Path::new(r"C:\L"));
        assert_eq!(dirs[2], Path::new(r"C:\R").join("Microsoft Flight Simulator 2024"));
        assert!(dirs[3].ends_with(Path::new("Microsoft.Limitless_8wekyb3d8bbwe").join("LocalCache")));
    }

    #[test]
    fn file_round_trip_keeps_a_bom_and_a_backup() {
        let dir = std::env::temp_dir().join(format!("snx-exexml-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("exe.xml");
        let original = format!("{BOM}{MSFS_FILE}");
        std::fs::write(&path, &original).unwrap();

        assert_eq!(register_in(&path, &exe()), Ok(true));
        assert_eq!(register_in(&path, &exe()), Ok(false), "already there");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with(BOM) && has_entry(&written));
        let backup = std::fs::read_to_string(path.with_extension(BACKUP_EXTENSION)).unwrap();
        assert_eq!(backup, original);

        assert_eq!(unregister_in(&path), Ok(true));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(unregister_in(&path), Ok(false));

        let fresh = dir.join("fresh").join("exe.xml");
        std::fs::create_dir_all(fresh.parent().unwrap()).unwrap();
        assert_eq!(register_in(&fresh, &exe()), Ok(true), "a missing file is created");
        assert!(has_entry(&std::fs::read_to_string(&fresh).unwrap()));
        assert_eq!(unregister_in(&dir.join("none.xml")), Ok(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_real_installs_get_a_new_file() {
        let root = std::env::temp_dir().join(format!("snx-installs-{}", std::process::id()));
        let (with_file, with_cfg, leftover) = (root.join("a"), root.join("b"), root.join("c"));
        for d in [&with_file, &with_cfg, &leftover] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(with_file.join("exe.xml"), MSFS_FILE).unwrap();
        std::fs::write(with_cfg.join("UserCfg.opt"), "").unwrap();
        let found = exe_xmls_in(&[with_file.clone(), with_cfg.clone(), leftover, root.join("missing")]);
        assert_eq!(found, vec![with_file.join("exe.xml"), with_cfg.join("exe.xml")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn quits_only_after_msfs_was_there_and_stayed_away() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = QuitWatch::default();
        w.update(false, t0);
        assert!(!w.should_quit(at(600)), "waits for MSFS to start, however long");
        w.update(true, at(60));
        w.update(false, at(100));
        assert!(!w.should_quit(at(110)));
        w.update(true, at(115));
        assert!(!w.should_quit(at(200)), "back within the grace time");
        w.update(false, at(300));
        w.update(false, at(310));
        assert!(!w.should_quit(at(329)));
        assert!(w.should_quit(at(330)));
    }
}
