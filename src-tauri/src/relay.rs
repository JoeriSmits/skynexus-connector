//! The uplink: a WebSocket client to `wss://<relay>/v1/connector/ws`, with
//! exponential backoff, ticket refresh before expiry, and keepalive.
//! Outbound only; the connector never listens on a port.
//!
//! Protocol:
//! 1. open the socket and send `{"type":"auth","ticket":"<JWT>"}` first, within 5 s;
//! 2. wait for `{"type":"ready","role":"connector","exp":...}`;
//! 3. send bare frames as text, at most 5 per second and 16 KB each;
//! 4. before the ticket expires, fetch a new one and send it as another auth
//!    message on the same socket; the relay answers `ready` again and the
//!    stream goes on. No `ready` within the auth timeout (a relay without
//!    renewal) falls back to reconnecting with the new ticket.
//!
//! The relay refuses `?ticket=` in the URL with HTTP 400, so it never goes there.
//!
//! Close codes: 4001 ticket expired (refresh, reconnect at once), 4002 auth
//! protocol error, 4003 ticket refused (fetch a new one; also an `iat` over
//! 60 s ahead, so check the clock), 4029 too many connectors for this user
//! (wait 30 s or more), 1009 frame too big, 1001 relay shutting down.
//! Upgrade answers 429 (too many pending sockets from this address) and 503
//! (relay busy) back off from 5 s to 60 s.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::watch;
use tokio::time::{sleep, sleep_until, timeout, Instant};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::api::ApiError;
use crate::config::{relay_socket_url, Config};
use crate::ticket::TicketManager;

/// The latest frame as JSON, shared by the sim driver and the relay.
pub type FrameSlot = Option<Arc<str>>;

pub const CLOSE_TICKET_EXPIRED: u16 = 4001;
pub const CLOSE_AUTH_PROTOCOL: u16 = 4002;
pub const CLOSE_TICKET_REFUSED: u16 = 4003;
pub const CLOSE_TOO_MANY: u16 = 4029;

/// The longest `Retry-After` honoured from the ticket route (its 503
/// `not_configured` says 3600).
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, PartialEq)]
pub enum RelayState {
    Idle,
    Connecting,
    Connected,
    /// Waiting before the next attempt.
    Retrying {
        reason: String,
        in_s: u64,
    },
    /// The account lacks the `connector` flag (ticket 403); retried slowly.
    NotEnabled,
    /// The relay URL does not parse.
    NotConfigured,
}

#[derive(Debug, PartialEq)]
pub enum RelayExit {
    /// The credential was refused (401): the device was revoked.
    Unauthorized,
}

#[derive(Debug, Clone, Copy)]
pub struct RelayTiming {
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub connect_timeout: Duration,
    /// How long to wait for `ready` after the auth message.
    pub auth_timeout: Duration,
    pub ping_every: Duration,
    /// Close and reconnect when nothing (not even a pong) arrived for this long.
    pub dead_after: Duration,
    /// A session that lasted this long resets the backoff.
    pub stable_after: Duration,
    /// After close 4029 (too many connectors for this user).
    pub busy_wait: Duration,
    /// After ticket 403 `not_enabled`.
    pub not_enabled_wait: Duration,
    /// First wait after an upgrade answered 429 or 503; doubles up to `backoff_max`.
    pub busy_upgrade_min: Duration,
}

impl Default for RelayTiming {
    fn default() -> Self {
        RelayTiming {
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(10),
            auth_timeout: Duration::from_secs(5),
            ping_every: Duration::from_secs(20),
            dead_after: Duration::from_secs(50),
            stable_after: Duration::from_secs(30),
            busy_wait: Duration::from_secs(45),
            not_enabled_wait: Duration::from_secs(300),
            busy_upgrade_min: Duration::from_secs(5),
        }
    }
}

