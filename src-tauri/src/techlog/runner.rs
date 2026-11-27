//! The TechLog runner: reads TechLog on a cadence and POSTs health and the
//! snapshot to SkyNexus when something changed.
//!
//! - Reads every 15 s while TechLog answers `OK`, every 30 s otherwise; the
//!   next read is timed from the end of the previous one (a read can take
//!   about 10 s when both requests time out).
//! - POSTs when the code or the snapshot hash changed, and every 20 min while
//!   TechLog is present so the server knows the data is still fresh.
//! - After SkyNexus accepted a snapshot, its short hash goes on `tl_out` for
//!   the own-ship frame; after a health-only POST, `tl_out` is cleared.
//! - Runs only while the tray switch is on, a credential is there and the
//!   last ticket said `features.techlog`. A 503 `disabled` stops it until the
//!   next ticket; a 401 stops it until the credential changes.
//!
//! The snapshot body is never logged.

use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::watch;
use tokio::time::{sleep, Instant};

use super::client::{Code, TechlogClient};
use super::snapshot::{self, Snapshot};
use crate::api::{ApiClient, ApiError, Features};

/// The server refuses bodies above this many bytes.
pub const MAX_BODY_BYTES: usize = 32_768;
/// The health object's schema version.
pub const HEALTH_SCHEMA: u8 = 1;
/// The longest `Retry-After` honoured from the TechLog route.
pub(super) const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);

/// Resend an unchanged `OK` snapshot this often: well inside the EFB's 30 min freshness window,
/// so data the pilot is looking at never ages to stale while TechLog is up.
pub const KEEPALIVE: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone, Copy)]
pub struct RunnerTiming {
    /// Between reads while TechLog does not answer `OK`.
    pub absent_every: Duration,
    /// Between reads while TechLog answers `OK`.
    pub present_every: Duration,
    /// POST an unchanged `OK` snapshot again after this long.
    pub keepalive: Duration,
}

impl Default for RunnerTiming {
    fn default() -> Self {
        RunnerTiming {
            absent_every: Duration::from_secs(30),
            present_every: Duration::from_secs(15),
            keepalive: KEEPALIVE,
        }
    }
}

pub struct RunnerCfg {
    /// The connector credential; None while signed out.
    pub credential: watch::Receiver<Option<String>>,
    /// `features` of every ticket, published on each renewal.
    pub features: watch::Receiver<Features>,
    pub timing: RunnerTiming,
}

type Key = (Code, Option<String>);

fn same(k: &Option<Key>, code: &Code, hash: Option<&str>) -> bool {
    k.as_ref().is_some_and(|(c, h)| c == code && h.as_deref() == hash)
}

/// What SkyNexus last accepted, and what it last refused with 422 (never
/// sent again, not even as a keep-alive, until the state changes).
#[derive(Debug, Default)]
struct Tracker {
    last: Option<Key>,
    last_post_at: Option<Instant>,
    refused: Option<Key>,
}

impl Tracker {
    fn due(&mut self, code: &Code, hash: Option<&str>, now: Instant, keepalive: Duration) -> bool {
        if same(&self.refused, code, hash) {
            return false;
        }
        self.refused = None;
        let changed = !same(&self.last, code, hash);
        let stale = *code == Code::Ok && self.last_post_at.is_none_or(|at| now.duration_since(at) >= keepalive);
        changed || stale
    }

    fn sent(&mut self, code: &Code, hash: Option<&str>, now: Instant) {
        self.last = Some((code.clone(), hash.map(str::to_string)));
        self.last_post_at = Some(now);
    }

    fn refused(&mut self, code: &Code, hash: Option<&str>) {
        self.refused = Some((code.clone(), hash.map(str::to_string)));
    }
}

