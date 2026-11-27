//! The engine ties the parts together and owns their lifetimes: the sim
//! driver thread, the pairing task, the relay task, the TechLog runner and presence check, and the GSX
//! runner. The tray and the headless mode are both thin shells around it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::api::{ApiClient, Credential, Features};
use crate::config::Config;
use crate::credentials::CredentialStore;
use crate::frame::{Frame, SimKind};
use crate::gsx::block::Emitter;
use crate::gsx::runner::{self as gsx_runner, GsxFeed, GsxProbe, GsxRunnerCfg, GsxTiming};
use crate::pairing::{self, PairEvent, PollTiming};
use crate::relay::{FrameSlot, Relay, RelayExit, RelayState, RelayTiming};
use crate::sim::{self, SimState, Source};
use crate::techlog::client::{TechlogClient, DEFAULT_BASE};
use crate::techlog::presence::{self, PresenceCfg};
use crate::techlog::runner::{self, RunnerCfg, RunnerTiming};
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
    /// The own-aircraft rate of the last frame: 4 while taxiing, the air
    /// rate (`--hz`) otherwise.
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
    techlog: Option<JoinHandle<()>>,
    presence: Option<JoinHandle<()>>,
    gsx: Option<JoinHandle<()>>,
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
    /// The credential for the TechLog runner; None while signed out.
    credential_tx: watch::Sender<Option<String>>,
    /// `features` of every ticket the relay fetches.
    techlog_features: watch::Sender<Features>,
    /// The "Read TechLog" switch; off until the tray turns it on.
    read_techlog: watch::Sender<bool>,
    /// The short hash of the TechLog snapshot SkyNexus last accepted, put on
    /// every own-ship frame.
    tl: watch::Sender<Option<String>>,
    /// True while a sim is connected with a flight loaded.
    sim_on: watch::Sender<bool>,
    /// The loaded aircraft's title (MSFS `TITLE`); None for X-Plane and replays.
    aircraft: watch::Sender<Option<String>>,
    /// GSX L:vars from MSFS, every 5 s; all None without them.
    sim_extras: watch::Sender<sim::SimExtras>,
    /// The connected sim while a flight is loaded; None otherwise.
    sim_kind: watch::Sender<Option<SimKind>>,
    /// `(features.gsx, features.gsxRead)` of every ticket; both true until a
    /// ticket says otherwise.
    gsx_features: watch::Sender<(bool, bool)>,
    /// The "Read GSX" switch; on unless the tray turned it off.
    read_gsx: watch::Sender<bool>,
    /// What the GSX runner reads, for the own-ship frames.
    gsx: watch::Sender<Option<GsxFeed>>,
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
            credential_tx: watch::channel(None).0,
            techlog_features: watch::channel(Features::default()).0,
            read_techlog: watch::channel(false).0,
            tl: watch::channel(None).0,
            sim_on: watch::channel(false).0,
            aircraft: watch::channel(None).0,
            sim_extras: watch::channel(sim::SimExtras::default()).0,
            sim_kind: watch::channel(None).0,
            gsx_features: watch::channel((true, true)).0,
            read_gsx: watch::channel(true).0,
            gsx: watch::channel(None).0,
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

    /// Turns reading TechLog on or off (the tray's "Read TechLog").
    pub fn set_read_techlog(&self, on: bool) {
        self.read_techlog.send_if_modified(|cur| {
            let changed = *cur != on;
            *cur = on;
            changed
        });
    }

    /// Turns reading GSX on or off (the tray's "Read GSX").
    pub fn set_read_gsx(&self, on: bool) {
        self.read_gsx.send_if_modified(|cur| {
            let changed = *cur != on;
            *cur = on;
            changed
        });
    }

    /// The credential for the TechLog and GSX runners. Signing out also
    /// forgets the last ticket's `features` (`techlog` and `techlogRead`;
    /// `gsx` and `gsxRead` go back to on), so another account waits for its own.
    fn set_credential(&self, c: Option<&Credential>) {
        if c.is_none() {
            self.techlog_features.send_replace(Features::default());
            self.gsx_features.send_replace((true, true));
        }
        self.credential_tx.send_replace(c.map(|c| c.credential.clone()));
    }

    /// Starts the sim driver and, with a stored credential, the relay.
    pub fn start(self: &Arc<Self>) {
        self.start_sim();
        self.forward_relay_state();
        if self.cfg.offline {
            return;
        }
        self.start_techlog();
        // GSX Pro exists only for MSFS on Windows: elsewhere it never probes.
        if cfg!(windows) {
            self.start_gsx(GsxProbe::system());
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

    /// Subscribes to the GSX L:vars the sim driver reads.
    pub fn sim_extras(&self) -> watch::Receiver<sim::SimExtras> {
        self.sim_extras.subscribe()
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
        let tl = self.tl.subscribe();
        let sim_on = self.sim_on.clone();
        let aircraft = self.aircraft.clone();
        let sim_extras = self.sim_extras.clone();
        let sim_kind = self.sim_kind.clone();
        let gsx = self.gsx.subscribe();
        let relay = self.relay_state.subscribe();
        let mut gsx_emitter = Emitter::default();
        let handle = std::thread::Builder::new()
            .name("sim-driver".into())
            .spawn(move || {
                sim::run_driver(
                    source,
                    interval,
                    &stop,
                    |f: &Frame, hz: u32| {
                        let tl = tl.borrow().clone();
                        // Only frames that reach the relay spend the GSX emitter: one built while
                        // the relay is down may be replaced in the slot before it is sent.
                        let sending = !offline && *relay.borrow() == RelayState::Connected;
                        let json = if !sending {
                            f.to_json_with_tl(tl.as_deref())
                        } else {
                            let feed = gsx.borrow().clone();
                            let now = std::time::Instant::now();
                            gsx_runner::frame_json(f, tl.as_deref(), &mut gsx_emitter, feed.as_ref(), now)
                        };
                        if print {
                            println!("{json}");
                        }
                        if !offline {
                            frames.send_replace(Some(Arc::from(json)));
                        }
                        // 4 Hz while taxiing, the air rate otherwise.
                        status.send_if_modified(|st| {
                            let changed = st.hz != hz;
                            st.hz = hz;
                            changed
                        });
                    },
                    |s| {
                        // A sim that left Connected must never have its last
                        // frame re-sent by a relay reconnect.
                        let connected = matches!(s, SimState::Connected { .. });
                        if !connected {
                            frames.send_replace(None);
                        }
                        let kind = match &s {
                            SimState::Connected { kind, .. } => Some(*kind),
                            _ => None,
                        };
                        sim_kind.send_if_modified(|cur| std::mem::replace(cur, kind) != kind);
                        sim_on.send_if_modified(|cur| std::mem::replace(cur, connected) != connected);
                        status.send_modify(|st| st.sim = s)
                    },
                    |title| {
                        aircraft.send_replace(title);
                    },
                    |e| {
                        sim_extras.send_replace(e);
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

    /// Starts the TechLog runner and the presence check. The runner reads
    /// nothing until the switch is on, a credential is there and a ticket
    /// allowed it; the presence check pings only while the runner is off.
    fn start_techlog(self: &Arc<Self>) {
        let cfg = RunnerCfg {
            credential: self.credential_tx.subscribe(),
            features: self.techlog_features.subscribe(),
            timing: RunnerTiming::default(),
        };
        let client = TechlogClient::new(url::Url::parse(DEFAULT_BASE).expect("TechLog URL"));
        let task = self.rt.spawn(runner::run(
            cfg,
            client,
            self.api.clone(),
            self.tl.clone(),
            self.read_techlog.subscribe(),
        ));
        if let Some(old) = self.tasks.lock().unwrap().techlog.replace(task) {
            old.abort();
        }
        let cfg = PresenceCfg {
            credential: self.credential_tx.subscribe(),
            features: self.techlog_features.subscribe(),
            read_switch: self.read_techlog.subscribe(),
            sim: self.sim_on.subscribe(),
            aircraft: self.aircraft.subscribe(),
            every: presence::EVERY,
            title_grace: presence::TITLE_GRACE,
            backoff_max: presence::BACKOFF_MAX,
        };
        let client = TechlogClient::new(url::Url::parse(DEFAULT_BASE).expect("TechLog URL"));
        let task = self.rt.spawn(presence::run(cfg, client, self.api.clone()));
        if let Some(old) = self.tasks.lock().unwrap().presence.replace(task) {
            old.abort();
        }
    }

    /// Starts the GSX runner: detection while MSFS is connected and signed
    /// in, reading while the ticket and the tray allow it.
    fn start_gsx(self: &Arc<Self>, probe: GsxProbe) {
        let posted_file = if self.cfg.ephemeral || self.cfg.offline {
            None
        } else {
            crate::config::app_config_dir().map(|d| d.join(crate::config::GSX_POSTED_FILE))
        };
        let cfg = GsxRunnerCfg {
            credential: self.credential_tx.subscribe(),
            features: self.gsx_features.subscribe(),
            tray: self.read_gsx.subscribe(),
            sim: self.sim_kind.subscribe(),
            extras: self.sim_extras.subscribe(),
            probe,
            posted_file,
            timing: GsxTiming::default(),
        };
        let task = self.rt.spawn(gsx_runner::run(cfg, self.api.clone(), self.gsx.clone()));
        if let Some(old) = self.tasks.lock().unwrap().gsx.replace(task) {
            old.abort();
        }
    }

    fn start_relay(self: &Arc<Self>, c: Credential) {
        *self.credential.lock().unwrap() = Some(c.clone());
        self.set_credential(Some(&c));
        let me = self.clone();
        let relay = Relay {
            cfg: self.cfg.clone(),
            tickets: TicketManager::new(self.api.clone(), c.credential)
                .with_features(self.techlog_features.clone())
                .with_gsx_features(self.gsx_features.clone()),
            frames: self.frames.subscribe(),
            state: self.relay_state.clone(),
            timing: RelayTiming::default(),
        };
        let task = self.rt.spawn(async move {
            match relay.run().await {
                RelayExit::Unauthorized => {
                    log::warn!("the credential was refused; signing out");
                    me.credential.lock().unwrap().take();
                    me.set_credential(None);
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
        self.set_credential(None);
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
        for task in [
            t.pairing.take(),
            t.relay.take(),
            t.techlog.take(),
            t.presence.take(),
            t.gsx.take(),
        ]
        .into_iter()
        .flatten()
        {
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
        let taxiing = Status { hz: 4, ..s.clone() };
        assert_eq!(taxiing.headline(), "Streaming MSFS 4 Hz");
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
            fuel_kg: None,
            callsign: None,
            tl: None,
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
    fn frames_carry_the_techlog_hash_once_there_is_one() {
        let own = Own {
            lat: 50.0,
            lon: 8.5,
            alt_ft: 364.0,
            gs_kt: 0.0,
            hdg: 249.0,
            trk: 249.0,
            vs_fpm: 0.0,
            on_ground: true,
            fuel_kg: None,
            callsign: None,
            tl: None,
        };
        let f = Frame::new(SimKind::Msfs, 1, own);
        assert!(!f.to_json_with_tl(None).contains("\"tl\""));
        let json = f.to_json_with_tl(Some("0123456789ab"));
        assert!(json.contains(r#""tl":"0123456789ab""#), "{json}");
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
        let both = Features {
            techlog: true,
            techlog_read: true,
        };
        engine.techlog_features.send_replace(both);
        engine.gsx_features.send_replace((false, false));
        engine.sign_out();
        assert!(
            *engine.techlog_features.borrow() == Features::default(),
            "a new account waits for its own ticket"
        );
        assert_eq!(*engine.gsx_features.borrow(), (true, true), "GSX back to its default");
        assert_eq!(*engine.credential_tx.borrow(), None);
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
    fn the_gsx_runner_reads_while_signed_in_in_msfs() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (server, port) = rt.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/connector/gsx"))
                .respond_with(ResponseTemplate::new(204))
                .mount(&server)
                .await;
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            tokio::spawn(async move {
                let (s, _) = l.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
                let hello = r#"{"type":"hello","protocol":2,"engine":"4.0.8"}"#;
                let snap = r#"{"type":"snapshot","state":{"airport":{"icao":"EHAM"}}}"#;
                ws.send(Message::text(hello)).await.unwrap();
                ws.send(Message::text(snap)).await.unwrap();
                while let Some(Ok(_)) = ws.next().await {}
            });
            (server, port)
        });
        let cfg = Config::from_cli(<crate::config::Cli as clap::Parser>::parse_from([
            "x",
            "--ephemeral",
            "--base-url",
            &server.uri(),
        ]));
        let store = Arc::new(crate::credentials::MemoryStore::default());
        let engine = Engine::new(cfg, store, rt.handle().clone());
        let probe = GsxProbe {
            installed: Arc::new(|| true),
            port: Arc::new(move || port),
        };
        engine.start_gsx(probe);
        let mut gsx = engine.gsx.subscribe();
        let secs = std::time::Duration::from_secs;
        engine.credential_tx.send_replace(Some("snc_1".into()));
        engine.sim_kind.send_replace(Some(SimKind::Msfs));
        rt.block_on(async {
            let up = tokio::time::timeout(secs(5), gsx.wait_for(Option::is_some));
            let feed = up.await.unwrap().unwrap().clone().unwrap();
            assert_eq!(feed.block.apt.as_deref(), Some("EHAM"));
        });
        engine.sign_out();
        rt.block_on(async {
            let down = tokio::time::timeout(secs(5), gsx.wait_for(Option::is_none));
            down.await.unwrap().unwrap();
        });
        let calls = rt.block_on(server.received_requests()).unwrap();
        let gsx_posts: Vec<_> = calls.iter().filter(|r| r.url.path() == "/api/connector/gsx").collect();
        assert_eq!(gsx_posts.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&gsx_posts[0].body).unwrap();
        assert_eq!(body["health"]["code"], "GSX_OK");
        engine.shutdown();
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