/// Exponential backoff with up to 25% jitter.
#[derive(Debug)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        Backoff {
            min,
            max: max.max(min),
            next: min,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let base = self.next;
        self.next = (self.next * 2).min(self.max);
        let jitter = base.mul_f64(rand::random::<f64>() * 0.25);
        (base + jitter).min(self.max)
    }

    pub fn reset(&mut self) {
        self.next = self.min;
    }
}

#[derive(Debug, PartialEq)]
enum SessionEnd {
    /// Our own timer: the ticket is about to expire, and renewing it on the
    /// open socket was not possible (no new ticket).
    TicketRefresh,
    /// A new ticket was sent on the open socket but the relay never confirmed
    /// it (a relay without renewal): reconnect with that ticket.
    RenewalUnconfirmed,
    /// 4001: the relay says the ticket expired.
    TicketExpired,
    /// 4003, or HTTP 401/403 on the upgrade.
    TicketRefused,
    /// 4029: two connectors are already streaming for this user.
    TooMany,
    Lost(String),
}

fn classify_close(frame: Option<&CloseFrame>) -> SessionEnd {
    match frame.map(|f| u16::from(f.code)) {
        Some(CLOSE_TICKET_EXPIRED) => SessionEnd::TicketExpired,
        Some(CLOSE_TICKET_REFUSED) => SessionEnd::TicketRefused,
        Some(CLOSE_TOO_MANY) => SessionEnd::TooMany,
        Some(CLOSE_AUTH_PROTOCOL) => SessionEnd::Lost("relay refused the auth message".into()),
        Some(1009) => SessionEnd::Lost("relay refused a frame as too big".into()),
        Some(1001) => SessionEnd::Lost("relay is restarting".into()),
        Some(code) => SessionEnd::Lost(format!(
            "relay closed ({code}{})",
            frame.map(|f| format!(" {}", f.reason)).unwrap_or_default()
        )),
        None => SessionEnd::Lost("relay closed the connection".into()),
    }
}

pub fn auth_message(ticket: &str) -> String {
    serde_json::json!({ "type": "auth", "ticket": ticket }).to_string()
}

fn is_ready(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .is_some_and(|v| v.get("type").and_then(Value::as_str) == Some("ready"))
}

