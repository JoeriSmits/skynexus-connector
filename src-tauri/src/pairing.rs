//! Device pairing (RFC 8628 in shape): start, open the browser, poll until the
//! signed-in user clicks Allow, then keep the credential in the keychain.
//!
//! Server behaviour followed: start can answer
//! 429 `rate_limited` with `Retry-After: 60` (per-source cap) or 503 `busy`;
//! both are waited out. Poll answers `invalid_grant` when the pending
//! pairing was evicted during a flood; pairing then starts over once.

use std::time::Duration;

use crate::api::{ApiClient, ApiError, Credential, PairStart, PollOutcome, SLOW_DOWN_STEP_S};
use crate::credentials::CredentialStore;

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("the sign-in code expired before it was approved")]
    Expired,
    #[error("sign-in was denied in the browser")]
    Denied,
    #[error("the sign-in code is no longer valid; sign in again")]
    Invalid,
    #[error("too many connected sims on your account; remove one under Connected sims, then sign in again")]
    TooManyDevices,
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error("could not store the credential: {0}")]
    Store(String),
}

/// Timing knobs. Production uses whole seconds; tests shrink the unit.
#[derive(Debug, Clone, Copy)]
pub struct PollTiming {
    /// One "second" of the server's `interval`, `expires_in` and `Retry-After`.
    pub unit: Duration,
    /// Network errors while polling are retried until the code expires.
    pub retry_after_error: Duration,
    /// First wait when start is refused as busy; doubles up to `start_backoff_max`.
    pub start_backoff_min: Duration,
    pub start_backoff_max: Duration,
    /// Stop retrying start after this long.
    pub start_give_up: Duration,
}

impl Default for PollTiming {
    fn default() -> Self {
        PollTiming {
            unit: Duration::from_secs(1),
            retry_after_error: Duration::from_secs(5),
            start_backoff_min: Duration::from_secs(5),
            start_backoff_max: Duration::from_secs(60),
            start_give_up: Duration::from_secs(600),
        }
    }
}

impl PollTiming {
    /// A server duration (Retry-After) in this timing's unit.
    fn scaled(&self, d: Duration) -> Duration {
        self.unit.mul_f64(d.as_secs_f64())
    }
}

/// What the tray shows while pairing.
#[derive(Debug)]
pub enum PairEvent<'a> {
    /// A code to approve (again after a restart).
    Code { start: &'a PairStart, url: &'a str },
    /// SkyNexus asked to wait before the next start.
    Waiting { reason: String, wait: Duration },
}

fn transient(e: &ApiError) -> bool {
    matches!(
        e,
        ApiError::Network(_) | ApiError::RateLimited { .. } | ApiError::Unavailable { .. }
    )
}

