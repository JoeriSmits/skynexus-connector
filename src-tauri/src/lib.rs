//! SkyNexus Connector: streams the user's own simulator aircraft (and the
//! traffic the sim knows around it) to that user's SkyNexus enroute map.
//!
//! Layout: `frame` (wire schema), `api` (web app routes), `pairing`,
//! `credentials`, `ticket`, `relay`, `sim` (adapters and pacing), `engine`
//! (lifetimes), `tray` (UI), `config` (flags), `msfs_launch` (start and stop
//! with MSFS), `logs` (the log file).

pub mod api;
pub mod config;
pub mod credentials;
pub mod engine;
pub mod frame;
pub mod logs;
pub mod msfs_launch;
pub mod pairing;
pub mod relay;
pub mod sim;
pub mod ticket;
mod tray;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use config::Config;
use credentials::{CredentialStore, KeyringStore, MemoryStore};
use engine::Engine;
use frame::SimKind;
use sim::SimState;

/// Logs to stderr and to the log file (see `logs`).
fn init_logging() {
    let file = logs::log_dir().and_then(|d| logs::open(&d));
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,tao=warn,wry=warn"))
        .format_timestamp_secs()
        .target(env_logger::Target::Pipe(Box::new(logs::Tee::new(file))))
        .init();
}

/// The uninstaller's step: take our entry out of every MSFS exe.xml.
fn remove_msfs_entries() {
    for path in msfs_launch::installed_exe_xmls() {
        match msfs_launch::unregister_in(&path) {
            Ok(true) => log::info!("start with MSFS: entry removed from {}", path.display()),
            Ok(false) => {}
            Err(e) => log::warn!("start with MSFS: {}: {e}", path.display()),
        }
    }
}

fn store_for(cfg: &Config) -> Arc<dyn CredentialStore> {
    if cfg.ephemeral || cfg.offline {
        Arc::new(MemoryStore::default())
    } else {
        Arc::new(KeyringStore::new(cfg.keychain_account()))
    }
}

/// Entry point for the binary.
pub fn run() {
    let cfg = Config::from_env_args();
    init_logging();
    if cfg.remove_msfs_entry {
        remove_msfs_entries();
        return;
    }
    log::info!(
        "SkyNexus Connector {} ({}{})",
        env!("CARGO_PKG_VERSION"),
        cfg.base_url,
        cfg.replay
            .as_ref()
            .map(|p| format!(", replay {}", p.display()))
            .unwrap_or_default()
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("connector")
        .build()
        .expect("tokio runtime");
    let store = store_for(&cfg);
    let headless = cfg.headless;
    let engine = Engine::new(cfg, store, rt.handle().clone());
    if headless {
        rt.block_on(run_headless(engine));
        return;
    }
    run_tray(&rt, engine);
}

/// Logs each change of the tray lines, in both modes.
async fn log_status(engine: Arc<Engine>) {
    let mut rx = engine.status();
    let mut last = String::new();
    loop {
        let line = {
            let s = rx.borrow_and_update();
            format!("{} | {}", s.headline(), s.detail())
        };
        if line != last {
            log::info!("status: {line}");
            last = line;
        }
        if rx.changed().await.is_err() {
            break;
        }
    }
}

/// For a connector that MSFS started: returns once MSFS has closed.
async fn msfs_closed(engine: Arc<Engine>) {
    let mut rx = engine.status();
    let mut watch = msfs_launch::QuitWatch::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let connected = matches!(
            rx.borrow_and_update().sim,
            SimState::Connected {
                kind: SimKind::Msfs,
                ..
            } | SimState::NoFlight { kind: SimKind::Msfs }
        );
        let now = Instant::now();
        watch.update(connected, now);
        if watch.should_quit(now) {
            log::info!("MSFS has closed; quitting, as MSFS started this connector");
            return;
        }
    }
}

/// Set once this copy follows MSFS's lifetime, so it never does so twice.
static FOLLOWING_MSFS: AtomicBool = AtomicBool::new(false);

/// From now on, quit once MSFS has closed (tray mode).
fn follow_msfs(handle: tauri::AppHandle, engine: Arc<Engine>, why: &str) {
    if FOLLOWING_MSFS.swap(true, Ordering::SeqCst) {
        return;
    }
    log::info!("{why}");
    tauri::async_runtime::spawn(async move {
        msfs_closed(engine).await;
        handle.exit(0);
    });
}

/// What a running copy does when another is started with `argv`: when MSFS
/// started it, take over its job (quit with MSFS), unless this copy runs
/// because of "Start at login".
fn on_second_copy(app: &tauri::AppHandle, argv: &[String]) {
    use tauri::Manager;
    let by_msfs = argv.iter().any(|a| a == msfs_launch::LAUNCH_FLAG);
    match app.try_state::<Arc<Engine>>() {
        Some(engine) if by_msfs && !engine.config().at_login => follow_msfs(
            app.clone(),
            engine.inner().clone(),
            "MSFS started a second copy; this one takes over and quits when MSFS closes",
        ),
        _ => log::info!("a second copy was started; this one keeps running"),
    }
}

