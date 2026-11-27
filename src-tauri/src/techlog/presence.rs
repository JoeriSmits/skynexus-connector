//! The TechLog presence check: while a sim is connected and the full reader
//! is off, ping TechLog (never `/api/state`) and tell SkyNexus whether it
//! runs, its version and the loaded aircraft, so the EFB can offer to turn
//! TechLog on.
//!
//! - Pings every 60 s; the next ping is timed from the end of the previous
//!   one. A change of the aircraft title pings at once. When a sim connects
//!   without a title yet, the first ping waits up to 3 s for it, so loading
//!   a flight posts once, not null and then the title.
//! - A failed post (network, 5xx, 429) holds posting for a doubling backoff
//!   from 60 s, capped at 10 min (or `Retry-After` when longer), reset on
//!   success. A wake-up for a new title or ticket does not cut it short.
//! - POSTs `{mode: "presence", health, present, simAircraft}` only when
//!   (present, techlogVersion, simAircraft) changed since SkyNexus last
//!   accepted it. No tech log data leaves the PC.
//! - Runs only with a credential and `features.techlog`, and never while the
//!   full reader is on. A 503 `disabled` stops it until the next ticket, a
//!   401 until the credential changes, and a 422 keeps that state from being
//!   sent again.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::time::{sleep, timeout, Instant};

use super::client::{Code, TechlogClient};
use super::runner::{describe, health, valid_version, MAX_RETRY_AFTER};
use crate::api::{ApiClient, ApiError, Features};

/// Between pings.
pub const EVERY: Duration = Duration::from_secs(60);
/// How long the first ping after a sim connects waits for the title.
pub const TITLE_GRACE: Duration = Duration::from_secs(3);
/// The longest backoff after failed posts.
pub const BACKOFF_MAX: Duration = Duration::from_secs(600);
/// The longest `simAircraft` the server accepts, in characters.
pub const MAX_AIRCRAFT_CHARS: usize = 120;

pub struct PresenceCfg {
    /// The connector credential; None while signed out.
    pub credential: watch::Receiver<Option<String>>,
    /// `features` of every ticket, published on each renewal.
    pub features: watch::Receiver<Features>,
    /// The tray's "Read TechLog" switch.
    pub read_switch: watch::Receiver<bool>,
    /// True while a sim is connected with a flight loaded.
    pub sim: watch::Receiver<bool>,
    /// The loaded aircraft's title (MSFS only).
    pub aircraft: watch::Receiver<Option<String>>,
    /// Between pings, and the first backoff step.
    pub every: Duration,
    /// See [`TITLE_GRACE`].
    pub title_grace: Duration,
    /// See [`BACKOFF_MAX`].
    pub backoff_max: Duration,
}

/// The next backoff step: `every` first, then doubling, capped at `max`.
fn next_backoff(prev: Option<Duration>, every: Duration, max: Duration) -> Duration {
    prev.map_or(every, |b| b.saturating_mul(2)).min(max)
}

/// What the presence post reports; a post is due when it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    present: bool,
    version: Option<String>,
    aircraft: Option<String>,
}

/// Whether the full reader runs instead (it reports presence itself).
pub fn full_reader_on(read_switch: bool, features: Features) -> bool {
    read_switch || features.techlog_read
}

/// The title as the server takes it: trimmed, at most 120 characters, None
/// when empty.
fn aircraft(title: Option<&str>) -> Option<String> {
    let t = title?.trim();
    let t: String = t.chars().take(MAX_AIRCRAFT_CHARS).collect();
    let t = t.trim_end().to_string();
    (!t.is_empty()).then_some(t)
}

fn key(code: &Code, version: Option<String>, title: Option<&str>) -> Key {
    let present = matches!(code, Code::Ok | Code::Starting);
    Key {
        present,
        version: version.filter(|v| present && valid_version(v)),
        aircraft: aircraft(title),
    }
}

fn body(code: &Code, k: &Key) -> Value {
    json!({
        "mode": "presence",
        "health": health(code, k.version.as_deref()),
        "present": k.present,
        "simAircraft": k.aircraft,
    })
}