pub struct Relay {
    pub cfg: Arc<Config>,
    pub tickets: TicketManager,
    pub frames: watch::Receiver<FrameSlot>,
    pub state: Arc<watch::Sender<RelayState>>,
    pub timing: RelayTiming,
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl Relay {
    fn set(&self, s: RelayState) {
        self.state.send_if_modified(|cur| {
            let changed = *cur != s;
            *cur = s;
            changed
        });
    }

    async fn wait_for(&self, d: Duration, reason: String) {
        log::info!("relay: {reason}; retrying in {} s", d.as_secs_f32().round());
        self.set(RelayState::Retrying {
            reason,
            in_s: d.as_secs_f32().round() as u64,
        });
        sleep(d).await;
    }

    async fn wait(&self, backoff: &mut Backoff, reason: String) {
        let d = backoff.next_delay();
        self.wait_for(d, reason).await;
    }

    /// Runs until the credential is refused. Abort the task to stop it.
    pub async fn run(mut self) -> RelayExit {
        let mut backoff = Backoff::new(self.timing.backoff_min, self.timing.backoff_max);
        let mut busy = Backoff::new(self.timing.busy_upgrade_min, self.timing.backoff_max);
        loop {
            self.set(RelayState::Connecting);
            let ticket = match self.tickets.valid(std::time::Instant::now()).await {
                Ok(t) => t,
                Err(ApiError::Unauthorized) => return RelayExit::Unauthorized,
                Err(ApiError::NotEnabled) => {
                    log::info!("relay: the connector is not enabled for this account; asking again later");
                    self.set(RelayState::NotEnabled);
                    sleep(self.timing.not_enabled_wait).await;
                    continue;
                }
                Err(e) => {
                    // Honour Retry-After (429 and 503), within reason.
                    let d = backoff
                        .next_delay()
                        .max(e.retry_after().unwrap_or_default().min(MAX_RETRY_AFTER));
                    self.wait_for(d, format!("no ticket ({e})")).await;
                    continue;
                }
            };
            let base = self.cfg.relay_base(ticket.relay_url.as_deref());
            let url = match relay_socket_url(&base) {
                Ok(u) => u,
                Err(e) => {
                    log::error!("relay: bad relay URL {base}: {e}");
                    self.set(RelayState::NotConfigured);
                    sleep(Duration::from_secs(60)).await;
                    continue;
                }
            };
            let ws = match timeout(
                self.timing.connect_timeout,
                tokio_tungstenite::connect_async(url.as_str()),
            )
            .await
            {
                Ok(Ok((ws, _))) => ws,
                Ok(Err(tungstenite::Error::Http(res))) => {
                    let code = res.status().as_u16();
                    if matches!(code, 429 | 503) {
                        self.wait(&mut busy, format!("relay is busy (HTTP {code})")).await;
                        continue;
                    }
                    if matches!(code, 401 | 403) {
                        self.tickets.invalidate();
                    }
                    self.wait(&mut backoff, format!("relay answered HTTP {code}")).await;
                    continue;
                }
                Ok(Err(e)) => {
                    self.wait(&mut backoff, format!("cannot reach the relay ({})", short(&e)))
                        .await;
                    continue;
                }
                Err(_) => {
                    self.wait(&mut backoff, "relay connect timed out".into()).await;
                    continue;
                }
            };
            busy.reset();
            let started = Instant::now();
            let refresh_at = Instant::now() + self.tickets.refresh_in(std::time::Instant::now());
            let end = match self.authenticate(ws, &ticket.token).await {
                Ok(ws) => {
                    log::info!("relay: streaming to {base}");
                    self.set(RelayState::Connected);
                    self.session(ws, refresh_at).await
                }
                Err(end) => end,
            };
            if started.elapsed() >= self.timing.stable_after {
                backoff.reset();
            }
            match end {
                SessionEnd::RenewalUnconfirmed => {
                    log::info!("relay: the relay did not confirm the renewed ticket; reconnecting with it");
                }
                SessionEnd::TicketRefresh | SessionEnd::TicketExpired => {
                    log::info!("relay: refreshing the ticket");
                    self.tickets.invalidate();
                    // A server handing out near-expired tickets must not cause a hot loop.
                    if started.elapsed() < Duration::from_secs(5) {
                        self.wait(&mut backoff, "ticket expired almost at once".into()).await;
                    }
                }
                SessionEnd::TicketRefused => {
                    self.tickets.invalidate();
                    self.wait(&mut backoff, "ticket refused; fetching a new one".into())
                        .await;
                }
                SessionEnd::TooMany => {
                    let d = self.timing.busy_wait.max(backoff.next_delay());
                    self.wait_for(d, "two other connectors already stream for this account".into())
                        .await;
                }
                SessionEnd::Lost(reason) => self.wait(&mut backoff, reason).await,
            }
        }
    }

    /// Sends the auth message and waits for `ready`.
    async fn authenticate(&self, mut ws: Ws, ticket: &str) -> Result<Ws, SessionEnd> {
        if let Err(e) = ws.send(Message::text(auth_message(ticket))).await {
            return Err(SessionEnd::Lost(format!("auth send failed ({})", short(&e))));
        }
        let deadline = Instant::now() + self.timing.auth_timeout;
        loop {
            let msg = match timeout(deadline.saturating_duration_since(Instant::now()), ws.next()).await {
                Err(_) => return Err(SessionEnd::Lost("relay never confirmed the ticket".into())),
                Ok(m) => m,
            };
            match msg {
                None => return Err(SessionEnd::Lost("relay closed during auth".into())),
                Some(Err(e)) => return Err(SessionEnd::Lost(format!("relay error during auth ({})", short(&e)))),
                Some(Ok(Message::Close(frame))) => return Err(classify_close(frame.as_ref())),
                Some(Ok(Message::Text(t))) if is_ready(t.as_str()) => return Ok(ws),
                Some(Ok(_)) => {}
            }
        }
    }

    async fn session(&mut self, ws: Ws, mut refresh_at: Instant) -> SessionEnd {
        let (mut tx, mut rx) = ws.split();
        let mut ping = tokio::time::interval(self.timing.ping_every);
        ping.tick().await;
        let mut last_heard = Instant::now();
        // Set while a renewed ticket waits for the relay's `ready`.
        let mut renewal_due: Option<Instant> = None;
        // Send the newest frame at once if there is one, then every new one.
        let current = self.frames.borrow_and_update().clone();
        if let Some(json) = current {
            if let Err(e) = tx.send(Message::text(json.to_string())).await {
                return SessionEnd::Lost(format!("send failed ({})", short(&e)));
            }
        }
        loop {
            tokio::select! {
                changed = self.frames.changed() => {
                    if changed.is_err() {
                        let _ = tx.close().await;
                        return SessionEnd::Lost("frame source stopped".into());
                    }
                    let frame = self.frames.borrow_and_update().clone();
                    if let Some(json) = frame {
                        if let Err(e) = tx.send(Message::text(json.to_string())).await {
                            return SessionEnd::Lost(format!("send failed ({})", short(&e)));
                        }
                    }
                }
                msg = rx.next() => {
                    last_heard = Instant::now();
                    match msg {
                        None => return SessionEnd::Lost("relay closed the connection".into()),
                        Some(Err(e)) => return SessionEnd::Lost(format!("relay connection error ({})", short(&e))),
                        Some(Ok(Message::Close(frame))) => return classify_close(frame.as_ref()),
                        Some(Ok(Message::Text(t))) if renewal_due.is_some() && is_ready(t.as_str()) => {
                            renewal_due = None;
                            refresh_at = Instant::now() + self.tickets.refresh_in(std::time::Instant::now());
                            log::info!("relay: ticket renewed on the open connection");
                        }
                        Some(Ok(Message::Text(t))) => log::debug!("relay says: {}", t.as_str()),
                        Some(Ok(_)) => {}
                    }
                }
                _ = ping.tick() => {
                    if last_heard.elapsed() > self.timing.dead_after {
                        return SessionEnd::Lost("relay stopped answering".into());
                    }
                    if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                        return SessionEnd::Lost("ping failed".into());
                    }
                }
                _ = sleep_until(refresh_at), if renewal_due.is_none() => {
                    self.tickets.invalidate();
                    let renewed = match self.tickets.valid(std::time::Instant::now()).await {
                        Ok(t) => tx.send(Message::text(auth_message(&t.token))).await.map_err(|e| short(&e)),
                        Err(e) => Err(e.to_string()),
                    };
                    match renewed {
                        Ok(()) => {
                            log::debug!("relay: renewing the ticket on the open connection");
                            renewal_due = Some(Instant::now() + self.timing.auth_timeout);
                        }
                        Err(e) => {
                            log::info!("relay: cannot renew the ticket on the open connection ({e})");
                            let _ = tx
                                .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "ticket refresh".into() })))
                                .await;
                            return SessionEnd::TicketRefresh;
                        }
                    }
                }
                _ = sleep_until(renewal_due.unwrap_or(refresh_at)), if renewal_due.is_some() => {
                    let _ = tx
                        .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "ticket refresh".into() })))
                        .await;
                    return SessionEnd::RenewalUnconfirmed;
                }
            }
        }
    }
}