/// The server's pattern for `techlogVersion`: `^\d{1,4}(\.\d{1,4}){1,3}$`.
pub(super) fn valid_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|p| (1..=4).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The TechLog version as the server accepts it. A read that is otherwise
/// `OK` but has no usable version becomes `TL_SCHEMA` at `ping.version`, so
/// it goes out as health only.
fn checked_version(code: Code, version: Option<String>) -> (Code, Option<String>) {
    let version = version.filter(|v| valid_version(v));
    match (code, version) {
        (Code::Ok, None) => (Code::Schema("ping.version".into()), None),
        (code, version) => (code, version),
    }
}

/// The `health` object. Optional keys are left out, never sent as null.
pub(super) fn health(code: &Code, techlog_version: Option<&str>) -> Value {
    let mut h = Map::new();
    h.insert("code".into(), json!(code.as_str()));
    if let Some(p) = code.path() {
        h.insert("path".into(), json!(p));
    }
    if let Some(s) = code.http_status() {
        h.insert("httpStatus".into(), json!(s));
    }
    if let Some(v) = techlog_version {
        h.insert("techlogVersion".into(), json!(v));
    }
    h.insert("connectorVersion".into(), json!(env!("CARGO_PKG_VERSION")));
    h.insert("schema".into(), json!(HEALTH_SCHEMA));
    Value::Object(h)
}

/// The POST body, kept under [`MAX_BODY_BYTES`] by dropping items from the
/// end of the snapshot (the hash is recomputed each time). Returns the body
/// and the hash of the snapshot it carries.
fn body(health: Value, snapshot: Option<Snapshot>) -> (Value, Option<String>) {
    let Some(mut snap) = snapshot else {
        return (json!({ "health": health }), None);
    };
    loop {
        let b = json!({ "health": health, "snapshot": snap });
        if serde_json::to_vec(&b).map_or(0, |v| v.len()) <= MAX_BODY_BYTES {
            return (b, Some(snap.hash));
        }
        if snap.items.pop().is_none() {
            log::warn!(
                "techlog: the snapshot does not fit in {MAX_BODY_BYTES} bytes even without items; sending health only"
            );
            return (json!({ "health": health }), None);
        }
        snap.hash = snapshot::hash(&snap);
    }
}

fn publish(tl_out: &watch::Sender<Option<String>>, v: Option<String>) {
    tl_out.send_if_modified(|cur| {
        let changed = *cur != v;
        *cur = v;
        changed
    });
}

pub(super) fn describe(code: &Code) -> String {
    match (code.path(), code.http_status()) {
        (Some(p), _) => format!("{} at {p}", code.as_str()),
        (_, Some(s)) => format!("{} {s}", code.as_str()),
        _ => code.as_str().to_string(),
    }
}

/// Whether the runner must not read now: switched off, signed out, a
/// refused credential, or SkyNexus has the link off.
fn blocked(on: bool, credential: Option<&String>, refused: Option<&String>, feature: bool, server_off: bool) -> bool {
    !on || credential.is_none() || (refused.is_some() && credential == refused) || !feature || server_off
}

