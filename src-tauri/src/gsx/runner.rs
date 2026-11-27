//! The GSX runner: detects GSX Pro while MSFS is connected, posts the
//! health to SkyNexus when it changed, and reads GSX for the relay frames.
//!
//! - Detects at once after MSFS connects (a flight loaded) and every 30 s
//!   after that; X-Plane and replays never probe and never post. While the
//!   socket is being read, detection does not open a second one.
//! - POSTs `{health: {code, connectorVersion, schema: 1, gsxVersion?, reading}}`
//!   only when (code, gsxVersion, reading) changed, and once after sign-in.
//!   The last accepted value is kept on disk with a fingerprint of the
//!   credential, so a restart without a change posts nothing. A GSX that is
//!   not up yet in the first [`GsxTiming::settle`] after MSFS connected is
//!   not posted: Couatl starts some seconds after the flight loads.
//! - Reads while `features.gsx && features.gsxRead && tray`, signed in and
//!   in MSFS. The read loop runs in its own task with a fresh [`GsxState`]
//!   per connection, aborted when any of that stops; the current block and
//!   gate list go on `out` for the frame builder ([`frame_json`]).
//! - A 503 `disabled` stops posting until the next ticket; a 401 until the
//!   credential changes; a 400 is not resent for the same health.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Instant};

use super::block::{block, parkings, Emitter, GsxBlock, GsxParkings};
use super::client::GsxClient;
use super::detect::{classify, GsxCode, Hello};
use super::state::GsxState;
use crate::api::{ApiClient, ApiError};
use crate::config::{credential_fingerprint, load_gsx_posted, save_gsx_posted, GsxPosted};
use crate::frame::{Frame, SimKind};
use crate::sim::SimExtras;

/// The health object's schema version.
pub const HEALTH_SCHEMA: u8 = 1;
/// The server's cap on `gsxVersion`, in UTF-16 code units.
const MAX_VERSION: usize = 24;
/// The longest `Retry-After` honoured from the GSX route.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);

/// What the frame builder puts on the next frames: the block and GSX's
/// gate list for its airport.
#[derive(Debug, Clone, PartialEq)]
pub struct GsxFeed {
    pub block: GsxBlock,
    pub parkings: Option<GsxParkings>,
}

/// How the runner finds GSX: installed on this PC, and the Remote API port.
#[derive(Clone)]
pub struct GsxProbe {
    pub installed: Arc<dyn Fn() -> bool + Send + Sync>,
    pub port: Arc<dyn Fn() -> u16 + Send + Sync>,
}