/// `pair/start`, waiting out 429 and 503 `busy` (at least `Retry-After`).
async fn start_with_retry(
    api: &ApiClient,
    device_name: &str,
    timing: PollTiming,
    on_event: &mut impl FnMut(PairEvent<'_>),
) -> Result<PairStart, PairingError> {
    let give_up = tokio::time::Instant::now() + timing.start_give_up;
    let mut backoff = timing.start_backoff_min;
    loop {
        match api.pair_start(device_name).await {
            Ok(s) => return Ok(s),
            Err(e) if transient(&e) => {
                let wait = backoff.max(e.retry_after().map(|r| timing.scaled(r)).unwrap_or_default());
                if tokio::time::Instant::now() + wait > give_up {
                    return Err(e.into());
                }
                log::info!("pairing: start refused ({e}); retrying in {} s", wait.as_secs());
                on_event(PairEvent::Waiting {
                    reason: e.to_string(),
                    wait,
                });
                tokio::time::sleep(wait).await;
                backoff = (backoff * 2).min(timing.start_backoff_max);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Polls until the pairing is approved, denied or expired, honouring the
/// server interval and `slow_down` (RFC 8628 section 3.5).
pub async fn wait_for_approval(
    api: &ApiClient,
    start: &PairStart,
    timing: PollTiming,
) -> Result<Credential, PairingError> {
    let deadline = tokio::time::Instant::now() + timing.unit * u32::try_from(start.expires_in).unwrap_or(u32::MAX);
    let mut interval_s = start.interval_s();
    loop {
        let wait = timing.unit * u32::try_from(interval_s).unwrap_or(u32::MAX);
        if tokio::time::Instant::now() + wait > deadline {
            return Err(PairingError::Expired);
        }
        tokio::time::sleep(wait).await;
        match api.pair_poll(&start.device_code).await {
            Ok(PollOutcome::Pending) => {}
            Ok(PollOutcome::SlowDown) => interval_s += SLOW_DOWN_STEP_S,
            Ok(PollOutcome::Expired) => return Err(PairingError::Expired),
            Ok(PollOutcome::Denied) => return Err(PairingError::Denied),
            Ok(PollOutcome::Invalid) => return Err(PairingError::Invalid),
            Ok(PollOutcome::TooManyDevices) => return Err(PairingError::TooManyDevices),
            Ok(PollOutcome::Approved(c)) => return Ok(c),
            Err(e) if transient(&e) => {
                log::warn!("pairing poll failed, retrying: {e}");
                let extra = e.retry_after().map(|r| timing.scaled(r)).unwrap_or_default();
                tokio::time::sleep(timing.retry_after_error.max(extra)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// The whole flow. `on_event` gets each code (the tray shows it, so pairing
/// also works when the browser is on another device) and each wait.
pub async fn pair(
    api: &ApiClient,
    store: &dyn CredentialStore,
    device_name: &str,
    open_browser: bool,
    timing: PollTiming,
    mut on_event: impl FnMut(PairEvent<'_>),
) -> Result<Credential, PairingError> {
    let mut restarted = false;
    loop {
        let start = start_with_retry(api, device_name, timing, &mut on_event).await?;
        let url = start.browser_url(api.base());
        on_event(PairEvent::Code {
            start: &start,
            url: &url,
        });
        if open_browser {
            if let Err(e) = open::that_detached(&url) {
                log::warn!("could not open the browser ({e}); open {url} by hand");
            }
        }
        match wait_for_approval(api, &start, timing).await {
            Ok(credential) => {
                store.save(&credential).map_err(PairingError::Store)?;
                return Ok(credential);
            }
            Err(PairingError::Invalid) if !restarted => {
                // The server evicted the pending pairing (a flood); start over once.
                log::info!("pairing: the code was dropped by SkyNexus; starting again");
                restarted = true;
            }
            Err(e) => return Err(e),
        }
    }
}

/// A readable device name for the Allow screen and the account settings list.
pub fn device_name() -> String {
    let host = gethostname::gethostname().to_string_lossy().trim().to_string();
    // "joeri-pc.home" or "Mac.local" reads better as the bare host name.
    let host = host.split('.').next().unwrap_or_default().to_string();
    if host.is_empty() {
        format!("{} computer", std::env::consts::OS)
    } else {
        host
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryStore;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fast() -> PollTiming {
        PollTiming {
            unit: Duration::from_millis(5),
            retry_after_error: Duration::from_millis(5),
            start_backoff_min: Duration::from_millis(5),
            start_backoff_max: Duration::from_millis(40),
            start_give_up: Duration::from_secs(5),
        }
    }

    fn start_body(code: &str) -> serde_json::Value {
        serde_json::json!({
            "device_code": format!("dev-{code}"),
            "user_code": code,
            "verification_uri": "/connect",
            "expires_in": 600,
            "interval": 1,
        })
    }

    async fn server_with_start(interval: u64, expires_in: u64) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/start"))
            .and(body_json(serde_json::json!({ "device_name": "TEST-PC" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code": "dev-code-1",
                "user_code": "ABCD-EFGH",
                "verification_uri": "/connect",
                "expires_in": expires_in,
                "interval": interval,
            })))
            .mount(&server)
            .await;
        server
    }

    fn pending() -> ResponseTemplate {
        ResponseTemplate::new(400).set_body_json(serde_json::json!({ "error": "authorization_pending" }))
    }

    async fn count(server: &MockServer, p: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == p)
            .count()
    }

    #[tokio::test]
    async fn pending_then_approved_stores_the_credential() {
        let server = server_with_start(1, 600).await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/poll"))
            .and(body_json(serde_json::json!({ "device_code": "dev-code-1" })))
            .respond_with(pending())
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/poll"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "credential": "cred-xyz",
                "device": { "id": "dev-9", "name": "TEST-PC" }
            })))
            .with_priority(2)
            .mount(&server)
            .await;

        let api = ApiClient::new(&server.uri());
        let store = MemoryStore::default();
        let mut seen = None;
        let c = pair(&api, &store, "TEST-PC", false, fast(), |e| {
            if let PairEvent::Code { start, url } = e {
                seen = Some((start.user_code.clone(), url.to_string()));
            }
        })
        .await
        .unwrap();
        assert_eq!(c.credential, "cred-xyz");
        assert_eq!(store.load().unwrap().unwrap().device_id.as_deref(), Some("dev-9"));
        let (code, url) = seen.unwrap();
        assert_eq!(code, "ABCD-EFGH");
        assert_eq!(url, format!("{}/connect?code=ABCD-EFGH", server.uri()));
        assert_eq!(count(&server, "/api/connector/pair/poll").await, 3);
        assert_eq!(count(&server, "/api/connector/pair/start").await, 1);
    }

    #[tokio::test]
    async fn denied_and_expired_answers_end_the_flow() {
        for (error, check) in [
            (
                "access_denied",
                (|e: &PairingError| matches!(e, PairingError::Denied)) as fn(&PairingError) -> bool,
            ),
            ("expired_token", |e: &PairingError| matches!(e, PairingError::Expired)),
        ] {
            let server = server_with_start(1, 600).await;
            Mock::given(method("POST"))
                .and(path("/api/connector/pair/poll"))
                .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({ "error": error })))
                .mount(&server)
                .await;
            let api = ApiClient::new(&server.uri());
            let store = MemoryStore::default();
            let err = pair(&api, &store, "TEST-PC", false, fast(), |_| {}).await.unwrap_err();
            assert!(check(&err), "{error}: {err:?}");
            assert!(store.load().unwrap().is_none());
            assert_eq!(count(&server, "/api/connector/pair/start").await, 1);
        }
    }

    #[tokio::test]
    async fn invalid_grant_starts_over_once_with_a_new_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(start_body("BCDF-GHJK")))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(start_body("LMNP-QRST")))
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/poll"))
            .and(body_json(serde_json::json!({ "device_code": "dev-BCDF-GHJK" })))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({ "error": "invalid_grant" })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/poll"))
            .and(body_json(serde_json::json!({ "device_code": "dev-LMNP-QRST" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "credential": "snc_2" })))
            .mount(&server)
            .await;
        let api = ApiClient::new(&server.uri());
        let mut codes = Vec::new();
        let c = pair(&api, &MemoryStore::default(), "TEST-PC", false, fast(), |e| {
            if let PairEvent::Code { start, .. } = e {
                codes.push(start.user_code.clone());
            }
        })
        .await
        .unwrap();
        assert_eq!(c.credential, "snc_2");
        assert_eq!(codes, vec!["BCDF-GHJK", "LMNP-QRST"], "the tray shows the new code");
    }

    #[tokio::test]
    async fn a_second_invalid_grant_gives_up() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(start_body("BCDF-GHJK")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/poll"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({ "error": "invalid_grant" })))
            .mount(&server)
            .await;
        let api = ApiClient::new(&server.uri());
        let err = pair(&api, &MemoryStore::default(), "TEST-PC", false, fast(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(err, PairingError::Invalid), "{err:?}");
        assert_eq!(
            count(&server, "/api/connector/pair/start").await,
            2,
            "exactly one restart"
        );
    }

    #[tokio::test]
    async fn start_rate_limit_and_busy_are_waited_out() {
        for (status, body) in [(429, r#"{"error":"rate_limited"}"#), (503, r#"{"error":"busy"}"#)] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/connector/pair/start"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("retry-after", "60")
                        .set_body_raw(body, "application/json"),
                )
                .up_to_n_times(1)
                .with_priority(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/connector/pair/start"))
                .respond_with(ResponseTemplate::new(200).set_body_json(start_body("BCDF-GHJK")))
                .with_priority(2)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/connector/pair/poll"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "credential": "snc_x" })))
                .mount(&server)
                .await;
            let api = ApiClient::new(&server.uri());
            let mut waits = Vec::new();
            let c = pair(&api, &MemoryStore::default(), "TEST-PC", false, fast(), |e| {
                if let PairEvent::Waiting { wait, .. } = e {
                    waits.push(wait);
                }
            })
            .await
            .unwrap();
            assert_eq!(c.credential, "snc_x");
            // Retry-After 60 at a 5 ms test unit is 300 ms, above the 5 ms backoff.
            assert_eq!(waits, vec![Duration::from_millis(300)], "HTTP {status}");
        }
    }

    #[tokio::test]
    async fn slow_down_widens_the_interval_and_local_expiry_stops_polling() {
        // interval 1, expires_in 12: polls at 1 and 7 (slow_down adds 5 each time);
        // the next wait of 11 would pass the expiry, so it stops without a third poll
        let server = server_with_start(1, 12).await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/poll"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({ "error": "slow_down" })))
            .mount(&server)
            .await;
        let api = ApiClient::new(&server.uri());
        let store = MemoryStore::default();
        let err = pair(&api, &store, "TEST-PC", false, fast(), |_| {}).await.unwrap_err();
        assert!(matches!(err, PairingError::Expired), "{err:?}");
        assert_eq!(count(&server, "/api/connector/pair/poll").await, 2);
    }

    #[tokio::test]
    async fn a_server_error_while_polling_is_retried() {
        let server = server_with_start(1, 600).await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/poll"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({ "error": "unavailable" })))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/poll"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "credential": "snc_x" })))
            .with_priority(2)
            .mount(&server)
            .await;
        let api = ApiClient::new(&server.uri());
        let c = pair(&api, &MemoryStore::default(), "TEST-PC", false, fast(), |_| {})
            .await
            .unwrap();
        assert_eq!(c.credential, "snc_x");
        assert_eq!(c.device_id, None);
    }

    #[tokio::test]
    async fn feature_dark_server_is_reported_at_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/pair/start"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "3600")
                    .set_body_json(serde_json::json!({ "error": "not_configured" })),
            )
            .mount(&server)
            .await;
        let api = ApiClient::new(&server.uri());
        let err = pair(&api, &MemoryStore::default(), "X", false, fast(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(err, PairingError::Api(ApiError::NotConfigured { .. })));
        assert_eq!(count(&server, "/api/connector/pair/start").await, 1);
    }

    #[test]
    fn device_name_is_never_empty() {
        assert!(!device_name().is_empty());
    }
}
