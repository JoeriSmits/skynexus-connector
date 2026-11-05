//! The tray menu: status, Sign in / Sign out, Open SkyNexus, Start with MSFS
//! (Windows), Start at login, Open logs, Quit.

use std::path::PathBuf;
use std::sync::Arc;

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{App, AppHandle, Wry};
use tauri_plugin_autostart::ManagerExt;

use crate::engine::{Auth, Engine, Status};
use crate::{logs, msfs_launch};

/// The summary at the top of diagnostics.txt: what support needs to know,
/// nothing secret (no credential, ticket or pairing code).
fn diagnostics(app: &AppHandle, engine: &Engine, msfs: Option<&(PathBuf, PathBuf)>) -> String {
    let s = engine.current();
    let cfg = engine.config();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut lines = vec![
        format!("SkyNexus Connector {}", env!("CARGO_PKG_VERSION")),
        format!("os: {} {}", std::env::consts::OS, std::env::consts::ARCH),
        format!("exe: {exe}"),
        format!("web app: {}", cfg.base_url),
        format!("status: {} | {}", s.headline(), s.detail()),
        format!("sim: {:?}", s.sim),
        format!("relay: {:?}", s.relay),
        format!("signed in: {}", s.signed_in()),
        format!(
            "this copy: started by MSFS {}, at login {}",
            cfg.launched_by_msfs, cfg.at_login
        ),
        format!("start at login: {}", app.autolaunch().is_enabled().unwrap_or(false)),
    ];
    match msfs {
        Some((dir, _)) => {
            let on = msfs_launch::wanted(msfs_launch::read_setting(dir).as_deref());
            lines.push(format!("start with MSFS: {on}"));
            for p in msfs_launch::installed_exe_xmls() {
                let entry = std::fs::read_to_string(&p).is_ok_and(|x| msfs_launch::has_entry(&x));
                lines.push(format!(
                    "  {}: {}",
                    p.display(),
                    if entry { "entry present" } else { "no entry" }
                ));
            }
        }
        None => lines.push("start with MSFS: not available in this run".into()),
    }
    lines.join("\n")
}

/// The setting folder and exe for "Start with MSFS", and whether it is on.
pub type MsfsEntry = ((PathBuf, PathBuf), bool);

struct Items {
    status: MenuItem<Wry>,
    detail: MenuItem<Wry>,
    sign_in: MenuItem<Wry>,
    sign_out: MenuItem<Wry>,
}

impl Items {
    fn show(&self, s: &Status) {
        let _ = self.status.set_text(s.headline());
        let _ = self.detail.set_text(s.detail());
        let (can_in, can_out, in_label) = match &s.auth {
            Auth::Offline => (false, false, "Sign in"),
            Auth::SignedOut { .. } => (true, false, "Sign in"),
            Auth::Pairing { .. } => (true, false, "Cancel sign in"),
            Auth::SignedIn => (false, true, "Sign in"),
        };
        let _ = self.sign_in.set_text(in_label);
        let _ = self.sign_in.set_enabled(can_in);
        let _ = self.sign_out.set_enabled(can_out);
    }
}

pub fn build(app: &App, engine: Arc<Engine>, msfs: Option<MsfsEntry>) -> tauri::Result<()> {
    let handle = app.handle();
    let first = engine.current();
    let items = Items {
        status: MenuItem::with_id(handle, "status", first.headline(), false, None::<&str>)?,
        detail: MenuItem::with_id(handle, "detail", first.detail(), false, None::<&str>)?,
        sign_in: MenuItem::with_id(handle, "sign_in", "Sign in", true, None::<&str>)?,
        sign_out: MenuItem::with_id(handle, "sign_out", "Sign out", false, None::<&str>)?,
    };
    let open = MenuItem::with_id(handle, "open", "Open SkyNexus", true, None::<&str>)?;
    let autostart_on = handle.autolaunch().is_enabled().unwrap_or(false);
    let autostart = CheckMenuItem::with_id(handle, "autostart", "Start at login", true, autostart_on, None::<&str>)?;
    let with_msfs = match &msfs {
        Some((_, on)) => Some(CheckMenuItem::with_id(
            handle,
            "with_msfs",
            "Start with MSFS",
            true,
            *on,
            None::<&str>,
        )?),
        None => None,
    };
    let open_logs = MenuItem::with_id(handle, "logs", "Open logs", true, None::<&str>)?;
    let quit = MenuItem::with_id(handle, "quit", "Quit SkyNexus Connector", true, None::<&str>)?;
    let separator_top = PredefinedMenuItem::separator(handle)?;
    let separator_bottom = PredefinedMenuItem::separator(handle)?;
    let mut entries: Vec<&dyn tauri::menu::IsMenuItem<Wry>> = vec![
        &items.status,
        &items.detail,
        &separator_top,
        &items.sign_in,
        &items.sign_out,
        &open,
    ];
    if let Some(item) = &with_msfs {
        entries.push(item);
    }
    entries.extend([
        &autostart as &dyn tauri::menu::IsMenuItem<Wry>,
        &open_logs,
        &separator_bottom,
        &quit,
    ]);
    let menu = Menu::with_items(handle, &entries)?;
    items.show(&first);

    let mut builder = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip(format!("SkyNexus Connector: {}", first.headline()));
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let on_menu = engine.clone();
    let autostart_item = autostart.clone();
    let msfs_paths = msfs.map(|(paths, _)| paths);
    let tray = builder
        .on_menu_event(move |app: &AppHandle, event| match event.id().as_ref() {
            "sign_in" => {
                if matches!(on_menu.current().auth, Auth::Pairing { .. }) {
                    on_menu.cancel_sign_in();
                } else {
                    on_menu.sign_in();
                }
            }
            "sign_out" => on_menu.sign_out(),
            "open" => {
                if let Err(e) = open::that_detached(&on_menu.config().base_url) {
                    log::warn!("open browser: {e}");
                }
            }
            "autostart" => {
                let launcher = app.autolaunch();
                let result = if launcher.is_enabled().unwrap_or(false) {
                    launcher.disable()
                } else {
                    launcher.enable()
                };
                if let Err(e) = result {
                    log::warn!("start at login: {e}");
                }
                let _ = autostart_item.set_checked(launcher.is_enabled().unwrap_or(false));
            }
            "with_msfs" => {
                if let (Some((dir, exe)), Some(item)) = (&msfs_paths, &with_msfs) {
                    let on = !msfs_launch::wanted(msfs_launch::read_setting(dir).as_deref());
                    let _ = item.set_checked(msfs_launch::set(dir, exe, on));
                }
            }
            "logs" => match logs::log_dir() {
                Some(dir) => {
                    if let Err(e) = logs::write_diagnostics(&dir, &diagnostics(app, &on_menu, msfs_paths.as_ref())) {
                        log::warn!("diagnostics: {e}");
                    }
                    if let Err(e) = open::that_detached(&dir) {
                        log::warn!("open log folder: {e}");
                    }
                }
                None => log::warn!("no log folder on this system"),
            },
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    // Keep the menu in step with the engine.
    let mut rx = engine.status();
    tauri::async_runtime::spawn(async move {
        while rx.changed().await.is_ok() {
            let s = rx.borrow_and_update().clone();
            items.show(&s);
            let _ = tray.set_tooltip(Some(format!("SkyNexus Connector: {}", s.headline())));
        }
    });
    Ok(())
}