impl GsxProbe {
    /// The ini and registry of this PC, and the port in the ini.
    pub fn system() -> Self {
        GsxProbe {
            installed: Arc::new(super::detect::installed_here),
            port: Arc::new(|| super::port::remote_port(&super::port::ini_path())),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GsxTiming {
    /// Between detections while MSFS is connected.
    pub detect_every: Duration,
    /// After MSFS connected, `GSX_NOT_RUNNING` and `GSX_NO_API` wait this
    /// long before they are posted.
    pub settle: Duration,
    /// After the GSX socket dropped, before the next detection.
    pub reconnect_after: Duration,
}

impl Default for GsxTiming {
    fn default() -> Self {
        GsxTiming {
            detect_every: Duration::from_secs(30),
            settle: Duration::from_secs(90),
            reconnect_after: Duration::from_secs(2),
        }
    }
}

pub struct GsxRunnerCfg {
    /// The connector credential; None while signed out.
    pub credential: watch::Receiver<Option<String>>,
    /// `(features.gsx, features.gsxRead)` of every ticket.
    pub features: watch::Receiver<(bool, bool)>,
    /// The tray's "Read GSX".
    pub tray: watch::Receiver<bool>,
    /// The connected sim with a flight loaded; None otherwise.
    pub sim: watch::Receiver<Option<SimKind>>,
    /// GSX L:vars from MSFS.
    pub extras: watch::Receiver<SimExtras>,
    pub probe: GsxProbe,
    /// Where the last posted health is kept; None keeps it in memory only.
    pub posted_file: Option<PathBuf>,
    pub timing: GsxTiming,
}

fn code_str(c: GsxCode) -> &'static str {
    match c {
        GsxCode::NotInstalled => "GSX_NOT_INSTALLED",
        GsxCode::NotRunning => "GSX_NOT_RUNNING",
        GsxCode::NoApi => "GSX_NO_API",
        GsxCode::Auth => "GSX_AUTH",
        GsxCode::Ok => "GSX_OK",
    }
}

/// GSX's `engine` string as the server takes it (1..24 UTF-16 units, no
/// control chars); anything else is left out rather than cut.
fn version_of(engine: &str) -> Option<String> {
    let v = engine.trim();
    let ok = !v.is_empty() && v.encode_utf16().count() <= MAX_VERSION && !v.chars().any(char::is_control);
    ok.then(|| v.to_string())
}

/// The POST body. `gsxVersion` is left out when there is none, never null.
fn body(p: &GsxPosted) -> Value {
    let mut h = Map::new();
    h.insert("code".into(), json!(code_str(p.code)));
    h.insert("connectorVersion".into(), json!(env!("CARGO_PKG_VERSION")));
    h.insert("schema".into(), json!(HEALTH_SCHEMA));
    if let Some(v) = &p.gsx_version {
        h.insert("gsxVersion".into(), json!(v));
    }
    h.insert("reading".into(), json!(p.reading));
    json!({ "health": h })
}

fn publish(out: &watch::Sender<Option<GsxFeed>>, v: Option<GsxFeed>) {
    out.send_if_modified(|cur| {
        let changed = *cur != v;
        *cur = v;
        changed
    });
}

/// The read loop's task; dropping it aborts the loop (and closes the socket).
struct Reader {
    task: JoinHandle<()>,
    version: Option<String>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Resolves when the read loop ended by itself (the socket dropped).
async fn ended(reader: &mut Option<Reader>) {
    match reader {
        Some(r) => {
            let _ = (&mut r.task).await;
        }
        None => std::future::pending().await,
    }
}

/// Reads one GSX session into a fresh state and publishes the feed after
/// every message (and every L:var change, for the pax fallback). Clears the
/// feed when the socket closes.
async fn read_loop(
    mut client: GsxClient,
    hello: Value,
    mut extras: watch::Receiver<SimExtras>,
    out: watch::Sender<Option<GsxFeed>>,
) {
    // `connect` consumed the hello: seed the new session's state with it.
    let mut state = GsxState::default();
    state.apply(&hello);
    let mut has_state = false;
    loop {
        tokio::select! {
            msg = client.next() => {
                let Some(msg) = msg else { break };
                let kind = msg.get("type").and_then(Value::as_str);
                has_state |= matches!(kind, Some("snapshot" | "patch"));
                state.apply(&msg);
            }
            r = extras.changed() => if r.is_err() { break },
        }
        if has_state {
            let feed = GsxFeed {
                block: block(&state, &extras.borrow(), None),
                parkings: parkings(&state),
            };
            publish(&out, Some(feed));
        }
    }
    publish(&out, None);
}

/// One detection. With a readable GSX, also the open client and its hello,
/// for the read loop.
async fn probe(p: &GsxProbe, couatl: Option<bool>) -> (GsxCode, Option<String>, Option<(GsxClient, Value)>) {
    if !(p.installed)() {
        return (GsxCode::NotInstalled, None, None);
    }
    if couatl == Some(false) {
        return (GsxCode::NotRunning, None, None);
    }
    let (hello, client) = match GsxClient::connect((p.port)()).await {
        Ok((c, h)) => (h, Some(c)),
        Err(h) => (h, None),
    };
    let code = classify(true, couatl, hello.clone());
    match hello {
        Hello::Valid { protocol, engine, .. } => {
            let version = version_of(&engine);
            let seed = json!({"type": "hello", "protocol": protocol, "engine": engine});
            let client = client.filter(|_| code == GsxCode::Ok).map(|c| (c, seed));
            (code, version, client)
        }
        _ => (code, None, None),
    }
}

enum Wake {
    Closed,
    Ticket,
    ReaderEnded,
    Other,
}

/// Runs until a watch sender is dropped. Abort the task to stop it.
pub async fn run(mut cfg: GsxRunnerCfg, api: ApiClient, out: watch::Sender<Option<GsxFeed>>) {
    let t = cfg.timing;
    let mut posted = cfg.posted_file.as_deref().and_then(load_gsx_posted);
    let mut reader: Option<Reader> = None;
    // The credential SkyNexus refused with 401; skip it until it changes.
    let mut refused: Option<String> = None;
    // 503 `disabled`: wait for the next ticket.
    let mut server_off = false;
    // A health the server refused with 400; not sent again.
    let mut invalid: Option<GsxPosted> = None;
    // `Retry-After` of the last failed post.
    let mut hold_until: Option<Instant> = None;
    let mut post_failing = false;
    let mut msfs_since: Option<Instant> = None;
    let mut last_logged: Option<GsxPosted> = None;
    loop {
        let credential = cfg.credential.borrow_and_update().clone();
        let (gsx, gsx_read) = *cfg.features.borrow_and_update();
        let tray = *cfg.tray.borrow_and_update();
        let msfs = *cfg.sim.borrow_and_update() == Some(SimKind::Msfs);
        let active = credential.clone().filter(|_| msfs);
        if let Some(credential) = &active {
            let since = *msfs_since.get_or_insert_with(Instant::now);
            if !(gsx && gsx_read && tray) {
                reader = None;
                publish(&out, None);
            }
            let (code, version) = match &reader {
                Some(r) => (GsxCode::Ok, r.version.clone()),
                None => {
                    let couatl = cfg.extras.borrow().couatl_started;
                    let (code, version, client) = probe(&cfg.probe, couatl).await;
                    if let Some((client, hello)) = client.filter(|_| gsx && gsx_read && tray) {
                        let task = tokio::spawn(read_loop(client, hello, cfg.extras.clone(), out.clone()));
                        reader = Some(Reader {
                            task,
                            version: version.clone(),
                        });
                    }
                    (code, version)
                }
            };
            let current = GsxPosted {
                credential: credential_fingerprint(credential),
                code,
                gsx_version: version,
                reading: reader.is_some(),
            };
            if last_logged.as_ref() != Some(&current) {
                let v = current.gsx_version.as_deref().unwrap_or("-");
                log::info!("gsx: {} {v}, reading {}", code_str(code), current.reading);
                last_logged = Some(current.clone());
            }
            let now = Instant::now();
            let settling = matches!(code, GsxCode::NotRunning | GsxCode::NoApi) && now.duration_since(since) < t.settle;
            let held = refused.as_ref() == Some(credential)
                || server_off
                || settling
                || invalid.as_ref() == Some(&current)
                || hold_until.is_some_and(|h| now < h);
            if !held && posted.as_ref() != Some(&current) {
                let result = api.post_gsx(credential, &body(&current)).await;
                if !matches!(
                    result,
                    Err(ApiError::Network(_) | ApiError::RateLimited { .. } | ApiError::Unavailable { .. })
                ) {
                    post_failing = false;
                    hold_until = None;
                }
                match result {
                    Ok(()) => {
                        if let Some(f) = cfg.posted_file.as_deref() {
                            if let Err(e) = save_gsx_posted(f, &current) {
                                log::warn!("gsx: cannot keep the last health: {e}");
                            }
                        }
                        posted = Some(current);
                    }
                    Err(ApiError::Unauthorized) => {
                        log::warn!("gsx: SkyNexus refused the credential; not sending until it changes");
                        refused = Some(credential.clone());
                    }
                    Err(ApiError::InvalidBody { path }) => {
                        log::warn!("gsx: SkyNexus refused the health at {path}; not resending it");
                        invalid = Some(current);
                    }
                    Err(ApiError::Disabled) => {
                        log::info!("gsx: switched off on SkyNexus; asking again on the next ticket");
                        server_off = true;
                        // Mark the current ticket as seen: only a newer one turns it back on.
                        cfg.features.borrow_and_update();
                    }
                    Err(e) => {
                        if !post_failing {
                            log::info!("gsx: cannot post the health ({e}); retrying");
                            post_failing = true;
                        }
                        hold_until = e.retry_after().map(|d| now + d.min(MAX_RETRY_AFTER));
                    }
                }
            }
        } else {
            reader = None;
            msfs_since = None;
            publish(&out, None);
        }
        let wake = tokio::select! {
            _ = sleep(t.detect_every), if active.is_some() => Wake::Other,
            _ = ended(&mut reader) => Wake::ReaderEnded,
            r = cfg.features.changed() => if r.is_err() { Wake::Closed } else { Wake::Ticket },
            r = cfg.credential.changed() => if r.is_err() { Wake::Closed } else { Wake::Other },
            r = cfg.tray.changed() => if r.is_err() { Wake::Closed } else { Wake::Other },
            r = cfg.sim.changed() => if r.is_err() { Wake::Closed } else { Wake::Other },
        };
        match wake {
            Wake::Closed => {
                drop(reader);
                publish(&out, None);
                return;
            }
            Wake::Ticket => server_off = false,
            Wake::ReaderEnded => {
                reader = None;
                publish(&out, None);
                sleep(t.reconnect_after).await;
            }
            Wake::Other => {}
        }
    }
}

/// The JSON of a frame that is about to be sent, with the GSX fields the
/// emitter says are due on it. Call it only for frames that go out, so a
/// change or the one `null` is never spent on a frame that is dropped.
pub fn frame_json(
    f: &Frame,
    tl: Option<&str>,
    em: &mut Emitter,
    feed: Option<&GsxFeed>,
    now: std::time::Instant,
) -> String {
    let gsx = em.on_frame(now, feed.map(|x| x.block.clone()));
    let gates = feed
        .and_then(|x| x.parkings.as_ref())
        .filter(|p| em.parkings_due(&p.apt, now))
        .cloned();
    if gsx.is_none() && gates.is_none() {
        return f.to_json_with_tl(tl);
    }
    let mut f = f.clone();
    f.gsx = gsx;
    f.gsx_parkings = gates;
    // The block and the gate list may push the frame over the relay's limit.
    f.fit_to_budget().to_json_with_tl(tl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HELLO: &str = r#"{"type":"hello","protocol":2,"engine":"4.0.8","authRequired":false}"#;

    fn snapshot(gate: &str) -> String {
        json!({"type":"snapshot","state":3,
            "handlerData":{
                "airport":{"icao":"EHAM",
                    "parkings":[{"uiGateName":"D 7"},{"uiGateName":"D 8"}]},
                "gate":{"uiName":"","uiGateName":gate,"uiType":"GATE_MEDIUM"}},
            "services":[{"id":"Boarding","stateRaw":5,"canTrigger":false,
                "progressText":"31/174 passengers"}]})
        .to_string()
    }

    /// A local GSX: each connection gets the next script (the last one
    /// repeats), then stays open (or closes when `close` is set).
    struct FakeGsx {
        port: u16,
        accepts: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
        task: JoinHandle<()>,
    }

    impl Drop for FakeGsx {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn fake_gsx(scripts: Vec<(Vec<String>, bool)>) -> FakeGsx {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (accepts, closed) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (a, c) = (accepts.clone(), closed.clone());
        let task = tokio::spawn(async move {
            loop {
                let (s, _) = l.accept().await.unwrap();
                let n = a.fetch_add(1, Ordering::SeqCst);
                let (msgs, close) = scripts[n.min(scripts.len() - 1)].clone();
                let c = c.clone();
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
                    for m in msgs {
                        ws.send(Message::text(m)).await.unwrap();
                    }
                    if !close {
                        while let Some(Ok(_)) = ws.next().await {}
                    }
                    c.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        FakeGsx {
            port,
            accepts,
            closed,
            task,
        }
    }

    async fn gsx_up() -> FakeGsx {
        fake_gsx(vec![(vec![HELLO.into(), snapshot("D 7")], false)]).await
    }

    fn refused_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    async fn app(resp: ResponseTemplate) -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/gsx"))
            .respond_with(resp)
            .mount(&s)
            .await;
        s
    }

    fn fast() -> GsxTiming {
        GsxTiming {
            detect_every: Duration::from_millis(30),
            settle: Duration::ZERO,
            reconnect_after: Duration::from_millis(10),
        }
    }

    struct Opts {
        installed: bool,
        port: u16,
        sim: SimKind,
        features: (bool, bool),
        file: Option<PathBuf>,
        timing: GsxTiming,
    }

    fn opts(port: u16) -> Opts {
        Opts {
            installed: true,
            port,
            sim: SimKind::Msfs,
            features: (true, true),
            file: None,
            timing: fast(),
        }
    }

    struct H {
        out: watch::Receiver<Option<GsxFeed>>,
        credential: watch::Sender<Option<String>>,
        features: watch::Sender<(bool, bool)>,
        tray: watch::Sender<bool>,
        sim: watch::Sender<Option<SimKind>>,
        _extras: watch::Sender<SimExtras>,
        task: JoinHandle<()>,
    }

    impl Drop for H {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn start(app: &MockServer, o: Opts) -> H {
        let (out_tx, out) = watch::channel(None);
        let (credential, credential_rx) = watch::channel(Some("snc_1".to_string()));
        let (features, features_rx) = watch::channel(o.features);
        let (tray, tray_rx) = watch::channel(true);
        let (sim, sim_rx) = watch::channel(Some(o.sim));
        let (extras, extras_rx) = watch::channel(SimExtras::default());
        let installed = o.installed;
        let port = o.port;
        let cfg = GsxRunnerCfg {
            credential: credential_rx,
            features: features_rx,
            tray: tray_rx,
            sim: sim_rx,
            extras: extras_rx,
            probe: GsxProbe {
                installed: Arc::new(move || installed),
                port: Arc::new(move || port),
            },
            posted_file: o.file,
            timing: o.timing,
        };
        let task = tokio::spawn(run(cfg, ApiClient::new(&app.uri()), out_tx));
        H {
            out,
            credential,
            features,
            tray,
            sim,
            _extras: extras,
            task,
        }
    }

    async fn posts(app: &MockServer) -> Vec<Value> {
        let reqs = app.received_requests().await.unwrap();
        reqs.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
    }

    async fn until(what: &str, mut f: impl AsyncFnMut() -> bool) {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !f().await {
            assert!(std::time::Instant::now() < end, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn n_posts(app: &MockServer, n: usize) {
        until("posts", async || posts(app).await.len() >= n).await;
    }

    fn quiet() -> tokio::time::Sleep {
        tokio::time::sleep(Duration::from_millis(200))
    }

    fn health(b: &Value) -> &Value {
        &b["health"]
    }

    fn temp_file(name: &str) -> PathBuf {
        let pid = std::process::id();
        let d = std::env::temp_dir().join(format!("snx-gsx-runner-{name}-{pid}"));
        let _ = std::fs::remove_dir_all(&d);
        d.join("gsx-health.json")
    }

    #[tokio::test]
    async fn x_plane_and_replays_never_probe_or_post() {
        for sim in [SimKind::Xplane, SimKind::Replay] {
            let g = gsx_up().await;
            let a = app(ResponseTemplate::new(204)).await;
            let h = start(&a, Opts { sim, ..opts(g.port) });
            quiet().await;
            assert_eq!(g.accepts.load(Ordering::SeqCst), 0, "{sim:?}");
            assert!(posts(&a).await.is_empty(), "{sim:?}");
            assert_eq!(*h.out.borrow(), None);
        }
    }

    #[tokio::test]
    async fn not_installed_posts_once_and_a_restart_posts_nothing() {
        let file = temp_file("restart");
        let a = app(ResponseTemplate::new(204)).await;
        let o = || Opts {
            installed: false,
            file: Some(file.clone()),
            ..opts(refused_port())
        };
        let h = start(&a, o());
        n_posts(&a, 1).await;
        quiet().await;
        let sent = posts(&a).await;
        assert_eq!(sent.len(), 1, "several detections, one POST");
        let hl = health(&sent[0]);
        assert_eq!(hl["code"], "GSX_NOT_INSTALLED");
        assert_eq!(hl["reading"], false);
        assert_eq!(hl["schema"], 1);
        assert_eq!(hl["connectorVersion"], env!("CARGO_PKG_VERSION"));
        assert!(hl.get("gsxVersion").is_none(), "absent, never null");
        assert_eq!(sent[0].as_object().unwrap().len(), 1, "only health");
        drop(h);
        // A restart with the saved value: nothing new to say.
        let _h = start(&a, o());
        quiet().await;
        assert_eq!(posts(&a).await.len(), 1);
    }

    #[tokio::test]
    async fn a_new_sign_in_posts_once_even_with_a_saved_value() {
        let file = temp_file("signin");
        let a = app(ResponseTemplate::new(204)).await;
        let o = Opts {
            installed: false,
            file: Some(file),
            ..opts(refused_port())
        };
        let h = start(&a, o);
        n_posts(&a, 1).await;
        h.credential.send_replace(None);
        quiet().await;
        assert_eq!(posts(&a).await.len(), 1, "nothing while signed out");
        h.credential.send_replace(Some("snc_2".into()));
        n_posts(&a, 2).await;
        let reqs = a.received_requests().await.unwrap();
        assert_eq!(reqs[1].headers["authorization"], "Bearer snc_2");
    }

    #[tokio::test]
    async fn gsx_up_posts_ok_with_version_then_streams_blocks() {
        let g = gsx_up().await;
        let a = app(ResponseTemplate::new(204)).await;
        let mut h = start(&a, opts(g.port));
        n_posts(&a, 1).await;
        let hl = health(&posts(&a).await[0]).clone();
        assert_eq!(hl["code"], "GSX_OK");
        assert_eq!(hl["gsxVersion"], "4.0.8");
        assert_eq!(hl["reading"], true);
        let feed = h.out.wait_for(Option::is_some).await.unwrap().clone().unwrap();
        assert_eq!(feed.block.ver.as_deref(), Some("4.0.8"));
        assert_eq!(feed.block.apt.as_deref(), Some("EHAM"));
        assert_eq!(feed.block.gate.as_deref(), Some("D 7"));
        assert_eq!(feed.block.svc.len(), 1);
        assert_eq!(feed.parkings.unwrap().names, vec!["D 7", "D 8"]);
        quiet().await;
        assert_eq!(posts(&a).await.len(), 1, "no change, no post");
        assert_eq!(g.accepts.load(Ordering::SeqCst), 1, "no second socket");
    }

    #[tokio::test]
    async fn gsx_read_off_posts_reading_false_once_and_publishes_none() {
        let g = gsx_up().await;
        let a = app(ResponseTemplate::new(204)).await;
        let h = start(
            &a,
            Opts {
                features: (true, false),
                ..opts(g.port)
            },
        );
        n_posts(&a, 1).await;
        quiet().await;
        let sent = posts(&a).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(health(&sent[0])["code"], "GSX_OK");
        assert_eq!(health(&sent[0])["reading"], false);
        assert_eq!(*h.out.borrow(), None);
    }

    #[tokio::test]
    async fn the_tray_switch_and_the_kill_switch_stop_reading() {
        let g = gsx_up().await;
        let a = app(ResponseTemplate::new(204)).await;
        let mut h = start(&a, opts(g.port));
        h.out.wait_for(Option::is_some).await.unwrap();
        h.tray.send_replace(false);
        h.out.wait_for(Option::is_none).await.unwrap();
        n_posts(&a, 2).await;
        assert_eq!(health(&posts(&a).await[1])["reading"], false);
        until("the socket closed", async || g.closed.load(Ordering::SeqCst) >= 1).await;
        h.tray.send_replace(true);
        h.out.wait_for(Option::is_some).await.unwrap();
        h.features.send_replace((false, true));
        h.out.wait_for(Option::is_none).await.unwrap();
    }

    #[tokio::test]
    async fn leaving_msfs_aborts_the_read_loop() {
        let g = gsx_up().await;
        let a = app(ResponseTemplate::new(204)).await;
        let mut h = start(&a, opts(g.port));
        h.out.wait_for(Option::is_some).await.unwrap();
        h.sim.send_replace(None);
        h.out.wait_for(Option::is_none).await.unwrap();
        until("the socket closed", async || g.closed.load(Ordering::SeqCst) == 1).await;
        quiet().await;
        assert_eq!(g.accepts.load(Ordering::SeqCst), 1, "no probes without MSFS");
    }

    #[tokio::test]
    async fn a_dropped_socket_clears_the_block_and_reconnects_fresh() {
        let first = (vec![HELLO.into(), snapshot("D 7")], true);
        // The second session sends only a patch: no snapshot yet.
        let patch = json!({"type":"patch","path":"/handlerData/gate/uiGateName","value":"D 8"});
        let second = (vec![HELLO.into(), patch.to_string()], false);
        let g = fake_gsx(vec![first, second]).await;
        let a = app(ResponseTemplate::new(204)).await;
        let mut h = start(&a, opts(g.port));
        until("a reconnect", async || g.accepts.load(Ordering::SeqCst) >= 2).await;
        let feed = h.out.wait_for(Option::is_some).await.unwrap().clone().unwrap();
        let b = feed.block;
        assert!(b.svc.is_empty(), "no services from the old session");
        assert_eq!((b.apt, b.gate), (None, None), "nothing from the old snapshot");
        assert_eq!(b.ver.as_deref(), Some("4.0.8"), "seeded from the new hello");
        assert_eq!(feed.parkings, None);
    }

    #[tokio::test]
    async fn not_up_yet_after_msfs_connects_waits_for_the_settle_time() {
        let a = app(ResponseTemplate::new(204)).await;
        let timing = GsxTiming {
            settle: Duration::from_secs(30),
            ..fast()
        };
        let _h = start(
            &a,
            Opts {
                timing,
                ..opts(refused_port())
            },
        );
        quiet().await;
        assert!(posts(&a).await.is_empty(), "GSX_NOT_RUNNING is held back");
    }

    #[tokio::test]
    async fn disabled_stops_posting_until_the_next_ticket() {
        let a = MockServer::start().await;
        let off = json!({ "error": "disabled" });
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_json(off))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&a)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .with_priority(5)
            .mount(&a)
            .await;
        let h = start(
            &a,
            Opts {
                installed: false,
                ..opts(refused_port())
            },
        );
        n_posts(&a, 1).await;
        quiet().await;
        assert_eq!(posts(&a).await.len(), 1, "no retries while switched off");
        h.features.send_replace((true, true));
        n_posts(&a, 2).await;
        quiet().await;
        assert_eq!(posts(&a).await.len(), 2, "accepted, then quiet");
    }

    #[tokio::test]
    async fn a_refused_credential_stops_until_it_changes() {
        let a = app(ResponseTemplate::new(401).set_body_json(json!({ "error": "revoked" }))).await;
        let h = start(
            &a,
            Opts {
                installed: false,
                ..opts(refused_port())
            },
        );
        n_posts(&a, 1).await;
        quiet().await;
        assert_eq!(posts(&a).await.len(), 1);
        h.credential.send_replace(Some("snc_2".into()));
        n_posts(&a, 2).await;
        let reqs = a.received_requests().await.unwrap();
        assert_eq!(reqs[1].headers["authorization"], "Bearer snc_2");
    }

    #[tokio::test]
    async fn an_invalid_body_is_not_resent() {
        let bad = json!({ "error": "invalid_body", "path": "health.gsxVersion" });
        let a = app(ResponseTemplate::new(400).set_body_json(bad)).await;
        let _h = start(
            &a,
            Opts {
                installed: false,
                ..opts(refused_port())
            },
        );
        n_posts(&a, 1).await;
        quiet().await;
        assert_eq!(posts(&a).await.len(), 1);
    }

    #[test]
    fn versions_the_server_would_refuse_are_left_out() {
        assert_eq!(version_of(" 4.0.8 ").as_deref(), Some("4.0.8"));
        assert_eq!(version_of(""), None);
        assert_eq!(version_of(&"9".repeat(25)), None);
        assert_eq!(version_of("4.0\n8"), None);
    }

    fn frame() -> Frame {
        let own = crate::frame::Own {
            lat: 52.3,
            lon: 4.7,
            alt_ft: 0.0,
            gs_kt: 0.0,
            hdg: 0.0,
            trk: 0.0,
            vs_fpm: 0.0,
            on_ground: true,
            fuel_kg: None,
            callsign: None,
            tl: None,
        };
        Frame::new(SimKind::Msfs, 1, own)
    }

    fn feed() -> GsxFeed {
        let mut st = GsxState::default();
        st.apply(&serde_json::from_str(HELLO).unwrap());
        st.apply(&serde_json::from_str(&snapshot("D 7")).unwrap());
        GsxFeed {
            block: block(&st, &SimExtras::default(), None),
            parkings: parkings(&st),
        }
    }

    #[test]
    fn frames_carry_the_block_and_gates_when_due() {
        let (mut em, t0) = (Emitter::default(), std::time::Instant::now());
        let f = frame();
        let at = |ms| t0 + Duration::from_millis(ms);
        let plain = frame_json(&f, Some("0123456789ab"), &mut em, None, at(0));
        assert!(!plain.contains("gsx"));
        let fd = feed();
        let v: Value = serde_json::from_str(&frame_json(&f, None, &mut em, Some(&fd), at(250))).unwrap();
        assert_eq!(v["gsx"]["gate"], "D 7");
        assert_eq!(v["gsxParkings"]["apt"], "EHAM");
        let next = frame_json(&f, Some("0123456789ab"), &mut em, Some(&fd), at(500));
        assert!(!next.contains("gsx"), "unchanged: nothing until the heartbeat");
        assert!(next.contains(r#""tl":"0123456789ab""#));
        let gone = frame_json(&f, None, &mut em, None, at(750));
        assert!(gone.contains(r#""gsx":null"#));
        assert!(!frame_json(&f, None, &mut em, None, at(1000)).contains("gsx"));
    }

    #[test]
    fn a_feed_without_gates_sends_no_gate_list() {
        let (mut em, t0) = (Emitter::default(), std::time::Instant::now());
        let fd = GsxFeed {
            parkings: None,
            ..feed()
        };
        let json = frame_json(&frame(), None, &mut em, Some(&fd), t0);
        assert!(json.contains(r#""gsx":{"#) && !json.contains("gsxParkings"));
        // The gate list arrives later: it goes out on the next frame.
        let json = frame_json(&frame(), None, &mut em, Some(&feed()), t0);
        assert!(json.contains("gsxParkings"));
    }
}
