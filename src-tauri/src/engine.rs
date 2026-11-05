//! The engine ties the parts together and owns their lifetimes: the sim
//! driver thread, the pairing task and the relay task. The tray and the
//! headless mode are both thin shells around it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::api::{ApiClient, Credential};
use crate::config::Config;
use crate::credentials::CredentialStore;
use crate::frame::{Frame, SimKind};
use crate::pairing::{self, PairEvent, PollTiming};
use crate::relay::{FrameSlot, Relay, RelayExit, RelayState, RelayTiming};
use crate::sim::{self, SimState, Source};
use crate::ticket::TicketManager;

#[derive(Debug, Clone, PartialEq)]
pub enum Auth {
    /// `--offline`: no SkyNexus, no relay.
    Offline,
    SignedOut {
        note: Option<String>,
    },
    Pairing {
        user_code: String,
        url: String,
    },
    SignedIn,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub auth: Auth,
    pub sim: SimState,
    pub relay: RelayState,
    pub hz: u32,
}

impl Status {
    /// The first tray line.
    pub fn headline(&self) -> String {
        match &self.auth {
            Auth::SignedOut { .. } => return "Not signed in".into(),
            Auth::Pairing { user_code, .. } => return format!("Sign in with code {user_code}"),
            Auth::Offline | Auth::SignedIn => {}
        }
        let kind = match &self.sim {
            SimState::Searching | SimState::Unavailable { .. } => return "Waiting for sim".into(),
            SimState::Finished => return "Replay finished".into(),
            SimState::NoFlight { kind } => return format!("{} running, no flight loaded", kind.label()),
            SimState::Connected { kind, .. } => *kind,
        };
        match (&self.auth, &self.relay) {
            (Auth::Offline, _) => format!("{} connected (offline)", kind.label()),
            (_, RelayState::Connected) => format!("Streaming {} {} Hz", kind.label(), self.hz),
            (_, RelayState::Connecting | RelayState::Idle) => format!("{} connected, reaching SkyNexus", kind.label()),
            (_, RelayState::Retrying { .. }) => format!("{} connected, SkyNexus link down", kind.label()),
            (_, RelayState::NotConfigured) => format!("{} connected, relay URL invalid", kind.label()),
            (_, RelayState::NotEnabled) => format!("{} connected, not enabled for your account yet", kind.label()),
        }
    }

    /// The second tray line: what to do, or why.
    pub fn detail(&self) -> String {
        match &self.auth {
            Auth::SignedOut { note: Some(n) } => return n.clone(),
            Auth::SignedOut { note: None } => return "Sign in to stream to your map".into(),
            Auth::Pairing { .. } => return "Approve it in the browser window".into(),
            _ => {}
        }
        match (&self.sim, &self.relay) {
            (SimState::Searching, _) => "Start MSFS or X-Plane on this computer".into(),
            (SimState::Unavailable { reason }, _) => reason.clone(),
            (SimState::NoFlight { .. }, _) => "Load a flight to stream to your map".into(),
            (SimState::Connected { detail, .. }, RelayState::Retrying { in_s, .. }) => {
                format!("{detail}, retrying in {in_s} s")
            }
            (SimState::Connected { detail, .. }, _) => detail.clone(),
            (SimState::Finished, _) => "Restart with --replay to play it again".into(),
        }
    }

    pub fn signed_in(&self) -> bool {
        self.auth == Auth::SignedIn
    }