fn short(e: &dyn std::fmt::Display) -> String {
    let s = e.to_string();
    if s.chars().count() > 120 {
        format!("{}...", s.chars().take(117).collect::<String>())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiClient;
    use crate::config::Cli;
    use crate::frame::now_ms;
    use base64::Engine as _;
    use clap::Parser;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn jwt(exp_s: u64, n: u32) -> String {
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("h.{}.s", enc.encode(format!(r#"{{"exp":{exp_s},"n":{n}}}"#)))
    }

    #[test]
    fn backoff_grows_and_resets() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(8));
        let d: Vec<u64> = (0..6).map(|_| b.next_delay().as_millis() as u64).collect();
        assert!((1000..=1250).contains(&d[0]));
        assert!((2000..=2500).contains(&d[1]));
        assert!((4000..=5000).contains(&d[2]));
        assert_eq!(d[4], 8000);
        b.reset();
        assert!(b.next_delay() <= Duration::from_millis(1250));
    }

    #[test]
    fn close_codes() {
        let f = |code: u16| CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        };
        assert_eq!(classify_close(Some(&f(4001))), SessionEnd::TicketExpired);
        assert_eq!(classify_close(Some(&f(4003))), SessionEnd::TicketRefused);
        assert_eq!(classify_close(Some(&f(4029))), SessionEnd::TooMany);
        assert!(matches!(classify_close(Some(&f(4002))), SessionEnd::Lost(_)));
        assert_eq!(
            classify_close(Some(&f(1009))),
            SessionEnd::Lost("relay refused a frame as too big".into())
        );
        assert!(matches!(classify_close(None), SessionEnd::Lost(_)));
        assert!(is_ready(r#"{"type":"ready","role":"connector","exp":1}"#));
        assert!(!is_ready(r#"{"type":"status"}"#));
        let v: Value = serde_json::from_str(&auth_message("a.b.c")).unwrap();
        assert_eq!(
            (v["type"].as_str(), v["ticket"].as_str()),
            (Some("auth"), Some("a.b.c"))
        );
    }

    /// How the fake relay treats one connection, chosen by the ticket it gets.
    #[derive(Clone)]
    enum Verdict {
        Ready,
        Close(u16),
    }

    /// A tiny relay with the real handshake: the first message must be the
    /// auth message; then it acks `ready` and records text frames per ticket.
    /// A later auth message is a renewal: with `renews` it is acked with
    /// `ready` and later frames are recorded under the new ticket; without
    /// (an older relay) it is recorded like any other text. The counter is
    /// the number of connections accepted.
    async fn fake_relay(
        verdict: impl Fn(&str) -> Verdict + Send + Sync + 'static,
    ) -> (String, mpsc::UnboundedReceiver<(String, String)>) {
        let (url, rx, _) = fake_relay_with(verdict, false).await;
        (url, rx)
    }

    #[allow(clippy::result_large_err)] // the handshake callback type is fixed by tungstenite
    async fn fake_relay_with(
        verdict: impl Fn(&str) -> Verdict + Send + Sync + 'static,
        renews: bool,
    ) -> (
        String,
        mpsc::UnboundedReceiver<(String, String)>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let verdict = Arc::new(verdict);
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = connections.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let tx = tx.clone();
                let verdict = verdict.clone();
                tokio::spawn(async move {
                    let cb = |req: &Request, res: Response| {
                        assert_eq!(req.uri().path(), "/v1/connector/ws");
                        assert_eq!(req.uri().query(), None, "the ticket never goes in the URL");
                        Ok(res)
                    };
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await else {
                        return;
                    };
                    let Some(Ok(Message::Text(first))) = ws.next().await else {
                        return;
                    };
                    let v: Value = serde_json::from_str(first.as_str()).unwrap();
                    assert_eq!(v["type"], "auth");
                    let mut ticket = v["ticket"].as_str().unwrap().to_string();
                    match verdict(&ticket) {
                        Verdict::Close(code) => {
                            let _ = ws
                                .send(Message::Close(Some(CloseFrame {
                                    code: CloseCode::from(code),
                                    reason: "test".into(),
                                })))
                                .await;
                            return;
                        }
                        Verdict::Ready => {
                            ws.send(Message::text(r#"{"type":"ready","role":"connector","exp":1}"#))
                                .await
                                .unwrap();
                        }
                    }
                    while let Some(Ok(m)) = ws.next().await {
                        if let Message::Text(t) = m {
                            let renewal = serde_json::from_str::<Value>(t.as_str())
                                .ok()
                                .filter(|v| v["type"] == "auth")
                                .and_then(|v| v["ticket"].as_str().map(str::to_string));
                            match renewal {
                                Some(next) if renews => {
                                    ticket = next;
                                    let _ = ws
                                        .send(Message::text(r#"{"type":"ready","role":"connector","exp":2}"#))
                                        .await;
                                }
                                _ => {
                                    let _ = tx.send((ticket.clone(), t.to_string()));
                                }
                            }
                        }
                    }
                });
            }
        });
        (format!("ws://{addr}"), rx, connections)
    }

    fn relay_for(
        api_base: &str,
        relay_base: &str,
        frames: watch::Receiver<FrameSlot>,
    ) -> (Relay, watch::Receiver<RelayState>) {
        let cfg = Config::from_cli(Cli::parse_from([
            "x",
            "--base-url",
            api_base,
            "--relay-url",
            relay_base,
        ]));
        let (state_tx, state_rx) = watch::channel(RelayState::Idle);
        let relay = Relay {
            cfg: Arc::new(cfg),
            tickets: TicketManager::new(ApiClient::new(api_base), "cred".into()),
            frames,
            state: Arc::new(state_tx),
            timing: RelayTiming {
                backoff_min: Duration::from_millis(20),
                backoff_max: Duration::from_millis(100),
                auth_timeout: Duration::from_millis(500),
                busy_wait: Duration::from_millis(50),
                not_enabled_wait: Duration::from_millis(50),
                ..RelayTiming::default()
            },
        };
        (relay, state_rx)
    }

    /// Ticket server: the first answer is ticket `n = 1`, every later one `n = 2`.
    /// `expires_in` follows from the exp, so the lifetimes are what they say.
    async fn tickets(first_exp: u64, later_exp: u64) -> MockServer {
        let life = |exp: u64| exp.saturating_sub(now_ms() / 1000).max(1);
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/ticket"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ticket": jwt(first_exp, 1), "expires_in": life(first_exp) })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/connector/ticket"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ticket": jwt(later_exp, 2), "expires_in": life(later_exp) })),
            )
            .with_priority(2)
            .mount(&api)
            .await;
        api
    }

    async fn feed_until<F: Fn(&(String, String)) -> bool>(
        frames: &watch::Sender<FrameSlot>,
        rx: &mut mpsc::UnboundedReceiver<(String, String)>,
        done: F,
    ) -> (String, String) {
        for i in 0..400 {
            frames.send_replace(Some(Arc::from(format!(r#"{{"n":{i}}}"#))));
            if let Ok(Some(got)) = timeout(Duration::from_millis(25), rx.recv()).await {
                if done(&got) {
                    return got;
                }
                // Not the one yet: pace the feed so ticket timers can run.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        panic!("relay never delivered");
    }

    fn in_15_min() -> u64 {
        now_ms() / 1000 + 900
    }

    #[tokio::test]
    async fn authenticates_with_a_first_message_then_streams() {
        let exp = in_15_min();
        let api = tickets(exp, exp).await;
        let (relay_url, mut got) = fake_relay(|_| Verdict::Ready).await;
        let (ftx, frx) = watch::channel(None);
        let (relay, mut state) = relay_for(&api.uri(), &relay_url, frx);
        let task = tokio::spawn(relay.run());
        let (ticket, body) = feed_until(&ftx, &mut got, |_| true).await;
        assert_eq!(ticket, jwt(exp, 1));
        assert!(body.starts_with("{\"n\":"));
        state.wait_for(|s| *s == RelayState::Connected).await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn a_refused_ticket_is_replaced() {
        let exp = in_15_min();
        let api = tickets(exp, exp).await;
        let refused = jwt(exp, 1);
        let (relay_url, mut got) = fake_relay(move |t| {
            if t == refused {
                Verdict::Close(CLOSE_TICKET_REFUSED)
            } else {
                Verdict::Ready
            }
        })
        .await;
        let (ftx, frx) = watch::channel(None);
        let (relay, _state) = relay_for(&api.uri(), &relay_url, frx);
        let task = tokio::spawn(relay.run());
        let (ticket, _) = feed_until(&ftx, &mut got, |_| true).await;
        assert_eq!(ticket, jwt(exp, 2));
        task.abort();
    }

    #[tokio::test]
    async fn close_4001_fetches_a_fresh_ticket() {
        let exp = in_15_min();
        let api = tickets(exp, exp).await;
        let stale = jwt(exp, 1);
        let (relay_url, mut got) = fake_relay(move |t| {
            if t == stale {
                Verdict::Close(CLOSE_TICKET_EXPIRED)
            } else {
                Verdict::Ready
            }
        })
        .await;
        let (ftx, frx) = watch::channel(None);
        let (relay, _state) = relay_for(&api.uri(), &relay_url, frx);
        let task = tokio::spawn(relay.run());
        let (ticket, _) = feed_until(&ftx, &mut got, |_| true).await;
        assert_eq!(ticket, jwt(exp, 2));
        task.abort();
    }

    #[tokio::test]
    async fn renews_the_ticket_on_the_open_connection() {
        // The first ticket lives 1 s, so the renewal is due after half of
        // that; a relay that renews keeps the one connection.
        let soon = now_ms() / 1000 + 1;
        let later = in_15_min();
        let api = tickets(soon, later).await;
        let (relay_url, mut got, connections) = fake_relay_with(|_| Verdict::Ready, true).await;
        let (ftx, frx) = watch::channel(None);
        let (relay, mut state) = relay_for(&api.uri(), &relay_url, frx);
        let task = tokio::spawn(relay.run());
        let (ticket, body) = feed_until(&ftx, &mut got, |(t, _)| *t == jwt(later, 2)).await;
        assert_eq!(ticket, jwt(later, 2));
        assert!(body.starts_with("{\"n\":"), "frames, not the auth message");
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1, "no reconnect");
        assert_eq!(*state.borrow_and_update(), RelayState::Connected);
        task.abort();
    }

    #[tokio::test]
    async fn an_older_relay_gets_a_reconnect_with_the_new_ticket() {
        // Same tickets, but the relay never confirms a renewal: after the
        // auth timeout the connector reconnects with the ticket it fetched.
        let soon = now_ms() / 1000 + 1;
        let later = in_15_min();
        let api = tickets(soon, later).await;
        let (relay_url, mut got, connections) = fake_relay_with(|_| Verdict::Ready, false).await;
        let (ftx, frx) = watch::channel(None);
        let (relay, _state) = relay_for(&api.uri(), &relay_url, frx);
        let task = tokio::spawn(relay.run());
        let (ticket, _) = feed_until(&ftx, &mut got, |(t, _)| *t == jwt(later, 2)).await;
        assert_eq!(ticket, jwt(later, 2));
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            api.received_requests().await.unwrap().len(),
            2,
            "the renewed ticket is reused, not fetched again"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_fast_local_clock_streams_without_reconnecting() {
        // The JWT exp is 20 minutes in the past by this PC's clock, but the
        // server says the ticket lives 900 s: stream on, no ticket churn.
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let server_now = now_ms() / 1000 - 1200;
        let token = format!(
            "h.{}.s",
            enc.encode(format!(r#"{{"iat":{server_now},"exp":{}}}"#, server_now + 900))
        );
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/ticket"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ticket": token, "expires_in": 900 })),
            )
            .mount(&api)
            .await;
        let (relay_url, mut got) = fake_relay(|_| Verdict::Ready).await;
        let (ftx, frx) = watch::channel(None);
        let (relay, mut state) = relay_for(&api.uri(), &relay_url, frx);
        let task = tokio::spawn(relay.run());
        feed_until(&ftx, &mut got, |_| true).await;
        state.wait_for(|s| *s == RelayState::Connected).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(*state.borrow(), RelayState::Connected);
        assert_eq!(
            api.received_requests().await.unwrap().len(),
            1,
            "one ticket, no reconnect loop"
        );
        task.abort();
    }

    #[tokio::test]
    async fn ticket_retry_after_is_honoured() {
        for (status, body) in [
            (429, r#"{"error":"rate_limited"}"#),
            (503, r#"{"error":"unavailable"}"#),
        ] {
            let api = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/connector/ticket"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("retry-after", "7")
                        .set_body_raw(body, "application/json"),
                )
                .mount(&api)
                .await;
            let (_ftx, frx) = watch::channel(None);
            let (relay, mut state) = relay_for(&api.uri(), "ws://127.0.0.1:9", frx);
            let task = tokio::spawn(relay.run());
            let s = timeout(
                Duration::from_secs(5),
                state.wait_for(|s| matches!(s, RelayState::Retrying { .. })),
            )
            .await
            .unwrap()
            .unwrap()
            .clone();
            let RelayState::Retrying { in_s, .. } = s else {
                unreachable!()
            };
            assert!(in_s >= 7, "HTTP {status}: waits Retry-After, got {in_s}");
            task.abort();
        }
    }

    #[tokio::test]
    async fn too_many_connectors_waits_and_says_why() {
        let exp = in_15_min();
        let api = tickets(exp, exp).await;
        let (relay_url, _got) = fake_relay(|_| Verdict::Close(CLOSE_TOO_MANY)).await;
        let (_ftx, frx) = watch::channel(None);
        let (relay, mut state) = relay_for(&api.uri(), &relay_url, frx);
        let task = tokio::spawn(relay.run());
        timeout(
            Duration::from_secs(5),
            state.wait_for(
                |s| matches!(s, RelayState::Retrying { reason, .. } if reason.contains("two other connectors")),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn a_busy_relay_backs_off_from_five_seconds() {
        for status in ["429 Too Many Requests", "503 Service Unavailable"] {
            let exp = in_15_min();
            let api = tickets(exp, exp).await;
            // A relay that answers every upgrade with an HTTP error.
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let answer = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                while let Ok((mut s, _)) = listener.accept().await {
                    let mut buf = [0u8; 2048];
                    let _ = s.read(&mut buf).await;
                    let _ = s.write_all(answer.as_bytes()).await;
                }
            });
            let (_ftx, frx) = watch::channel(None);
            let (mut relay, mut state) = relay_for(&api.uri(), &format!("ws://{addr}"), frx);
            relay.timing.busy_upgrade_min = Duration::from_secs(5);
            relay.timing.backoff_max = Duration::from_secs(60);
            let task = tokio::spawn(relay.run());
            let s = timeout(
                Duration::from_secs(5),
                state.wait_for(|s| matches!(s, RelayState::Retrying { reason, .. } if reason.contains("busy"))),
            )
            .await
            .unwrap()
            .unwrap()
            .clone();
            let RelayState::Retrying { in_s, .. } = s else {
                unreachable!()
            };
            assert!(in_s >= 5, "{status}: waits at least 5 s, got {in_s}");
            task.abort();
        }
    }

    #[tokio::test]
    async fn revoked_credential_stops_the_relay() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/ticket"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({ "error": "revoked" })))
            .mount(&api)
            .await;
        let (_ftx, frx) = watch::channel(None);
        let (relay, _state) = relay_for(&api.uri(), "ws://127.0.0.1:9", frx);
        let exit = timeout(Duration::from_secs(5), relay.run()).await.unwrap();
        assert_eq!(exit, RelayExit::Unauthorized);
    }

    #[tokio::test]
    async fn not_enabled_keeps_the_credential_and_retries() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/connector/ticket"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({ "error": "not_enabled" })))
            .mount(&api)
            .await;
        let (_ftx, frx) = watch::channel(None);
        let (relay, mut state) = relay_for(&api.uri(), "ws://127.0.0.1:9", frx);
        let task = tokio::spawn(relay.run());
        timeout(Duration::from_secs(5), state.wait_for(|s| *s == RelayState::NotEnabled))
            .await
            .unwrap()
            .unwrap();
        // it keeps asking instead of signing out
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!task.is_finished());
        assert!(api.received_requests().await.unwrap().len() >= 2);
        task.abort();
    }
}
