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
//! - Nothing is read or posted before the first ticket (or after sign-out
//!   until the next one).
//! - Reads while `features.gsx && features.gsxRead && tray`, signed in and
//!   in MSFS. The read loop runs in its own task with a fresh [`GsxState`]
//!   per connection, stopped (and awaited) when any of that stops; the
//!   current block and gate list go on `out` for the frame builder
//!   ([`frame_json`]). A socket that drops is redialled after 2 s, doubling
//!   to 30 s while sessions keep dropping, back to 2 s after one of 60 s.
//! - A 503 `disabled` stops posting until the next ticket; a 401 until the
//!   credential changes; a 400 is not resent for the same health.
//! - The relay's `cmd` messages ([`super::commands`]): `gsx.read` sets the
//!   read switch until the next ticket; `gsx.gate` and `gsx.simbrief` run
//!   in the read loop, one at a time, and their result rides on the blocks
//!   as `last`. A gate GSX does not list is `unknown_gate`; nothing runs
//!   while GSX is not read.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant};

use super::block::{block, parkings, CmdResult, Emitter, GsxBlock, GsxParkings};
use super::client::GsxClient;
use super::commands::{check_gate, execute, parse_cmd, refused, Cmd, BUSY, NOT_READING};
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
/// Commands queued for the read loop; it drains them even while busy.
const READER_QUEUE: usize = 4;
/// A gate or SimBrief command that waited longer than this since the relay
/// delivered it is dropped, not run late.
const MAX_CMD_AGE: Duration = Duration::from_secs(10);

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
    /// After the GSX socket dropped, before the next detection; doubles
    /// on every drop up to `reconnect_max`.
    pub reconnect_after: Duration,
    pub reconnect_max: Duration,
    /// A session that lasted this long resets the reconnect wait.
    pub stable_after: Duration,
}

impl Default for GsxTiming {
    fn default() -> Self {
        GsxTiming {
            detect_every: Duration::from_secs(30),
            settle: Duration::from_secs(90),
            reconnect_after: Duration::from_secs(2),
            reconnect_max: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
        }
    }
}