    pub fn streaming_kind(&self) -> Option<SimKind> {
        match (&self.sim, &self.relay) {
            (SimState::Connected { kind, .. }, RelayState::Connected) => Some(*kind),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Tasks {
    pairing: Option<JoinHandle<()>>,
    relay: Option<JoinHandle<()>>,
    sim: Option<std::thread::JoinHandle<()>>,
    /// Signals when the sign-out revoke finished, so Quit can wait for it.
    revoke_done: Option<std::sync::mpsc::Receiver<()>>,
}

/// How long Quit waits for a sign-out revoke still in flight.
pub const REVOKE_WAIT_ON_QUIT: std::time::Duration = std::time::Duration::from_secs(3);

pub struct Engine {
    cfg: Arc<Config>,
    api: ApiClient,
    store: Arc<dyn CredentialStore>,
    status: Arc<watch::Sender<Status>>,
    frames: Arc<watch::Sender<FrameSlot>>,
    relay_state: Arc<watch::Sender<RelayState>>,
    rt: tokio::runtime::Handle,
    tasks: Mutex<Tasks>,
    stop: Arc<AtomicBool>,
    /// The credential in use, kept for sign-out's revoke even if the
    /// keychain cannot be read back then.
    credential: Mutex<Option<Credential>>,
}

impl Engine {
    pub fn new(cfg: Config, store: Arc<dyn CredentialStore>, rt: tokio::runtime::Handle) -> Arc<Self> {
        let auth = if cfg.offline {
            Auth::Offline
        } else {
            Auth::SignedOut { note: None }
        };
        let (status, _) = watch::channel(Status {
            auth,
            sim: SimState::Searching,
            relay: RelayState::Idle,
            hz: cfg.hz,
        });
        let (frames, _) = watch::channel(None);
        let (relay_state, _) = watch::channel(RelayState::Idle);
        Arc::new(Engine {
            api: ApiClient::new(&cfg.base_url),
            cfg: Arc::new(cfg),
            store,
            status: Arc::new(status),
            frames: Arc::new(frames),
            relay_state: Arc::new(relay_state),
            rt,
            tasks: Mutex::default(),
            stop: Arc::new(AtomicBool::new(false)),
            credential: Mutex::new(None),
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }

    pub fn current(&self) -> Status {
        self.status.borrow().clone()
    }

    fn set_auth(&self, auth: Auth) {
        self.status.send_modify(|s| s.auth = auth);
    }

    /// Starts the sim driver and, with a stored credential, the relay.
    pub fn start(self: &Arc<Self>) {
        self.start_sim();
        self.forward_relay_state();
        if self.cfg.offline {
            return;
        }
        match self.store.load() {
            Ok(Some(c)) => {
                self.set_auth(Auth::SignedIn);
                self.start_relay(c);
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!("keychain: {e}");
                self.set_auth(Auth::SignedOut {
                    note: Some("Could not read the keychain; sign in again".into()),
                });
            }
        }
    }

    fn start_sim(self: &Arc<Self>) {
        let source = match &self.cfg.replay {
            Some(path) => Source::Replay {
                path: path.clone(),
                looped: self.cfg.replay_loop,
                speed: self.cfg.replay_speed,
            },
            None => Source::Auto { hz: self.cfg.hz },
        };
        let interval = self.cfg.frame_interval();
        let frames = self.frames.clone();
        let status = self.status.clone();
        let stop = self.stop.clone();
        let print = self.cfg.print_frames;
        let offline = self.cfg.offline;
        let handle = std::thread::Builder::new()
            .name("sim-driver".into())
            .spawn(move || {
                sim::run_driver(
                    source,
                    interval,
                    &stop,
                    |f: &Frame| {
                        let json = f.to_json();
                        if print {
                            println!("{json}");
                        }
                        if !offline {
                            frames.send_replace(Some(Arc::from(json)));
                        }
                    },
                    |s| {
                        // A sim that left Connected must never have its last
                        // frame re-sent by a relay reconnect.
                        if !matches!(s, SimState::Connected { .. }) {
                            frames.send_replace(None);
                        }
                        status.send_modify(|st| st.sim = s)
                    },
                );
            })
            .expect("sim thread");
        self.tasks.lock().unwrap().sim = Some(handle);
    }

    fn forward_relay_state(self: &Arc<Self>) {
        let mut rx = self.relay_state.subscribe();
        let status = self.status.clone();
        self.rt.spawn(async move {
            loop {
                let s = rx.borrow_and_update().clone();
                status.send_if_modified(|st| {
                    let changed = st.relay != s;
                    st.relay = s;
                    changed
                });
                if rx.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    fn start_relay(self: &Arc<Self>, c: Credential) {
        *self.credential.lock().unwrap() = Some(c.clone());
        let me = self.clone();
        let relay = Relay {
            cfg: self.cfg.clone(),
            tickets: TicketManager::new(self.api.clone(), c.credential),
            frames: self.frames.subscribe(),
            state: self.relay_state.clone(),
            timing: RelayTiming::default(),
        };
        let task = self.rt.spawn(async move {
            match relay.run().await {
                RelayExit::Unauthorized => {
                    log::warn!("the credential was refused; signing out");
                    me.credential.lock().unwrap().take();
                    if let Err(e) = me.store.clear() {
                        log::warn!("keychain: {e}");
                    }
                    me.relay_state.send_replace(RelayState::Idle);
                    me.set_auth(Auth::SignedOut {
                        note: Some("This computer was disconnected from your account".into()),
                    });
                }
            }
        });
        if let Some(old) = self.tasks.lock().unwrap().relay.replace(task) {
            old.abort();
        }
    }

    /// Starts pairing unless it is already running or the connector is signed in.
    pub fn sign_in(self: &Arc<Self>) {
        if self.cfg.offline || self.current().signed_in() {
            return;
        }
        let mut tasks = self.tasks.lock().unwrap();
        if tasks.pairing.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let me = self.clone();
        tasks.pairing = Some(self.rt.spawn(async move { me.run_pairing().await }));
    }

    async fn run_pairing(self: Arc<Self>) {
        let name = pairing::device_name();
        let result = pairing::pair(
            &self.api,
            &*self.store,
            &name,
            self.cfg.open_browser,
            PollTiming::default(),
            |event| match event {
                PairEvent::Code { start, url } => {
                    log::info!("pairing: enter code {} at {url}", start.user_code);
                    if self.cfg.headless {
                        println!(
                            "\nTo connect this computer, open {url}\nand check that the code is {}\n",
                            start.user_code
                        );
                    }
                    self.set_auth(Auth::Pairing {
                        user_code: start.user_code.clone(),
                        url: url.to_string(),
                    });
                }
                PairEvent::Waiting { wait, .. } => self.set_auth(Auth::SignedOut {
                    note: Some(format!("SkyNexus is busy; retrying sign-in in {} s", wait.as_secs())),
                }),
            },
        )
        .await;
        match result {
            Ok(c) => {
                log::info!("pairing: signed in as device {}", c.device_id.as_deref().unwrap_or("?"));
                self.set_auth(Auth::SignedIn);
                self.start_relay(c);
            }
            Err(e) => {
                log::warn!("pairing: {e}");
                self.set_auth(Auth::SignedOut {
                    note: Some(capitalise(&e.to_string())),
                });
            }
        }
    }

    pub fn cancel_sign_in(&self) {
        if let Some(t) = self.tasks.lock().unwrap().pairing.take() {
            t.abort();
        }
        if matches!(self.current().auth, Auth::Pairing { .. }) {
            self.set_auth(Auth::SignedOut { note: None });
        }
    }

    /// Stops streaming, forgets the credential and asks SkyNexus to revoke it.
    pub fn sign_out(self: &Arc<Self>) {
        let (pairing, relay) = {
            let mut t = self.tasks.lock().unwrap();
            (t.pairing.take(), t.relay.take())
        };
        pairing.into_iter().chain(relay).for_each(|t| t.abort());
        let credential = self
            .credential
            .lock()
            .unwrap()
            .take()
            .or_else(|| self.store.load().ok().flatten());
        if let Err(e) = self.store.clear() {
            log::warn!("keychain: {e}");
        }
        self.relay_state.send_replace(RelayState::Idle);
        self.set_auth(Auth::SignedOut { note: None });
        if let Some(c) = credential {
            let api = self.api.clone();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            self.tasks.lock().unwrap().revoke_done = Some(done_rx);
            self.rt.spawn(async move {
                if let Err(e) = api.revoke(&c.credential).await {
                    log::warn!("revoke: {e} (revoke it in the account settings)");
                }
                let _ = done_tx.send(());
            });
        }
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut t = self.tasks.lock().unwrap();
        for task in [t.pairing.take(), t.relay.take()].into_iter().flatten() {
            task.abort();
        }
        let revoke = t.revoke_done.take();
        let sim = t.sim.take();
        drop(t);
        // Sign out then Quit at once: give the revoke a moment to reach SkyNexus.
        if let Some(rx) = revoke {
            if rx.recv_timeout(REVOKE_WAIT_ON_QUIT).is_err() {
                log::warn!("revoke still pending at quit; revoke the device in the account settings");
            }
        }
        if let Some(sim) = sim {
            let _ = sim.join();
        }
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Own;

    fn status(auth: Auth, sim: SimState, relay: RelayState) -> Status {
        Status {
            auth,
            sim,
            relay,
            hz: 1,
        }
    }

    fn msfs() -> SimState {
        SimState::Connected {
            kind: SimKind::Msfs,
            detail: "MSFS (SimConnect)".into(),
        }
    }

    #[test]
    fn tray_lines() {
        let s = status(Auth::SignedOut { note: None }, msfs(), RelayState::Idle);
        assert_eq!(s.headline(), "Not signed in");
        let s = status(Auth::SignedIn, SimState::Searching, RelayState::Connected);
        assert_eq!(s.headline(), "Waiting for sim");
        let s = status(Auth::SignedIn, msfs(), RelayState::Connected);
        assert_eq!(s.headline(), "Streaming MSFS 1 Hz");
        assert_eq!(s.streaming_kind(), Some(SimKind::Msfs));
        let s = status(
            Auth::SignedIn,
            msfs(),
            RelayState::Retrying {
                reason: "x".into(),
                in_s: 4,
            },
        );
        assert_eq!(s.headline(), "MSFS connected, SkyNexus link down");
        assert_eq!(s.detail(), "MSFS (SimConnect), retrying in 4 s");
        let s = status(
            Auth::Pairing {
                user_code: "ABCD-EFGH".into(),
                url: "u".into(),
            },
            SimState::Searching,
            RelayState::Idle,
        );
        assert_eq!(s.headline(), "Sign in with code ABCD-EFGH");
        let s = status(Auth::Offline, msfs(), RelayState::Idle);
        assert_eq!(s.headline(), "MSFS connected (offline)");
        let s = status(
            Auth::SignedIn,
            SimState::Unavailable {
                reason: crate::sim::msfs_proto::MISSING_DLL_HINT.into(),
            },
            RelayState::Idle,
        );
        assert_eq!(s.headline(), "Waiting for sim");
        assert!(s.detail().starts_with("SimConnect.dll missing next to the app"));
        let s = status(
            Auth::SignedIn,
            SimState::NoFlight { kind: SimKind::Msfs },
            RelayState::Connected,
        );
        assert_eq!(s.headline(), "MSFS running, no flight loaded");
        assert_eq!(s.detail(), "Load a flight to stream to your map");
        assert_eq!(s.streaming_kind(), None, "nothing to stream from the menus");
    }

    fn short_replay() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("snx-engine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("short.jsonl");
        let own = Own {
            lat: 50.0,
            lon: 8.5,
            alt_ft: 364.0,
            gs_kt: 0.0,
            hdg: 249.0,
            trk: 249.0,
            vs_fpm: 0.0,
            on_ground: true,
            callsign: None,
        };
        let lines: Vec<String> = (0..3)
            .map(|i| Frame::new(SimKind::Replay, i * 100, own.clone()).to_json())
            .collect();
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    #[test]
    fn the_frame_slot_is_cleared_when_the_sim_goes_away() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let path = short_replay();
        let cfg = Config::from_cli(<crate::config::Cli as clap::Parser>::parse_from([
            "x",
            "--ephemeral",
            "--replay-once",
            "--replay-speed",
            "100",
            "--replay",
            path.to_str().unwrap(),
        ]));
        let engine = Engine::new(
            cfg,
            Arc::new(crate::credentials::MemoryStore::default()),
            rt.handle().clone(),
        );
        let mut frames = engine.frames.subscribe();
        engine.start();
        let mut rx = engine.status();
        rt.block_on(async {
            // a frame arrives while the replay plays
            tokio::time::timeout(std::time::Duration::from_secs(5), frames.wait_for(|f| f.is_some()))
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                rx.wait_for(|s| s.sim == SimState::Finished),
            )
            .await
            .unwrap()
            .unwrap();
        });
        assert!(engine.frames.borrow().is_none(), "no stale frame left for a reconnect");
        engine.shutdown();
    }

    #[test]
    fn sign_out_revokes_with_the_kept_credential_and_quit_waits_for_it() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let server = rt.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("DELETE"))
                .and(path("/api/connector/devices"))
                .and(header("authorization", "Bearer snc_kept"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_delay(std::time::Duration::from_millis(400))
                        .set_body_json(serde_json::json!({ "ok": true })),
                )
                .mount(&server)
                .await;
            server
        });
        let cfg = Config::from_cli(<crate::config::Cli as clap::Parser>::parse_from([
            "x",
            "--ephemeral",
            "--base-url",
            &server.uri(),
        ]));
        // The keychain is empty: only the in-memory copy can be revoked.
        let engine = Engine::new(
            cfg,
            Arc::new(crate::credentials::MemoryStore::default()),
            rt.handle().clone(),
        );
        *engine.credential.lock().unwrap() = Some(Credential {
            credential: "snc_kept".into(),
            device_id: None,
        });
        engine.sign_out();
        let started = std::time::Instant::now();
        engine.shutdown();
        let waited = started.elapsed();
        assert!(
            waited >= std::time::Duration::from_millis(300),
            "quit waited for the revoke ({waited:?})"
        );
        assert!(waited < REVOKE_WAIT_ON_QUIT + std::time::Duration::from_secs(1));
        let calls = rt.block_on(server.received_requests()).unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn offline_replay_runs_without_any_network() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/eddf-departure.json");
        let cfg = Config::from_cli(<crate::config::Cli as clap::Parser>::parse_from([
            "x",
            "--offline",
            "--replay",
            path.to_str().unwrap(),
            "--replay-speed",
            "20",
        ]));
        let engine = Engine::new(
            cfg,
            Arc::new(crate::credentials::MemoryStore::default()),
            rt.handle().clone(),
        );
        engine.start();
        let mut rx = engine.status();
        rt.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                rx.wait_for(|s| matches!(s.sim, SimState::Connected { .. })),
            )
            .await
            .unwrap()
            .unwrap();
        });
        assert_eq!(engine.current().headline(), "Replay connected (offline)");
        engine.sign_in(); // a no-op offline
        assert_eq!(engine.current().auth, Auth::Offline);
        engine.shutdown();
    }
}