async fn run_headless(engine: Arc<Engine>) {
    engine.start();
    tokio::spawn(log_status(engine.clone()));
    if !engine.config().offline && !engine.current().signed_in() {
        engine.sign_in();
    }
    let follow_msfs = engine.config().launched_by_msfs;
    let closed = engine.clone();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = msfs_closed(closed), if follow_msfs => {}
    }
    let e = engine.clone();
    let _ = tokio::task::spawn_blocking(move || e.shutdown()).await;
}

fn run_tray(rt: &tokio::runtime::Runtime, engine: Arc<Engine>) {
    use tauri::RunEvent;
    use tauri_plugin_autostart::MacosLauncher;

    tauri::async_runtime::set(rt.handle().clone());
    let setup_engine = engine.clone();
    let pending: PendingUpdate = Arc::default();
    let setup_pending = pending.clone();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            on_second_copy(app, &argv)
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--at-login"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            use tauri::Manager;
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            app.manage(setup_engine.clone());
            let entry = msfs_entry(app, setup_engine.config());
            let msfs_on = entry.as_ref().map(|(dir, exe)| msfs_launch::apply(dir, exe));
            tray::build(app, setup_engine.clone(), entry.zip(msfs_on))?;
            setup_engine.start();
            tauri::async_runtime::spawn(log_status(setup_engine.clone()));
            if setup_engine.config().launched_by_msfs {
                follow_msfs(
                    app.handle().clone(),
                    setup_engine.clone(),
                    "started by MSFS; quits when MSFS closes",
                );
            }
            if updates_configured(app) {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(update_loop(handle, setup_engine.clone(), setup_pending.clone()));
            } else {
                log::info!("updater: no public key configured, update checks are off");
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("tauri app");
    app.run(move |_app, event| match event {
        // Tray only: there are no windows, so never exit for want of one.
        RunEvent::ExitRequested { api, code, .. } if code.is_none() => api.prevent_exit(),
        RunEvent::Exit => {
            engine.shutdown();
            // A downloaded update waits for Quit when a sim was connected.
            if let Some((update, bytes)) = pending.lock().unwrap().take() {
                log::info!("updater: installing {} on quit", update.version);
                if let Err(e) = update.install(bytes) {
                    log::warn!("updater: install failed: {e}");
                }
            }
        }
        _ => {}
    });
}

/// The "Start with MSFS" setting's folder and the exe the entry starts: on
/// Windows, for a production run only.
fn msfs_entry(app: &tauri::App, cfg: &Config) -> Option<(PathBuf, PathBuf)> {
    use tauri::Manager;
    if !cfg!(windows) || !cfg.is_production_run() {
        return None;
    }
    let dir = app.path().app_config_dir().ok()?;
    let exe = std::env::current_exe().ok()?;
    Some((dir, exe))
}

/// Update checks run only in release builds with a real updater key.
fn updates_configured(app: &tauri::App) -> bool {
    if cfg!(debug_assertions) {
        return false;
    }
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|u| u.get("pubkey"))
        .and_then(|p| p.as_str())
        .is_some_and(|k| !k.is_empty() && !k.starts_with("REPLACE"))
}

/// A downloaded update not yet installed.
type PendingUpdate = Arc<std::sync::Mutex<Option<(tauri_plugin_updater::Update, Vec<u8>)>>>;

/// Installing restarts the app, which would cut a live stream: only while
/// no sim is connected (a sim in its menus counts as connected; a flight may
/// load any moment). Otherwise the update waits for that, or for Quit.
fn may_install_now(sim: &sim::SimState) -> bool {
    !matches!(sim, sim::SimState::Connected { .. } | sim::SimState::NoFlight { .. })
}

async fn update_loop(app: tauri::AppHandle, engine: Arc<Engine>, pending: PendingUpdate) {
    use tauri_plugin_updater::UpdaterExt;
    loop {
        if pending.lock().unwrap().is_none() {
            match app.updater() {
                Ok(updater) => match updater.check().await {
                    Ok(Some(update)) => match update.download(|_, _| {}, || {}).await {
                        Ok(bytes) => {
                            log::info!(
                                "updater: {} downloaded; installs when no sim is connected",
                                update.version
                            );
                            *pending.lock().unwrap() = Some((update, bytes));
                        }
                        Err(e) => log::warn!("updater: download failed: {e}"),
                    },
                    Ok(None) => log::debug!("updater: up to date"),
                    Err(e) => log::warn!("updater: check failed: {e}"),
                },
                Err(e) => log::warn!("updater: {e}"),
            }
        }
        // Check every 30 s whether a downloaded update can go in; look for a
        // new one every 6 hours.
        for _ in 0..(6 * 120) {
            if may_install_now(&engine.current().sim) {
                let ready = pending.lock().unwrap().take();
                if let Some((update, bytes)) = ready {
                    log::info!("updater: installing {}", update.version);
                    match update.install(bytes) {
                        Ok(()) => app.restart(),
                        Err(e) => log::warn!("updater: install failed: {e}"),
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_wait_for_the_sim_to_disconnect() {
        assert!(may_install_now(&sim::SimState::Searching));
        assert!(may_install_now(&sim::SimState::Finished));
        assert!(!may_install_now(&sim::SimState::Connected {
            kind: frame::SimKind::Msfs,
            detail: "MSFS".into(),
        }));
        assert!(!may_install_now(&sim::SimState::NoFlight {
            kind: frame::SimKind::Msfs,
        }));
    }
}
