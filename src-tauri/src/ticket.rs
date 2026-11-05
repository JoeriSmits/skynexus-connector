//! Relay tickets: traded from the connector credential, cached, and refreshed
//! ahead of expiry so the relay never has to close a live stream.
//!
//! All timing is monotonic (`Instant`) from the moment a ticket arrived, so a
//! wrong system clock cannot make fresh tickets look stale.

use std::time::{Duration, Instant};

use crate::api::{ApiClient, ApiError, Ticket, SKEW_WARN_S};

/// Refresh this long before the ticket expires (at most half its lifetime).
pub const REFRESH_MARGIN: Duration = Duration::from_secs(90);

pub struct TicketManager {
    api: ApiClient,
    credential: String,
    current: Option<Ticket>,
}

fn refresh_at(t: &Ticket) -> Instant {
    t.expires_at() - REFRESH_MARGIN.min(t.lifetime / 2)
}

impl TicketManager {
    pub fn new(api: ApiClient, credential: String) -> Self {
        TicketManager {
            api,
            credential,
            current: None,
        }
    }

    /// A ticket that is not yet due for refresh, fetching a new one if needed.
    pub async fn valid(&mut self, now: Instant) -> Result<Ticket, ApiError> {
        if let Some(t) = &self.current {
            if now < refresh_at(t) {
                return Ok(t.clone());
            }
        }
        let t = self.api.ticket(&self.credential).await?;
        if let Some(skew) = t.clock_skew_s.filter(|s| s.abs() > SKEW_WARN_S) {
            log::warn!(
                "this computer's clock is {} s {} SkyNexus; tickets still work, but fix the clock",
                skew.abs(),
                if skew > 0 { "ahead of" } else { "behind" }
            );
        }
        self.current = Some(t.clone());
        Ok(t)
    }

    /// How long until the current ticket should be swapped for a fresh one.
    pub fn refresh_in(&self, now: Instant) -> Duration {
        match &self.current {
            Some(t) => refresh_at(t).saturating_duration_since(now),
            None => Duration::ZERO,
        }
    }

    /// Drops the cached ticket, for example after the relay refused it.
    pub fn invalidate(&mut self) {
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn jwt(iat: u64, exp: u64) -> String {
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("h.{}.s", enc.encode(format!(r#"{{"iat":{iat},"exp":{exp}}}"#)))
    }

    async fn server(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/ticket"))
            .and(header("authorization", "Bearer cred-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    async fn ticket_calls(server: &MockServer) -> usize {
        server.received_requests().await.unwrap().len()
    }

    #[tokio::test]
    async fn caches_until_the_margin_then_refreshes() {
        let s = server(serde_json::json!({ "ticket": "opaque", "expires_in": 900 })).await;
        let mut m = TicketManager::new(ApiClient::new(&s.uri()), "cred-1".into());
        let t0 = Instant::now();
        let t = m.valid(t0).await.unwrap();
        assert_eq!(t.lifetime, Duration::from_secs(900));
        let due = m.refresh_in(t0);
        assert!(
            // `received` is a hair after t0
            due <= Duration::from_secs(811) && due > Duration::from_secs(805),
            "{due:?}"
        );

        m.valid(t0 + Duration::from_secs(60)).await.unwrap();
        assert_eq!(ticket_calls(&s).await, 1, "cached");
        m.valid(t0 + Duration::from_secs(811)).await.unwrap();
        assert_eq!(ticket_calls(&s).await, 2, "inside the margin it refreshes");

        m.invalidate();
        assert_eq!(m.refresh_in(t0), Duration::ZERO);
        m.valid(t0).await.unwrap();
        assert_eq!(ticket_calls(&s).await, 3);
    }

    #[tokio::test]
    async fn a_fast_local_clock_does_not_make_fresh_tickets_stale() {
        // A server ticket issued "now" by server time, while this PC's clock is
        // 20 minutes ahead: by the local wall clock its exp is already past.
        let server_now = crate::frame::now_ms() / 1000 - 1200;
        let s = server(serde_json::json!({
            "ticket": jwt(server_now, server_now + 900),
            "expires_in": 900,
        }))
        .await;
        let mut m = TicketManager::new(ApiClient::new(&s.uri()), "cred-1".into());
        let t0 = Instant::now();
        let t = m.valid(t0).await.unwrap();
        assert!((1195..=1205).contains(&t.clock_skew_s.unwrap()), "{:?}", t.clock_skew_s);
        assert!(m.refresh_in(t0) > Duration::from_secs(800), "no reconnect loop");
        m.valid(t0 + Duration::from_secs(300)).await.unwrap();
        assert_eq!(ticket_calls(&s).await, 1);
    }

    #[tokio::test]
    async fn a_short_ticket_refreshes_at_half_its_life() {
        let s = server(serde_json::json!({ "ticket": "opaque", "expires_in": 60 })).await;
        let mut m = TicketManager::new(ApiClient::new(&s.uri()), "cred-1".into());
        let t0 = Instant::now();
        m.valid(t0).await.unwrap();
        let due = m.refresh_in(t0);
        assert!(
            due <= Duration::from_secs(31) && due > Duration::from_secs(25),
            "{due:?}"
        );
    }

    #[tokio::test]
    async fn revoked_credential_is_unauthorized() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/ticket"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({ "error": "revoked" })))
            .mount(&s)
            .await;
        let mut m = TicketManager::new(ApiClient::new(&s.uri()), "old".into());
        assert!(matches!(m.valid(Instant::now()).await, Err(ApiError::Unauthorized)));
    }
}