/// Runs until one of the watch senders is dropped. Abort the task to stop it.
pub async fn run(
    mut cfg: RunnerCfg,
    client: TechlogClient,
    api: ApiClient,
    tl_out: watch::Sender<Option<String>>,
    mut enabled: watch::Receiver<bool>,
) {
    let timing = cfg.timing;
    let mut tracker = Tracker::default();
    let mut last_code: Option<Code> = None;
    // The credential SkyNexus refused with 401; skip it until it changes.
    let mut refused: Option<String> = None;
    // 503 `disabled`: wait for the next ticket.
    let mut server_off = false;
    // The kind of retryable post error already logged in this streak.
    let mut post_failing: Option<std::mem::Discriminant<ApiError>> = None;
    loop {
        let credential = cfg.credential.borrow_and_update().clone();
        let feature = cfg.features.borrow_and_update().techlog;
        let on = *enabled.borrow_and_update();
        if blocked(on, credential.as_ref(), refused.as_ref(), feature, server_off) {
            publish(&tl_out, None);
            // Start over when it runs again, so the current state is sent.
            tracker = Tracker::default();
            last_code = None;
            tokio::select! {
                r = enabled.changed() => if r.is_err() { return },
                r = cfg.credential.changed() => if r.is_err() { return },
                r = cfg.features.changed() => {
                    if r.is_err() {
                        return;
                    }
                    server_off = false;
                }
            }
            continue;
        }
        let credential = credential.expect("checked above");

        let (code, version, state) = client.read().await;
        let (code, version) = checked_version(code, version);
        if last_code.as_ref() != Some(&code) {
            log::info!("techlog: {}", describe(&code));
            last_code = Some(code.clone());
        }
        let snap = match (&code, &state) {
            (Code::Ok, Some(s)) => version
                .as_deref()
                .and_then(|v| snapshot::build(s, v, chrono::Utc::now())),
            _ => None,
        };
        let hash = snap.as_ref().map(|s| s.hash.clone());
        let mut wait = if code == Code::Ok {
            timing.present_every
        } else {
            timing.absent_every
        };
        let now = Instant::now();
        if tracker.due(&code, hash.as_deref(), now, timing.keepalive) {
            let (b, sent_hash) = body(health(&code, version.as_deref()), snap);
            let result = api.post_techlog(&credential, &b).await;
            // A success, or an error that is not retried, ends the streak.
            if !matches!(
                result,
                Err(ApiError::Network(_)
                    | ApiError::NotEnabled
                    | ApiError::RateLimited { .. }
                    | ApiError::NotConfigured { .. }
                    | ApiError::Unavailable { .. }
                    | ApiError::Protocol(_))
            ) {
                post_failing = None;
            }
            match result {
                Ok(()) => {
                    // Tracked by the read hash, so a trimmed snapshot is not re-sent every read.
                    tracker.sent(&code, hash.as_deref(), now);
                    publish(&tl_out, sent_hash.as_deref().map(snapshot::short));
                }
                Err(ApiError::Unauthorized) => {
                    log::warn!("techlog: SkyNexus refused the credential; not sending until it changes");
                    refused = Some(credential.clone());
                }
                Err(ApiError::InvalidBody { path }) => {
                    log::warn!("techlog: SkyNexus refused the body at {path}; not resending this state");
                    tracker.refused(&code, hash.as_deref());
                }
                Err(ApiError::Disabled) => {
                    log::info!("techlog: switched off on SkyNexus; asking again on the next ticket");
                    server_off = true;
                    // Mark the current ticket as seen: only a newer one turns it back on.
                    cfg.features.borrow_and_update();
                }
                Err(e) => {
                    // Once per streak of the same kind, not on every retry.
                    let kind = std::mem::discriminant(&e);
                    if post_failing != Some(kind) {
                        if matches!(e, ApiError::Network(_)) {
                            log::warn!("techlog: cannot reach SkyNexus ({e}); retrying");
                        } else {
                            log::info!("techlog: SkyNexus answered: {e}; retrying");
                        }
                        post_failing = Some(kind);
                    }
                    wait = wait.max(e.retry_after().unwrap_or_default().min(MAX_RETRY_AFTER));
                }
            }
        }
        if blocked(on, Some(&credential), refused.as_ref(), feature, server_off) {
            continue;
        }
        tokio::select! {
            _ = sleep(wait) => {}
            r = enabled.changed() => if r.is_err() { return },
            r = cfg.credential.changed() => if r.is_err() { return },
            r = cfg.features.changed() => if r.is_err() { return },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::techlog::model::{parse_state, tests::fixture};
    use std::net::TcpListener;
    use url::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const STATE: &str = "state-fenix-go-mel.json";

    fn fast() -> RunnerTiming {
        RunnerTiming {
            absent_every: Duration::from_millis(20),
            present_every: Duration::from_millis(10),
            keepalive: Duration::from_secs(600),
        }
    }

    /// A TechLog state with the first defect's title changed.
    fn changed_state() -> String {
        let mut v: Value = serde_json::from_str(&fixture(STATE)).unwrap();
        v["defects"][0]["title"] = json!("Reverser 2 slow to deploy");
        v.to_string()
    }

    async fn mount_techlog(s: &MockServer, state: ResponseTemplate, times: Option<u64>, priority: u8) {
        let mut m = Mock::given(method("GET"))
            .and(path("/api/state"))
            .respond_with(state)
            .with_priority(priority);
        if let Some(n) = times {
            m = m.up_to_n_times(n);
        }
        m.mount(s).await;
    }

    async fn techlog() -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ping"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture("ping.json")))
            .mount(&s)
            .await;
        s
    }

    async fn techlog_with(state: &str) -> MockServer {
        let s = techlog().await;
        mount_techlog(&s, ResponseTemplate::new(200).set_body_string(state), None, 5).await;
        s
    }

    async fn app(resp: ResponseTemplate) -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/techlog"))
            .respond_with(resp)
            .mount(&s)
            .await;
        s
    }

    struct Harness {
        tl: watch::Receiver<Option<String>>,
        enabled: watch::Sender<bool>,
        credential: watch::Sender<Option<String>>,
        features: watch::Sender<Features>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn start(techlog: &str, app: &MockServer, timing: RunnerTiming, feature: bool) -> Harness {
        let (tl_tx, tl) = watch::channel(None);
        let (enabled, enabled_rx) = watch::channel(true);
        let (credential, credential_rx) = watch::channel(Some("snc_1".to_string()));
        let (features, features_rx) = watch::channel(Features { techlog: feature });
        let cfg = RunnerCfg {
            credential: credential_rx,
            features: features_rx,
            timing,
        };
        let client = TechlogClient::with_timeout(Url::parse(techlog).unwrap(), Duration::from_millis(500));
        let task = tokio::spawn(run(cfg, client, ApiClient::new(&app.uri()), tl_tx, enabled_rx));
        Harness {
            tl,
            enabled,
            credential,
            features,
            task,
        }
    }

    async fn hits(s: &MockServer, p: &str) -> usize {
        let reqs = s.received_requests().await.unwrap();
        reqs.iter().filter(|r| r.url.path() == p).count()
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

    async fn pings_at_least(s: &MockServer, n: usize) {
        until("reads", async || hits(s, "/api/ping").await >= n).await;
    }

    fn code(b: &Value) -> &str {
        b["health"]["code"].as_str().unwrap()
    }

    #[tokio::test]
    async fn unchanged_state_posts_once_and_marks_the_frame() {
        let tl = techlog_with(&fixture(STATE)).await;
        let a = app(ResponseTemplate::new(204)).await;
        let h = start(&tl.uri(), &a, fast(), true);
        pings_at_least(&tl, 10).await;
        let sent = posts(&a).await;
        assert_eq!(sent.len(), 1, "ten reads of the same state, one POST");
        let b = &sent[0];
        assert_eq!(code(b), "OK");
        assert_eq!(b["health"]["techlogVersion"], "2.1.2");
        assert_eq!(b["health"]["schema"], 1);
        assert!(b["health"].get("path").is_none() && b["health"].get("httpStatus").is_none());
        let hash = b["snapshot"]["hash"].as_str().unwrap();
        let tl_now = h.tl.borrow().clone().unwrap();
        assert_eq!(tl_now.len(), 12);
        assert_eq!(tl_now, snapshot::short(hash));
    }

    #[tokio::test]
    async fn a_changed_defect_posts_again() {
        let tl = techlog().await;
        let first = ResponseTemplate::new(200).set_body_string(fixture(STATE));
        mount_techlog(&tl, first, Some(3), 1).await;
        let later = ResponseTemplate::new(200).set_body_string(changed_state());
        mount_techlog(&tl, later, None, 5).await;
        let a = app(ResponseTemplate::new(204)).await;
        let h = start(&tl.uri(), &a, fast(), true);
        pings_at_least(&tl, 10).await;
        let sent = posts(&a).await;
        assert_eq!(sent.len(), 2);
        let (h1, h2) = (&sent[0]["snapshot"]["hash"], &sent[1]["snapshot"]["hash"]);
        assert_ne!(h1, h2);
        let want = snapshot::short(h2.as_str().unwrap());
        assert_eq!(h.tl.borrow().as_deref(), Some(want.as_str()));
    }

    #[tokio::test]
    async fn not_running_then_ok_posts_each_code_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let a = app(ResponseTemplate::new(204)).await;
        let h = start(&format!("http://{addr}"), &a, fast(), true);
        until("the TL_NOT_RUNNING post", async || !posts(&a).await.is_empty()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let sent = posts(&a).await;
        assert_eq!(sent.len(), 1, "several absent reads, one POST");
        assert_eq!(code(&sent[0]), "TL_NOT_RUNNING");
        assert!(sent[0].get("snapshot").is_none());
        assert!(
            sent[0]["health"].get("techlogVersion").is_none(),
            "absent keys are left out"
        );
        assert_eq!(*h.tl.borrow(), None);

        // TechLog starts on the same port.
        let tl = MockServer::builder()
            .listener(TcpListener::bind(addr).unwrap())
            .start()
            .await;
        Mock::given(method("GET"))
            .and(path("/api/ping"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture("ping.json")))
            .mount(&tl)
            .await;
        mount_techlog(&tl, ResponseTemplate::new(200).set_body_string(fixture(STATE)), None, 5).await;
        pings_at_least(&tl, 6).await;
        let sent = posts(&a).await;
        let codes: Vec<&str> = sent.iter().map(code).collect();
        assert_eq!(codes, vec!["TL_NOT_RUNNING", "OK"]);
        assert!(h.tl.borrow().is_some());
    }

    #[tokio::test]
    async fn an_unchanged_snapshot_is_sent_again_after_the_keepalive() {
        let tl = techlog_with(&fixture(STATE)).await;
        let a = app(ResponseTemplate::new(204)).await;
        let timing = RunnerTiming {
            keepalive: Duration::from_millis(150),
            ..fast()
        };
        let _h = start(&tl.uri(), &a, timing, true);
        until("three posts", async || posts(&a).await.len() >= 3).await;
        let sent = posts(&a).await;
        assert!(sent
            .iter()
            .all(|b| b["snapshot"]["hash"] == sent[0]["snapshot"]["hash"]));
        assert!(
            hits(&tl, "/api/ping").await > sent.len() * 3,
            "keepalive, not every read"
        );
    }

    #[tokio::test]
    async fn disabled_reads_and_sends_nothing() {
        // the tray switch is off
        let tl = techlog_with(&fixture(STATE)).await;
        let a = app(ResponseTemplate::new(204)).await;
        let h = start(&tl.uri(), &a, fast(), true);
        h.enabled.send_replace(false);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (reads, sent) = (hits(&tl, "/api/ping").await, posts(&a).await.len());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(hits(&tl, "/api/ping").await, reads, "no reads while off");
        assert_eq!(posts(&a).await.len(), sent);
        assert_eq!(*h.tl.borrow(), None);

        // the ticket says features.techlog false (or none)
        let tl = techlog_with(&fixture(STATE)).await;
        let a = app(ResponseTemplate::new(204)).await;
        let h = start(&tl.uri(), &a, fast(), false);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(hits(&tl, "/api/ping").await, 0);
        assert!(posts(&a).await.is_empty());
        // the next ticket allows it
        h.features.send_replace(Features { techlog: true });
        until("a post", async || posts(&a).await.len() == 1).await;
    }

    #[tokio::test]
    async fn server_kill_switch_stops_until_the_next_ticket() {
        let tl = techlog_with(&fixture(STATE)).await;
        let a = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({ "error": "disabled" })))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&a)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .with_priority(5)
            .mount(&a)
            .await;
        let h = start(&tl.uri(), &a, fast(), true);
        until("the refused post", async || posts(&a).await.len() == 1).await;
        let reads = hits(&tl, "/api/ping").await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(hits(&tl, "/api/ping").await, reads, "stopped reading");
        assert_eq!(*h.tl.borrow(), None);
        // a renewed ticket, still allowed
        h.features.send_replace(Features { techlog: true });
        until("the post after renewal", async || posts(&a).await.len() == 2).await;
        until("tl", async || h.tl.borrow().is_some()).await;
    }

    #[tokio::test]
    async fn a_refused_credential_stops_until_it_changes() {
        let tl = techlog_with(&fixture(STATE)).await;
        let a = app(ResponseTemplate::new(401).set_body_json(json!({ "error": "revoked" }))).await;
        let h = start(&tl.uri(), &a, fast(), true);
        until("the refused post", async || posts(&a).await.len() == 1).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(posts(&a).await.len(), 1);
        assert_eq!(hits(&tl, "/api/ping").await, 1, "no reads for a refused credential");
        h.credential.send_replace(Some("snc_2".into()));
        until("a post with the new credential", async || posts(&a).await.len() == 2).await;
        let reqs = a.received_requests().await.unwrap();
        assert_eq!(reqs[1].headers["authorization"], "Bearer snc_2");
    }

    #[tokio::test]
    async fn an_invalid_body_is_not_resent() {
        let tl = techlog().await;
        let first = ResponseTemplate::new(200).set_body_string(fixture(STATE));
        mount_techlog(&tl, first, Some(40), 1).await;
        let later = ResponseTemplate::new(200).set_body_string(changed_state());
        mount_techlog(&tl, later, None, 5).await;
        let invalid = json!({ "error": "invalid_body", "path": "snapshot.dispatch" });
        let a = app(ResponseTemplate::new(422).set_body_json(invalid)).await;
        let timing = RunnerTiming {
            keepalive: Duration::from_millis(30),
            ..fast()
        };
        let h = start(&tl.uri(), &a, timing, true);
        pings_at_least(&tl, 18).await;
        assert_eq!(posts(&a).await.len(), 1, "not even as a keep-alive");
        assert_eq!(*h.tl.borrow(), None);
        // the state changes: that is sent
        until("the post of the new state", async || posts(&a).await.len() == 2).await;
    }

    async fn techlog_with_ping(ping: &str) -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ping"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ping))
            .mount(&s)
            .await;
        mount_techlog(&s, ResponseTemplate::new(200).set_body_string(fixture(STATE)), None, 5).await;
        s
    }

    #[tokio::test]
    async fn a_missing_or_odd_version_sends_health_only() {
        for ping in [
            r#"{"ok":true,"app":"TechLog"}"#,
            r#"{"ok":true,"app":"TechLog","version":"v2.1.2"}"#,
        ] {
            let tl = techlog_with_ping(ping).await;
            let a = app(ResponseTemplate::new(204)).await;
            let h = start(&tl.uri(), &a, fast(), true);
            pings_at_least(&tl, 5).await;
            let sent = posts(&a).await;
            assert_eq!(sent.len(), 1, "{ping}");
            assert_eq!(code(&sent[0]), "TL_SCHEMA");
            assert_eq!(sent[0]["health"]["path"], "ping.version");
            assert!(sent[0]["health"].get("techlogVersion").is_none(), "{ping}");
            assert!(sent[0].get("snapshot").is_none(), "{ping}");
            assert_eq!(*h.tl.borrow(), None);
        }
    }

    #[test]
    fn version_pattern() {
        for ok in ["2.1", "2.1.2", "2026.11.26.1", "0.0"] {
            assert!(valid_version(ok), "{ok}");
        }
        for bad in [
            "",
            "2",
            "v2.1.2",
            "2.1.2-beta",
            "2.1.2.3.4",
            "12345.1",
            "2..1",
            "2.1.",
            "2.x",
        ] {
            assert!(!valid_version(bad), "{bad}");
        }
        let (c, v) = checked_version(Code::Http(500), Some("v2".into()));
        assert_eq!(
            (c, v),
            (Code::Http(500), None),
            "an odd version is just left out of other codes"
        );
    }

    #[tokio::test]
    async fn a_failed_post_is_retried_on_the_next_read() {
        let tl = techlog_with(&fixture(STATE)).await;
        let a = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({ "error": "unavailable" })))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&a)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .with_priority(5)
            .mount(&a)
            .await;
        let h = start(&tl.uri(), &a, fast(), true);
        pings_at_least(&tl, 8).await;
        assert_eq!(posts(&a).await.len(), 2);
        assert!(h.tl.borrow().is_some());
    }

    #[test]
    fn default_keepalive_is_inside_the_freshness_window() {
        assert_eq!(RunnerTiming::default().keepalive, Duration::from_secs(20 * 60));
        assert!(KEEPALIVE < Duration::from_secs(30 * 60));
    }

    #[test]
    fn keepalive_only_while_present() {
        let t0 = Instant::now();
        let k = KEEPALIVE;
        let mut t = Tracker::default();
        t.refused(&Code::Ok, Some("h0"));
        assert!(
            !t.due(&Code::Ok, Some("h0"), t0 + k, k),
            "a refused state is never resent"
        );
        assert!(t.due(&Code::Ok, Some("h1"), t0, k), "first read");
        t.sent(&Code::Ok, Some("h1"), t0);
        assert!(!t.due(&Code::Ok, Some("h1"), t0 + Duration::from_secs(19 * 60), k));
        assert!(t.due(&Code::Ok, Some("h1"), t0 + k, k), "20 min keepalive");
        assert!(t.due(&Code::Ok, Some("h2"), t0, k), "hash changed");
        t.sent(&Code::NotRunning, None, t0);
        assert!(
            !t.due(&Code::NotRunning, None, t0 + Duration::from_secs(3600), k),
            "no keepalive when absent"
        );
        assert!(t.due(&Code::Schema("dispatch".into()), None, t0, k), "code changed");
    }

    #[test]
    fn health_leaves_out_absent_keys() {
        let h = health(&Code::Schema("dispatch.status".into()), Some("2.1.2"));
        assert_eq!(h["path"], "dispatch.status");
        assert!(h.get("httpStatus").is_none());
        let h = health(&Code::Http(500), None);
        assert_eq!(h["code"], "TL_HTTP");
        assert_eq!(h["httpStatus"], 500);
        assert!(h.get("path").is_none() && h.get("techlogVersion").is_none());
        assert_eq!(h["connectorVersion"], env!("CARGO_PKG_VERSION"));
        assert_eq!(h["schema"], 1);
    }

    #[test]
    fn a_big_snapshot_drops_items_from_the_end_until_it_fits() {
        let state = parse_state(&fixture(STATE)).unwrap();
        let mut snap = snapshot::build(&state, "2.1.2", chrono::Utc::now()).unwrap();
        let template = snap.items[0].clone();
        snap.items = (0..60)
            .map(|i| snapshot::Item {
                id: i,
                placard: Some("x".repeat(900)),
                ..template.clone()
            })
            .collect();
        snap.hash = snapshot::hash(&snap);
        let (b, hash) = body(health(&Code::Ok, Some("2.1.2")), Some(snap.clone()));
        assert!(serde_json::to_vec(&b).unwrap().len() <= MAX_BODY_BYTES);
        let kept = b["snapshot"]["items"].as_array().unwrap().len();
        assert!(kept < 60 && kept > 0, "{kept}");
        assert_eq!(
            b["snapshot"]["items"][kept - 1]["id"],
            kept - 1,
            "the first items are kept"
        );
        snap.items.truncate(kept);
        let want = snapshot::hash(&snap);
        assert_eq!(hash.as_deref(), Some(want.as_str()), "hash recomputed");
        assert_eq!(b["snapshot"]["hash"], want);
    }
}