/// Runs until one of the watch senders is dropped. Abort the task to stop it.
pub async fn run(mut cfg: PresenceCfg, client: TechlogClient, api: ApiClient) {
    // What SkyNexus last accepted, and what it refused with 422.
    let mut last: Option<Key> = None;
    let mut refused_key: Option<Key> = None;
    let mut refused: Option<String> = None;
    let mut server_off = false;
    let mut last_code: Option<Code> = None;
    let mut failing: Option<std::mem::Discriminant<ApiError>> = None;
    let mut credential_seen: Option<String> = None;
    // The last backoff step, and no post before this.
    let mut backoff: Option<Duration> = None;
    let mut hold_until: Option<Instant> = None;
    // Whether the sim was on at the last ping (for the title grace).
    let mut sim_seen = false;
    loop {
        let credential = cfg.credential.borrow_and_update().clone();
        let features = *cfg.features.borrow_and_update();
        let switch = *cfg.read_switch.borrow_and_update();
        let sim = *cfg.sim.borrow_and_update();
        let title = cfg.aircraft.borrow_and_update().clone();
        let reader = full_reader_on(switch, features);
        if credential != credential_seen {
            // Another account (or a re-pair): its own refusal state.
            refused = None;
        }
        if credential != credential_seen || reader {
            // Another account, or the reader took over: send the state again.
            last = None;
            refused_key = None;
            credential_seen = credential.clone();
        }
        let blocked_cred = credential.is_none() || (refused.is_some() && credential == refused);
        if !sim || blocked_cred || !features.techlog || reader || server_off {
            last_code = None;
            if !sim {
                sim_seen = false;
            }
            tokio::select! {
                r = cfg.credential.changed() => if r.is_err() { return },
                r = cfg.read_switch.changed() => if r.is_err() { return },
                r = cfg.sim.changed() => if r.is_err() { return },
                r = cfg.aircraft.changed() => if r.is_err() { return },
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
        let mut title = title;
        if !sim_seen {
            sim_seen = true;
            if title.is_none() {
                // MSFS answers the title just after the flight loads.
                let _ = timeout(cfg.title_grace, cfg.aircraft.wait_for(Option::is_some)).await;
                title = cfg.aircraft.borrow_and_update().clone();
            }
        }

        let (code, version) = client.probe().await;
        if last_code.as_ref() != Some(&code) {
            log::info!("techlog presence: {}", describe(&code));
            last_code = Some(code.clone());
        }
        let k = key(&code, version, title.as_deref());
        let mut wait = cfg.every;
        let now = Instant::now();
        let held = hold_until.is_some_and(|t| now < t);
        if held {
            // Wake at the deadline at the latest, so the post is not late.
            wait = wait.min(hold_until.expect("held").duration_since(now));
        }
        if !held && last.as_ref() != Some(&k) && refused_key.as_ref() != Some(&k) {
            refused_key = None;
            let result = api.post_techlog(&credential, &body(&code, &k)).await;
            if !matches!(&result, Err(e) if failing == Some(std::mem::discriminant(e))) {
                failing = None;
            }
            match result {
                Ok(()) => {
                    last = Some(k);
                    backoff = None;
                    hold_until = None;
                }
                Err(ApiError::Unauthorized) => {
                    log::warn!("techlog presence: SkyNexus refused the credential; not sending until it changes");
                    refused = Some(credential.clone());
                }
                Err(ApiError::InvalidBody { path }) => {
                    log::warn!("techlog presence: SkyNexus refused the body at {path}; not resending this state");
                    refused_key = Some(k);
                }
                Err(ApiError::Disabled) => {
                    log::info!("techlog presence: switched off on SkyNexus; asking again on the next ticket");
                    server_off = true;
                    cfg.features.borrow_and_update();
                }
                Err(e) => {
                    let kind = std::mem::discriminant(&e);
                    if failing != Some(kind) {
                        log::warn!("techlog presence: cannot post ({e}); retrying");
                        failing = Some(kind);
                    }
                    let step = next_backoff(backoff, cfg.every, cfg.backoff_max);
                    backoff = Some(step);
                    let ra = e.retry_after().unwrap_or_default().min(MAX_RETRY_AFTER);
                    wait = step.max(ra);
                    hold_until = Some(Instant::now() + wait);
                }
            }
        }
        // Blocked now: the top of the loop waits for a change, so no spin.
        // Otherwise the sleep below always runs before the next ping.
        if server_off || refused.as_ref() == Some(&credential) {
            continue;
        }
        tokio::select! {
            _ = sleep(wait) => {}
            r = cfg.credential.changed() => if r.is_err() { return },
            r = cfg.read_switch.changed() => if r.is_err() { return },
            r = cfg.sim.changed() => if r.is_err() { return },
            r = cfg.aircraft.changed() => if r.is_err() { return },
            r = cfg.features.changed() => if r.is_err() { return },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::techlog::model::tests::fixture;
    use std::net::TcpListener;
    use url::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const A320: &str = "FenixA320 Fenix House Livery";
    const MS: Duration = Duration::from_millis(10);

    async fn techlog(ping: &str) -> MockServer {
        let s = MockServer::start().await;
        let resp = ResponseTemplate::new(200).set_body_string(ping);
        Mock::given(method("GET"))
            .and(path("/api/ping"))
            .respond_with(resp)
            .mount(&s)
            .await;
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

    struct H {
        features: watch::Sender<Features>,
        switch: watch::Sender<bool>,
        sim: watch::Sender<bool>,
        aircraft: watch::Sender<Option<String>>,
        credential: watch::Sender<Option<String>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for H {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn on() -> Features {
        Features {
            techlog: true,
            techlog_read: false,
        }
    }

    fn start(tl: &str, app: &MockServer, f: Features) -> H {
        start_with(tl, app, f, true, Some(A320), Duration::from_secs(1))
    }

    fn start_with(
        tl: &str,
        app: &MockServer,
        f: Features,
        sim_on: bool,
        title: Option<&str>,
        backoff_max: Duration,
    ) -> H {
        let (credential, credential_rx) = watch::channel(Some("snc_1".to_string()));
        let (features, features_rx) = watch::channel(f);
        let (switch, switch_rx) = watch::channel(false);
        let (sim, sim_rx) = watch::channel(sim_on);
        let (aircraft, aircraft_rx) = watch::channel(title.map(str::to_string));
        let cfg = PresenceCfg {
            credential: credential_rx,
            features: features_rx,
            read_switch: switch_rx,
            sim: sim_rx,
            aircraft: aircraft_rx,
            every: MS,
            title_grace: Duration::from_millis(400),
            backoff_max,
        };
        let url = Url::parse(tl).unwrap();
        let client = TechlogClient::with_timeout(url, Duration::from_millis(500));
        let api = ApiClient::new(&app.uri());
        let task = tokio::spawn(run(cfg, client, api));
        H {
            features,
            switch,
            sim,
            aircraft,
            credential,
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
            assert!(std::time::Instant::now() < end, "timed out: {what}");
            sleep(Duration::from_millis(5)).await;
        }
    }

    async fn pings(s: &MockServer, n: usize) {
        until("pings", async || hits(s, "/api/ping").await >= n).await;
    }

    async fn quiet() {
        sleep(Duration::from_millis(120)).await;
    }

    #[tokio::test]
    async fn posts_only_on_change_and_never_reads_state() {
        let tl = techlog(&fixture("ping.json")).await;
        let a = app(ResponseTemplate::new(204)).await;
        let h = start(&tl.uri(), &a, on());
        pings(&tl, 8).await;
        let sent = posts(&a).await;
        assert_eq!(sent.len(), 1, "eight pings, one post");
        let b = &sent[0];
        assert_eq!(b["mode"], "presence");
        assert_eq!(b["present"], true);
        assert_eq!(b["simAircraft"], A320);
        assert_eq!(b["health"]["code"], "OK");
        assert_eq!(b["health"]["techlogVersion"], "2.1.2");
        assert_eq!(b["health"]["schema"], 1);
        let v = env!("CARGO_PKG_VERSION");
        assert_eq!(b["health"]["connectorVersion"], v);
        assert!(b.get("snapshot").is_none());
        assert_eq!(hits(&tl, "/api/state").await, 0, "ping only");

        // another aircraft: one more post
        h.aircraft.send_replace(Some("PMDG 737-800".into()));
        until("2nd post", async || posts(&a).await.len() == 2).await;
        let n = hits(&tl, "/api/ping").await;
        pings(&tl, n + 5).await;
        let sent = posts(&a).await;
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1]["simAircraft"], "PMDG 737-800");

        // no aircraft title (X-Plane, replay): null
        h.aircraft.send_replace(None);
        until("3rd post", async || posts(&a).await.len() == 3).await;
        let sent = posts(&a).await;
        assert!(sent[2]["simAircraft"].is_null());
        assert_eq!(hits(&tl, "/api/state").await, 0, "ping only");
    }

    #[tokio::test]
    async fn absent_techlog_posts_present_false_once() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let a = app(ResponseTemplate::new(204)).await;
        let _h = start(&format!("http://{addr}"), &a, on());
        until("a post", async || !posts(&a).await.is_empty()).await;
        quiet().await;
        let sent = posts(&a).await;
        assert_eq!(sent.len(), 1, "several pings, one post");
        let b = &sent[0];
        assert_eq!(b["present"], false);
        assert_eq!(b["health"]["code"], "TL_NOT_RUNNING");
        assert!(b["health"].get("techlogVersion").is_none());
        assert_eq!(b["simAircraft"], A320);
    }

    #[tokio::test]
    async fn starting_is_present_and_an_odd_version_is_left_out() {
        let ping = r#"{"ok":true,"engine":false,"version":"v2"}"#;
        let tl = techlog(ping).await;
        let a = app(ResponseTemplate::new(204)).await;
        let _h = start(&tl.uri(), &a, on());
        until("a post", async || !posts(&a).await.is_empty()).await;
        let b = &posts(&a).await[0];
        assert_eq!(b["present"], true);
        assert_eq!(b["health"]["code"], "TL_STARTING");
        assert!(b["health"].get("techlogVersion").is_none());
    }

    #[tokio::test]
    async fn waits_for_a_sim_and_steps_aside_for_the_reader() {
        let tl = techlog(&fixture("ping.json")).await;
        let a = app(ResponseTemplate::new(204)).await;
        let h = start(&tl.uri(), &a, on());
        h.sim.send_replace(false);
        h.switch.send_replace(true);
        quiet().await;
        let n = hits(&tl, "/api/ping").await;
        quiet().await;
        assert_eq!(hits(&tl, "/api/ping").await, n, "no pings");
        // the sim alone is not enough while the reader is on
        h.sim.send_replace(true);
        quiet().await;
        assert_eq!(hits(&tl, "/api/ping").await, n);
        // reader off: presence again, the current state is sent
        let p = posts(&a).await.len();
        h.switch.send_replace(false);
        until("a post", async || posts(&a).await.len() == p + 1).await;
    }

    #[tokio::test]
    async fn an_opt_in_from_the_efb_hands_over_to_the_reader() {
        let tl = techlog(&fixture("ping.json")).await;
        let a = app(ResponseTemplate::new(204)).await;
        let h = start(&tl.uri(), &a, on());
        until("a post", async || posts(&a).await.len() == 1).await;
        let read = Features {
            techlog_read: true,
            ..on()
        };
        h.features.send_replace(read);
        quiet().await;
        let n = hits(&tl, "/api/ping").await;
        quiet().await;
        assert_eq!(hits(&tl, "/api/ping").await, n, "presence steps aside");
        // kill switch with the opt-in still set: neither runs
        let killed = Features {
            techlog: false,
            techlog_read: true,
        };
        h.features.send_replace(killed);
        quiet().await;
        assert_eq!(hits(&tl, "/api/ping").await, n);
        assert_eq!(posts(&a).await.len(), 1);
    }

    #[tokio::test]
    async fn the_kill_switch_stops_it() {
        let tl = techlog(&fixture("ping.json")).await;
        let a = app(ResponseTemplate::new(204)).await;
        let h = start(&tl.uri(), &a, Features::default());
        quiet().await;
        assert_eq!(hits(&tl, "/api/ping").await, 0);
        assert!(posts(&a).await.is_empty());
        h.features.send_replace(on());
        until("a post", async || posts(&a).await.len() == 1).await;
        h.features.send_replace(Features::default());
        quiet().await;
        let n = hits(&tl, "/api/ping").await;
        quiet().await;
        assert_eq!(hits(&tl, "/api/ping").await, n, "stopped");
    }

    #[tokio::test]
    async fn refusals_do_not_storm() {
        // 401: nothing until the credential changes
        let tl = techlog(&fixture("ping.json")).await;
        let body = json!({ "error": "revoked" });
        let a = app(ResponseTemplate::new(401).set_body_json(body)).await;
        let h = start(&tl.uri(), &a, on());
        until("a post", async || posts(&a).await.len() == 1).await;
        quiet().await;
        assert_eq!(posts(&a).await.len(), 1);
        assert_eq!(hits(&tl, "/api/ping").await, 1);
        h.credential.send_replace(Some("snc_2".into()));
        until("2nd post", async || posts(&a).await.len() == 2).await;

        // 422: that state is not sent again
        let tl = techlog(&fixture("ping.json")).await;
        let body = json!({ "error": "invalid_body", "path": "simAircraft" });
        let a = app(ResponseTemplate::new(422).set_body_json(body)).await;
        let h = start(&tl.uri(), &a, on());
        pings(&tl, 8).await;
        assert_eq!(posts(&a).await.len(), 1);
        h.aircraft.send_replace(Some("PMDG 777-300ER".into()));
        until("new state", async || posts(&a).await.len() == 2).await;

        // 503 disabled: nothing until the next ticket
        let tl = techlog(&fixture("ping.json")).await;
        let body = json!({ "error": "disabled" });
        let a = app(ResponseTemplate::new(503).set_body_json(body)).await;
        let h = start(&tl.uri(), &a, on());
        until("a post", async || posts(&a).await.len() == 1).await;
        quiet().await;
        assert_eq!(hits(&tl, "/api/ping").await, 1);
        h.features.send_replace(on());
        until("2nd post", async || posts(&a).await.len() == 2).await;
    }

    #[tokio::test]
    async fn a_new_credential_after_a_401_posts_without_spinning() {
        let tl = techlog(&fixture("ping.json")).await;
        let a = MockServer::start().await;
        let body = json!({ "error": "revoked" });
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_json(body))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&a)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .with_priority(5)
            .mount(&a)
            .await;
        let h = start(&tl.uri(), &a, on());
        until("the 401", async || posts(&a).await.len() == 1).await;
        quiet().await;
        assert_eq!(hits(&tl, "/api/ping").await, 1, "waits while refused");
        h.credential.send_replace(Some("snc_2".into()));
        until("2nd post", async || posts(&a).await.len() == 2).await;
        let reqs = a.received_requests().await.unwrap();
        assert_eq!(reqs[1].headers["authorization"], "Bearer snc_2");
        // every is 10 ms: about 12 pings in 120 ms; a spin is hundreds
        let n = hits(&tl, "/api/ping").await;
        quiet().await;
        let more = hits(&tl, "/api/ping").await - n;
        assert!(more <= 20, "{more} pings in 120 ms");
        assert_eq!(posts(&a).await.len(), 2, "unchanged state");
    }

    #[tokio::test]
    async fn a_flight_load_posts_once_with_the_title() {
        let tl = techlog(&fixture("ping.json")).await;
        let a = app(ResponseTemplate::new(204)).await;
        let f = on();
        let s = Duration::from_secs(1);
        let h = start_with(&tl.uri(), &a, f, false, None, s);
        quiet().await;
        // the sim connects; the title follows 50 ms later
        h.sim.send_replace(true);
        sleep(Duration::from_millis(50)).await;
        assert_eq!(hits(&tl, "/api/ping").await, 0, "waits for the title");
        h.aircraft.send_replace(Some(A320.into()));
        until("a post", async || !posts(&a).await.is_empty()).await;
        quiet().await;
        let sent = posts(&a).await;
        assert_eq!(sent.len(), 1, "not null, then the title");
        assert_eq!(sent[0]["simAircraft"], A320);

        // no title at all (X-Plane): null after the grace, once
        let tl = techlog(&fixture("ping.json")).await;
        let a = app(ResponseTemplate::new(204)).await;
        let _h = start_with(&tl.uri(), &a, on(), true, None, s);
        sleep(Duration::from_millis(200)).await;
        assert!(posts(&a).await.is_empty(), "inside the grace");
        until("a post", async || !posts(&a).await.is_empty()).await;
        assert!(posts(&a).await[0]["simAircraft"].is_null());
    }

    #[tokio::test]
    async fn retry_after_is_not_cut_short_by_a_new_title() {
        let tl = techlog(&fixture("ping.json")).await;
        let a = MockServer::start().await;
        let limited = ResponseTemplate::new(429).insert_header("retry-after", "1");
        Mock::given(method("POST"))
            .respond_with(limited)
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&a)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .with_priority(5)
            .mount(&a)
            .await;
        let h = start(&tl.uri(), &a, on());
        until("the 429", async || posts(&a).await.len() == 1).await;
        let t0 = std::time::Instant::now();
        for i in 0..5 {
            h.aircraft.send_replace(Some(format!("PMDG 737-{i}00")));
            sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(posts(&a).await.len(), 1, "held for Retry-After");
        until("the post after it", async || posts(&a).await.len() == 2).await;
        assert!(t0.elapsed() >= Duration::from_millis(850));
        assert_eq!(posts(&a).await[1]["simAircraft"], "PMDG 737-400");
    }

    #[tokio::test]
    async fn failed_posts_back_off_and_success_resets() {
        let tl = techlog(&fixture("ping.json")).await;
        let a = MockServer::start().await;
        let body = json!({ "error": "unavailable" });
        let down = ResponseTemplate::new(503).set_body_json(body);
        Mock::given(method("POST"))
            .respond_with(down)
            .up_to_n_times(5)
            .with_priority(1)
            .mount(&a)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .with_priority(5)
            .mount(&a)
            .await;
        // steps 10, 20, 40, 80, 80 ms: about 230 ms for 5 failures
        let cap = Duration::from_millis(80);
        let h = start_with(&tl.uri(), &a, on(), true, Some(A320), cap);
        let t0 = std::time::Instant::now();
        until("six posts", async || posts(&a).await.len() == 6).await;
        let took = t0.elapsed();
        assert!(took >= Duration::from_millis(200), "{took:?}");
        // reset: a change after the success posts within one step
        h.aircraft.send_replace(Some("PMDG 777-300ER".into()));
        until("next", async || posts(&a).await.len() == 7).await;
    }

    #[test]
    fn backoff_doubles_from_every_up_to_the_cap() {
        let (e, max) = (EVERY, BACKOFF_MAX);
        let mut b = None;
        let mut steps = Vec::new();
        for _ in 0..6 {
            let s = next_backoff(b, e, max);
            steps.push(s.as_secs());
            b = Some(s);
        }
        assert_eq!(steps, vec![60, 120, 240, 480, 600, 600]);
    }

    #[test]
    fn aircraft_is_trimmed_and_capped() {
        assert_eq!(aircraft(Some("  A320  ")).as_deref(), Some("A320"));
        assert_eq!(aircraft(Some("   ")), None);
        assert_eq!(aircraft(None), None);
        let long = "x".repeat(300);
        assert_eq!(aircraft(Some(&long)).unwrap().chars().count(), 120);
        let wide = "é".repeat(200);
        let a = aircraft(Some(&wide)).unwrap();
        assert_eq!(a.chars().count(), 120, "characters, not bytes");
    }

    #[test]
    fn key_reads_present_from_the_code() {
        let v = || Some("2.1.2".to_string());
        assert!(key(&Code::Ok, v(), None).present);
        assert!(key(&Code::Starting, None, None).present);
        for c in [Code::NotRunning, Code::NotTechlog, Code::Http(404)] {
            let k = key(&c, v(), None);
            assert!(!k.present && k.version.is_none(), "{c:?}");
        }
        assert_eq!(key(&Code::Ok, Some("x".into()), None).version, None);
    }
}