pub struct GsxRunnerCfg {
    /// The connector credential; None while signed out.
    pub credential: watch::Receiver<Option<String>>,
    /// `(features.gsx, features.gsxRead)` of every ticket; None until the
    /// first ticket (and again after sign-out).
    pub features: watch::Receiver<Option<(bool, bool)>>,
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
    /// The relay's `cmd` messages, unparsed, with the time each arrived.
    pub cmds: mpsc::Receiver<(Instant, Value)>,
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

/// What the runner hands the read loop.
enum ToReader {
    /// Run this on GSX; the relay delivered it at the `Instant`.
    Run(String, Cmd, Instant),
    /// Put this result on the blocks (a `gsx.read` the runner handled).
    Show(CmdResult),
}

/// The read loop's task; dropping it aborts the loop (and closes the socket).
struct Reader {
    task: JoinHandle<()>,
    version: Option<String>,
    started: Instant,
    cmds: mpsc::Sender<ToReader>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Stops the read loop and waits until it is gone, so a block it was
/// publishing cannot land after the `None` that follows.
async fn stop(reader: &mut Option<Reader>) {
    if let Some(mut r) = reader.take() {
        // A finished task may have been awaited already (`ended`).
        if !r.task.is_finished() {
            r.task.abort();
            let _ = (&mut r.task).await;
        }
    }
}

/// Stops reading and clears the feed.
async fn stop_reading(reader: &mut Option<Reader>, out: &watch::Sender<Option<GsxFeed>>) {
    stop(reader).await;
    publish(out, None);
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

fn feed(state: &GsxState, extras: &SimExtras, last: &Option<CmdResult>) -> GsxFeed {
    GsxFeed {
        block: block(state, extras, last.clone()),
        parkings: parkings(state),
    }
}

/// Reads one GSX session into a fresh state and publishes the feed after
/// every message (and every L:var change, for the pax fallback). Runs the
/// runner's commands one at a time; one that comes while another waits for
/// GSX is `busy`. The latest result rides on every block as `last`. Clears
/// the feed when the socket closes.
async fn read_loop(
    mut client: GsxClient,
    hello: Value,
    mut extras: watch::Receiver<SimExtras>,
    out: watch::Sender<Option<GsxFeed>>,
    mut cmds: mpsc::Receiver<ToReader>,
) {
    // `connect` consumed the hello: seed the new session's state with it.
    let mut state = GsxState::default();
    state.apply(&hello);
    let mut has_state = false;
    let mut last: Option<CmdResult> = None;
    'session: loop {
        tokio::select! {
            msg = client.next() => {
                let Some(msg) = msg else { break };
                let kind = msg.get("type").and_then(Value::as_str);
                has_state |= matches!(kind, Some("snapshot" | "patch"));
                state.apply(&msg);
            }
            r = extras.changed() => if r.is_err() { break },
            c = cmds.recv() => match c {
                // The runner let go of this session.
                None => break,
                Some(ToReader::Show(r)) => last = Some(r),
                Some(ToReader::Run(id, cmd, at)) => {
                    // Only a gate on GSX's own list for its airport is selected.
                    let gates = parkings(&state).map(|p| p.names);
                    if at.elapsed() > MAX_CMD_AGE {
                        log::info!("gsx: {} dropped; it waited {} s", cmd.verb(), at.elapsed().as_secs());
                    } else if let Some(r) = check_gate(&id, &cmd, gates.as_deref()) {
                        last = Some(r);
                    } else {
                        let run = execute(&mut client, id, cmd);
                        tokio::pin!(run);
                        let done = loop {
                            tokio::select! {
                                r = &mut run => break r,
                                c = cmds.recv() => {
                                    match c {
                                        None => break 'session,
                                        Some(ToReader::Run(id, ..)) => last = Some(refused(id, BUSY)),
                                        Some(ToReader::Show(r)) => last = Some(r),
                                    }
                                    if has_state {
                                        publish(&out, Some(feed(&state, &extras.borrow(), &last)));
                                    }
                                }
                            }
                        };
                        last = Some(done);
                    }
                }
            },
        }
        if has_state {
            publish(&out, Some(feed(&state, &extras.borrow(), &last)));
        }
    }
    publish(&out, None);
}

/// Hands the `gsx.read` result to the read loop; kept until there is one.
fn deliver(pending: &mut Option<CmdResult>, reader: &Option<Reader>) {
    if let Some(rd) = reader {
        if let Some(r) = pending.take() {
            let _ = rd.cmds.try_send(ToReader::Show(r));
        }
    }
}

/// One relay `cmd` that arrived at `at`. A `gsx.read` sets the read
/// switch until the next ticket and returns true when that changed it (the
/// runner looks again at once); its ok goes on the blocks as soon as GSX is
/// read. The others go to the read loop; without one they are not run
/// (`not_reading`, which no block carries, so it is only logged).
fn on_cmd(
    (at, v): (Instant, Value),
    reader: &Option<Reader>,
    read_override: &mut Option<bool>,
    pending: &mut Option<CmdResult>,
) -> bool {
    let Some((id, cmd)) = parse_cmd(&v) else {
        log::debug!("gsx: ignoring a command the Connector does not run");
        return false;
    };
    let verb = cmd.verb();
    if let Cmd::Read(on) = cmd {
        *pending = Some(CmdResult {
            id,
            ok: true,
            code: None,
        });
        if *read_override != Some(on) {
            log::info!("gsx: reading switched {} from SkyNexus", if on { "on" } else { "off" });
            *read_override = Some(on);
            return true;
        }
        deliver(pending, reader);
        return false;
    }
    match reader.as_ref().map(|r| r.cmds.try_send(ToReader::Run(id, cmd, at))) {
        Some(Ok(())) => {}
        Some(Err(_)) => log::info!("gsx: {verb} dropped; the read loop is not taking commands"),
        None => log::info!("gsx: {verb} not run ({NOT_READING})"),
    }
    false
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

/// The wait before redialling a GSX whose socket dropped.
struct Redial {
    base: Duration,
    max: Duration,
    next: Duration,
}

impl Redial {
    fn new(base: Duration, max: Duration) -> Self {
        Redial { base, max, next: base }
    }

    /// The wait after a session that ran for `ran`: doubles every time, up
    /// to the cap, and starts over after a session of at least `stable`.
    fn after(&mut self, ran: Duration, stable: Duration) -> Duration {
        if ran >= stable {
            self.next = self.base;
        }
        let d = self.next;
        self.next = (d * 2).min(self.max);
        d
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
    let mut redial = Redial::new(t.reconnect_after, t.reconnect_max);
    // After a dropped socket: no probe (and no post) before this.
    let mut redial_at: Option<Instant> = None;
    // `gsx.read` from SkyNexus; the next ticket clears it.
    let mut read_override: Option<bool> = None;
    // The `gsx.read` result, for the next block when GSX is read after it.
    let mut pending: Option<CmdResult> = None;
    let mut cmds_open = true;
    loop {
        let credential = cfg.credential.borrow_and_update().clone();
        let features = *cfg.features.borrow_and_update();
        let tray = *cfg.tray.borrow_and_update();
        let msfs = *cfg.sim.borrow_and_update() == Some(SimKind::Msfs);
        let signed_in_msfs = credential.clone().filter(|_| msfs);
        // The settle window starts when MSFS connects, not at the ticket.
        let since = signed_in_msfs
            .as_ref()
            .map(|_| *msfs_since.get_or_insert_with(Instant::now));
        if since.is_none() {
            msfs_since = None;
        }
        // Nothing is read or posted before the first ticket says what is on.
        let active = signed_in_msfs.zip(features);
        if let (Some((credential, (gsx, gsx_read))), Some(since)) = (&active, since) {
            let (gsx, gsx_read) = (*gsx, read_override.unwrap_or(*gsx_read));
            if !(gsx && gsx_read && tray) {
                stop_reading(&mut reader, &out).await;
            }
            // Inside the redial wait nothing is probed or posted; the wait
            // does not block, so tickets and switches are seen meanwhile.
            let redialing = reader.is_none() && redial_at.is_some_and(|d| Instant::now() < d);
            if !redialing {
                let (code, version) = match &reader {
                    Some(r) => (GsxCode::Ok, r.version.clone()),
                    None => {
                        let couatl = cfg.extras.borrow().couatl_started;
                        let (code, version, client) = probe(&cfg.probe, couatl).await;
                        if let Some((client, hello)) = client.filter(|_| gsx && gsx_read && tray) {
                            let (tx, rx) = mpsc::channel(READER_QUEUE);
                            let extras = cfg.extras.clone();
                            let task = tokio::spawn(read_loop(client, hello, extras, out.clone(), rx));
                            reader = Some(Reader {
                                task,
                                version: version.clone(),
                                started: Instant::now(),
                                cmds: tx,
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
                let settling =
                    matches!(code, GsxCode::NotRunning | GsxCode::NoApi) && now.duration_since(since) < t.settle;
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
            }
        } else {
            stop_reading(&mut reader, &out).await;
        }
        deliver(&mut pending, &reader);
        let next_detect = Instant::now() + t.detect_every;
        let wake = loop {
            tokio::select! {
                _ = sleep_until(next_detect), if active.is_some() => break Wake::Other,
                _ = sleep_until(redial_at.unwrap_or(next_detect)), if redial_at.is_some_and(|d| Instant::now() < d) => break Wake::Other,
                _ = ended(&mut reader) => break Wake::ReaderEnded,
                r = cfg.features.changed() => break if r.is_err() { Wake::Closed } else { Wake::Ticket },
                r = cfg.credential.changed() => break if r.is_err() { Wake::Closed } else { Wake::Other },
                r = cfg.tray.changed() => break if r.is_err() { Wake::Closed } else { Wake::Other },
                r = cfg.sim.changed() => break if r.is_err() { Wake::Closed } else { Wake::Other },
                c = cfg.cmds.recv(), if cmds_open => match c {
                    None => cmds_open = false,
                    Some(c) => if on_cmd(c, &reader, &mut read_override, &mut pending) { break Wake::Other },
                },
            }
        };
        match wake {
            Wake::Closed => {
                stop_reading(&mut reader, &out).await;
                return;
            }
            Wake::Ticket => {
                server_off = false;
                // The ticket says again whether GSX is read.
                read_override = None;
            }
            Wake::ReaderEnded => {
                let ran = reader.as_ref().map_or(Duration::ZERO, |r| r.started.elapsed());
                stop_reading(&mut reader, &out).await;
                // A GSX that accepts and drops at once is not dialled every 2 s.
                redial_at = Some(Instant::now() + redial.after(ran, t.stable_after));
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
    use super::super::block::GsxLast;
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
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
        /// Commands it got, other than `state.get`.
        got: Arc<Mutex<Vec<Value>>>,
        task: JoinHandle<()>,
    }

    /// How a fake GSX answers a command: after a wait, with `ok`.
    type Answer = Option<(Duration, bool)>;

    impl Drop for FakeGsx {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn fake_gsx(scripts: Vec<(Vec<String>, bool)>) -> FakeGsx {
        fake_gsx_answering(scripts, None).await
    }

    async fn fake_gsx_answering(scripts: Vec<(Vec<String>, bool)>, answer: Answer) -> FakeGsx {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (accepts, closed) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let got = Arc::new(Mutex::new(Vec::new()));
        let (a, c, g) = (accepts.clone(), closed.clone(), got.clone());
        let task = tokio::spawn(async move {
            loop {
                let (s, _) = l.accept().await.unwrap();
                let n = a.fetch_add(1, Ordering::SeqCst);
                let (msgs, close) = scripts[n.min(scripts.len() - 1)].clone();
                let (c, g) = (c.clone(), g.clone());
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
                    for m in msgs {
                        if ws.send(Message::text(m)).await.is_err() {
                            break;
                        }
                    }
                    if close {
                        c.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                    while let Some(Ok(m)) = ws.next().await {
                        let Message::Text(t) = m else { continue };
                        let v: Value = serde_json::from_str(&t).unwrap();
                        if v["type"] != "command" || v["verb"] == "state.get" {
                            continue;
                        }
                        g.lock().unwrap().push(v.clone());
                        if let Some((wait, ok)) = answer {
                            tokio::time::sleep(wait).await;
                            let r = json!({"type":"result","id":v["id"],"ok":ok});
                            let _ = ws.send(Message::text(r.to_string())).await;
                        }
                    }
                    c.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        FakeGsx {
            port,
            accepts,
            closed,
            got,
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
            reconnect_max: Duration::from_millis(40),
            stable_after: Duration::from_secs(60),
        }
    }

    struct Opts {
        installed: bool,
        port: u16,
        sim: SimKind,
        features: Option<(bool, bool)>,
        file: Option<PathBuf>,
        timing: GsxTiming,
    }

    fn opts(port: u16) -> Opts {
        Opts {
            installed: true,
            port,
            sim: SimKind::Msfs,
            features: Some((true, true)),
            file: None,
            timing: fast(),
        }
    }

    struct H {
        out: watch::Receiver<Option<GsxFeed>>,
        credential: watch::Sender<Option<String>>,
        features: watch::Sender<Option<(bool, bool)>>,
        tray: watch::Sender<bool>,
        sim: watch::Sender<Option<SimKind>>,
        _extras: watch::Sender<SimExtras>,
        cmds: mpsc::Sender<(Instant, Value)>,
        task: JoinHandle<()>,
    }

    impl H {
        async fn cmd(&self, id: &str, verb: &str, args: Value) {
            self.cmd_at(Instant::now(), id, verb, args).await;
        }

        /// A command the relay received at `at`.
        async fn cmd_at(&self, at: Instant, id: &str, verb: &str, args: Value) {
            let v = json!({"type":"cmd","id":id,"verb":verb,"args":args});
            self.cmds.send((at, v)).await.unwrap();
        }

        /// Waits for a block whose `last` is `(id, ok, code)`.
        async fn last_is(&mut self, id: &str, ok: bool, code: Option<&str>) {
            let want = GsxLast {
                id: id.into(),
                ok,
                code: code.map(str::to_string),
            };
            let is = |f: &Option<GsxFeed>| f.as_ref().and_then(|f| f.block.last.as_ref()) == Some(&want);
            let got = tokio::time::timeout(Duration::from_secs(5), self.out.wait_for(is));
            got.await.expect("no such last").unwrap();
        }
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
        let (cmds, cmds_rx) = mpsc::channel(16);
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
            cmds: cmds_rx,
        };
        let task = tokio::spawn(run(cfg, ApiClient::new(&app.uri()), out_tx));
        H {
            out,
            credential,
            features,
            tray,
            sim,
            _extras: extras,
            cmds,
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
                features: Some((true, false)),
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
        h.features.send_replace(Some((false, true)));
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
    async fn the_settle_window_elapses_then_not_running_posts() {
        let a = app(ResponseTemplate::new(204)).await;
        let timing = GsxTiming {
            settle: Duration::from_millis(300),
            ..fast()
        };
        let o = Opts {
            timing,
            ..opts(refused_port())
        };
        let t0 = std::time::Instant::now();
        let _h = start(&a, o);
        n_posts(&a, 1).await;
        assert!(t0.elapsed() >= Duration::from_millis(300), "not inside the window");
        assert_eq!(health(&posts(&a).await[0])["code"], "GSX_NOT_RUNNING");
    }

    #[tokio::test]
    async fn gsx_ok_posts_at_once_inside_the_settle_window() {
        let g = gsx_up().await;
        let a = app(ResponseTemplate::new(204)).await;
        let timing = GsxTiming {
            settle: Duration::from_secs(30),
            ..fast()
        };
        let o = Opts { timing, ..opts(g.port) };
        let _h = start(&a, o);
        n_posts(&a, 1).await;
        assert_eq!(health(&posts(&a).await[0])["code"], "GSX_OK");
    }

    #[tokio::test]
    async fn nothing_is_read_or_posted_before_the_first_ticket() {
        let g = gsx_up().await;
        let a = app(ResponseTemplate::new(204)).await;
        let o = Opts {
            features: None,
            ..opts(g.port)
        };
        let mut h = start(&a, o);
        quiet().await;
        assert!(posts(&a).await.is_empty(), "no post before a ticket");
        assert_eq!(g.accepts.load(Ordering::SeqCst), 0, "no read");
        assert_eq!(*h.out.borrow(), None);
        h.features.send_replace(Some((true, true)));
        h.out.wait_for(Option::is_some).await.unwrap();
        n_posts(&a, 1).await;
        quiet().await;
        assert_eq!(posts(&a).await.len(), 1, "then once");
        // Signed out (or a new account): back to waiting.
        h.features.send_replace(None);
        h.out.wait_for(Option::is_none).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_late_block_never_lands_after_the_none() {
        // A GSX that patches without pause: the read loop publishes all the time.
        let patches = (0..4000).map(|n| {
            let gate = format!("D {}", n % 50);
            let p = json!({"type":"patch","path":"/handlerData/gate/uiGateName","value":gate});
            p.to_string()
        });
        let msgs = [HELLO.to_string(), snapshot("D 7")].into_iter().chain(patches);
        let g = fake_gsx(vec![(msgs.collect(), false)]).await;
        let a = app(ResponseTemplate::new(204)).await;
        let mut h = start(&a, opts(g.port));
        for _ in 0..10 {
            h.out.wait_for(Option::is_some).await.unwrap();
            h.tray.send_replace(false);
            h.out.wait_for(Option::is_none).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(*h.out.borrow(), None, "a block landed after the None");
            h.tray.send_replace(true);
        }
    }

    #[tokio::test]
    async fn a_gsx_that_drops_at_once_is_redialled_ever_slower() {
        let g = fake_gsx(vec![(vec![HELLO.into(), snapshot("D 7")], true)]).await;
        let a = app(ResponseTemplate::new(204)).await;
        let timing = GsxTiming {
            reconnect_after: Duration::from_millis(40),
            reconnect_max: Duration::from_millis(320),
            ..fast()
        };
        let _h = start(&a, Opts { timing, ..opts(g.port) });
        // Waits 40, 80, 160, 320: five dials in 600 ms, not fifteen.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let n = g.accepts.load(Ordering::SeqCst);
        assert!((3..=6).contains(&n), "{n} dials");
    }

    #[test]
    fn the_redial_wait_doubles_to_its_cap_and_resets_after_a_long_session() {
        let ms = Duration::from_millis;
        let mut r = Redial::new(ms(2), ms(30));
        let short = ms(1);
        let stable = ms(60);
        let waits: Vec<_> = (0..6).map(|_| r.after(short, stable)).collect();
        assert_eq!(waits, [ms(2), ms(4), ms(8), ms(16), ms(30), ms(30)]);
        assert_eq!(r.after(stable, stable), ms(2), "a long session resets it");
        assert_eq!(r.after(short, stable), ms(4));
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
        h.features.send_replace(Some((true, true)));
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

    async fn gsx_answering(wait_ms: u64) -> FakeGsx {
        let script = vec![(vec![HELLO.into(), snapshot("D 7")], false)];
        fake_gsx_answering(script, Some((Duration::from_millis(wait_ms), true))).await
    }

    fn sent(g: &FakeGsx) -> Vec<Value> {
        g.got.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn a_gate_runs_on_gsx_and_its_result_lands_on_the_block() {
        let g = gsx_answering(0).await;
        let a = app(ResponseTemplate::new(204)).await;
        let mut h = start(&a, opts(g.port));
        h.out.wait_for(Option::is_some).await.unwrap();
        h.cmd("c1", "gsx.gate", json!({"gate":"D 8"})).await;
        h.last_is("c1", true, None).await;
        let got = sent(&g);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["verb"], "gate.select");
        assert_eq!(got[0]["args"]["gate"], "D 8");
        h.cmd("c2", "gsx.simbrief", json!({})).await;
        h.last_is("c2", true, None).await;
        assert_eq!(sent(&g)[1]["args"]["command"], "RELOAD_SIMBRIEF");
    }

    #[tokio::test]
    async fn a_gate_gsx_does_not_list_is_unknown_and_not_sent() {
        let g = gsx_answering(0).await;
        let a = app(ResponseTemplate::new(204)).await;
        let mut h = start(&a, opts(g.port));
        h.out.wait_for(Option::is_some).await.unwrap();
        h.cmd("c1", "gsx.gate", json!({"gate":"Gate 14"})).await;
        h.last_is("c1", false, Some("unknown_gate")).await;
        h.cmd("c2", "gsx.gate", json!({"gate":"d 8"})).await;
        h.last_is("c2", false, Some("unknown_gate")).await;
        assert!(sent(&g).is_empty(), "GSX was never asked");
    }

    #[tokio::test]
    async fn a_command_older_than_10_s_is_dropped() {
        let g = gsx_answering(0).await;
        let a = app(ResponseTemplate::new(204)).await;
        let mut h = start(&a, opts(g.port));
        h.out.wait_for(Option::is_some).await.unwrap();
        let old = Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
        h.cmd_at(old, "c1", "gsx.gate", json!({"gate":"D 8"})).await;
        h.cmd("c2", "gsx.simbrief", json!({})).await;
        h.last_is("c2", true, None).await;
        let got = sent(&g);
        assert_eq!(got.len(), 1, "c1 never ran");
        assert_eq!(got[0]["verb"], "command.run");
    }

    #[tokio::test]
    async fn a_ticket_during_the_redial_wait_clears_the_read_override() {
        // Probe 1 stays open; the reading session (2) drops at once.
        let up = (vec![HELLO.into(), snapshot("D 7")], false);
        let drops = (vec![HELLO.into(), snapshot("D 7")], true);
        let g = fake_gsx(vec![up.clone(), drops, up]).await;
        let a = app(ResponseTemplate::new(204)).await;
        let timing = GsxTiming {
            reconnect_after: Duration::from_millis(400),
            ..fast()
        };
        let o = Opts {
            features: Some((true, false)),
            timing,
            ..opts(g.port)
        };
        let h = start(&a, o);
        n_posts(&a, 1).await;
        h.cmd("r1", "gsx.read", json!({"on":true})).await;
        until("the session dropped", async || g.closed.load(Ordering::SeqCst) >= 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Inside the 400 ms wait: the ticket says GSX is not read.
        h.features.send_replace(Some((true, false)));
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(*h.out.borrow(), None, "the override outlived the ticket");
        let last = posts(&a).await.last().cloned().unwrap();
        assert_eq!(health(&last)["reading"], false);
    }

    #[tokio::test]
    async fn the_same_gsx_read_twice_probes_once() {
        let g = gsx_answering(0).await;
        let a = app(ResponseTemplate::new(204)).await;
        let timing = GsxTiming {
            detect_every: Duration::from_secs(30),
            ..fast()
        };
        let o = Opts {
            features: Some((true, false)),
            timing,
            ..opts(g.port)
        };
        let mut h = start(&a, o);
        n_posts(&a, 1).await;
        h.tray.send_replace(false);
        until("a probe for the tray", async || g.accepts.load(Ordering::SeqCst) >= 2).await;
        quiet().await;
        let before = g.accepts.load(Ordering::SeqCst);
        h.cmd("r1", "gsx.read", json!({"on":true})).await;
        until("a probe for r1", async || g.accepts.load(Ordering::SeqCst) > before).await;
        h.cmd("r2", "gsx.read", json!({"on":true})).await;
        quiet().await;
        assert_eq!(g.accepts.load(Ordering::SeqCst), before + 1, "r2 changed nothing");
        // Tray back on: reading, and r2's ok is the last result.
        h.tray.send_replace(true);
        h.last_is("r2", true, None).await;
    }

    #[tokio::test]
    async fn a_second_command_while_one_runs_is_busy() {
        let g = gsx_answering(400).await;
        let a = app(ResponseTemplate::new(204)).await;
        let mut h = start(&a, opts(g.port));
        h.out.wait_for(Option::is_some).await.unwrap();
        h.cmd("c1", "gsx.gate", json!({"gate":"D 8"})).await;
        until("c1 at GSX", async || sent(&g).len() == 1).await;
        h.cmd("c2", "gsx.simbrief", json!({})).await;
        h.last_is("c2", false, Some("busy")).await;
        h.last_is("c1", true, None).await;
        assert_eq!(sent(&g).len(), 1, "c2 never reached GSX");
    }

    #[tokio::test]
    async fn commands_while_not_reading_are_not_run() {
        let g = gsx_answering(0).await;
        let a = app(ResponseTemplate::new(204)).await;
        let o = Opts {
            features: Some((true, false)),
            ..opts(g.port)
        };
        let h = start(&a, o);
        n_posts(&a, 1).await;
        h.cmd("c1", "gsx.gate", json!({"gate":"D 8"})).await;
        h.cmd("c2", "gsx.simbrief", json!({})).await;
        quiet().await;
        assert!(sent(&g).is_empty());
        assert_eq!(*h.out.borrow(), None);
    }

    #[tokio::test]
    async fn services_and_pushback_never_reach_gsx() {
        let g = gsx_answering(0).await;
        let a = app(ResponseTemplate::new(204)).await;
        let mut h = start(&a, opts(g.port));
        h.out.wait_for(Option::is_some).await.unwrap();
        h.cmd("c1", "gsx.service", json!({"service":"Boarding"})).await;
        h.cmd("c2", "gsx.push", json!({})).await;
        h.cmd("c3", "gsx.gate", json!({"gate":"D 8"})).await;
        h.last_is("c3", true, None).await;
        assert_eq!(sent(&g).len(), 1, "only the gate");
    }

    #[tokio::test]
    async fn gsx_read_off_holds_until_the_next_ticket() {
        let g = gsx_answering(0).await;
        let a = app(ResponseTemplate::new(204)).await;
        let mut h = start(&a, opts(g.port));
        h.out.wait_for(Option::is_some).await.unwrap();
        h.cmd("r1", "gsx.read", json!({"on":false})).await;
        h.out.wait_for(Option::is_none).await.unwrap();
        n_posts(&a, 2).await;
        assert_eq!(health(&posts(&a).await[1])["reading"], false);
        quiet().await;
        assert_eq!(*h.out.borrow(), None, "still off");
        // The next ticket wins again.
        h.features.send_replace(Some((true, true)));
        h.out.wait_for(Option::is_some).await.unwrap();
    }

    #[tokio::test]
    async fn gsx_read_on_reads_against_the_ticket_and_says_ok() {
        let g = gsx_answering(0).await;
        let a = app(ResponseTemplate::new(204)).await;
        let o = Opts {
            features: Some((true, false)),
            ..opts(g.port)
        };
        let mut h = start(&a, o);
        n_posts(&a, 1).await;
        h.cmd("r1", "gsx.read", json!({"on":true})).await;
        h.last_is("r1", true, None).await;
        h.features.send_replace(Some((true, false)));
        h.out.wait_for(Option::is_none).await.unwrap();
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
