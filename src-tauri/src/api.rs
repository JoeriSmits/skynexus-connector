//! Every HTTP shape the connector exchanges with the SkyNexus web app, in one
//! place, as the SkyNexus web app's connector routes answer them:
//!
//! - `POST /api/connector/pair/start` `{device_name}`
//!   -> 200 `{device_code, user_code, verification_uri, verification_uri_complete, expires_in, interval}`;
//!   429 `rate_limited`, 503 `not_configured` | `unavailable`.
//! - `POST /api/connector/pair/poll` `{device_code}`
//!   -> 200 once `{credential: "snc_...", device: {id, name}}`;
//!   400 `{error}` with `authorization_pending`, `slow_down`, `access_denied`,
//!   `expired_token` or `invalid_grant`; 429 `slow_down`; 5xx transient.
//! - `POST /api/connector/ticket` with `Authorization: Bearer snc_...`
//!   -> 200 `{ticket, role: "connector", expires_in: 900, expires_at: ISO}`;
//!   401 `invalid_credential` | `revoked` (signed out); 403 `not_enabled`
//!   (keep the credential, retry later); 429; 503.
//! - `DELETE /api/connector/devices` with the bearer credential: sign-out
//!   self-revoke, idempotent.

use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const PAIR_START_PATH: &str = "/api/connector/pair/start";
pub const PAIR_POLL_PATH: &str = "/api/connector/pair/poll";
pub const TICKET_PATH: &str = "/api/connector/ticket";
pub const DEVICES_PATH: &str = "/api/connector/devices";

/// RFC 8628 section 3.2: the default poll interval when the server sends none.
pub const DEFAULT_POLL_INTERVAL_S: u64 = 5;
/// RFC 8628 section 3.5: add 5 s on every `slow_down`.
pub const SLOW_DOWN_STEP_S: u64 = 5;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("network: {0}")]
    Network(String),
    /// 401: the credential is unknown or revoked. Forget it.
    #[error("the connector credential was refused (revoked?)")]
    Unauthorized,
    /// 403 `not_enabled`: the account lacks the `connector` flag, or a flag
    /// read failed during an outage. Keep the credential and retry later.
    #[error("the connector is not enabled for this account yet")]
    NotEnabled,
    /// 429: back off, at least `Retry-After`.
    #[error("SkyNexus asks to slow down")]
    RateLimited { retry_after: Option<Duration> },
    /// 503 `not_configured`: the feature is dark on the server.
    #[error("SkyNexus has the connector switched off right now")]
    NotConfigured { retry_after: Option<Duration> },
    /// 503 `busy` or `unavailable`: transient.
    #[error("SkyNexus is busy")]
    Unavailable { retry_after: Option<Duration> },
    #[error("unexpected answer from SkyNexus: {0}")]
    Protocol(String),
}

impl ApiError {
    /// The server's `Retry-After`, if it sent one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ApiError::RateLimited { retry_after }
            | ApiError::NotConfigured { retry_after }
            | ApiError::Unavailable { retry_after } => *retry_after,
            _ => None,
        }
    }
}

/// `Retry-After` in seconds (the HTTP-date form is not used by SkyNexus).
pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let secs: u64 = value?.trim().parse().ok()?;
    Some(Duration::from_secs(secs.min(24 * 3600)))
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        ApiError::Network(e.to_string())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PairStartRequest<'a> {
    pub device_name: &'a str,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PairStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    #[serde(default = "default_expires_in")]
    pub expires_in: u64,
    #[serde(default)]
    pub interval: Option<u64>,
}

fn default_expires_in() -> u64 {
    600
}

impl PairStart {
    /// The page to open: the complete URI if the server gave one, otherwise
    /// the verification URI with the code as `?code=`.
    pub fn browser_url(&self, base: &str) -> String {
        if let Some(u) = self.verification_uri_complete.as_deref().filter(|u| !u.is_empty()) {
            return absolute(base, u);
        }
        let uri = absolute(base, &self.verification_uri);
        let sep = if uri.contains('?') { '&' } else { '?' };
        format!("{uri}{sep}code={}", self.user_code)
    }

    pub fn interval_s(&self) -> u64 {
        self.interval.filter(|i| *i > 0).unwrap_or(DEFAULT_POLL_INTERVAL_S)
    }
}

fn absolute(base: &str, uri: &str) -> String {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        uri.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), uri.trim_start_matches('/'))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PairPollRequest<'a> {
    pub device_code: &'a str,
}

/// The connector credential: opaque, long lived, revocable.
#[derive(Clone, PartialEq, Deserialize)]
pub struct Credential {
    pub credential: String,
    #[serde(default)]
    pub device_id: Option<String>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("credential", &"<redacted>")
            .field("device_id", &self.device_id)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PollOutcome {
    Pending,
    SlowDown,
    Expired,
    Denied,
    /// `access_denied` with `error_description: "too_many_devices"` (10 active sims per account).
    TooManyDevices,
    /// `invalid_grant`: unknown, malformed or already collected device code.
    Invalid,
    Approved(Credential),
}

#[derive(Debug, Deserialize)]
struct PollDevice {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PollBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    device: Option<PollDevice>,
    #[serde(default)]
    device_id: Option<String>,
}

/// Interprets a poll answer. RFC 8628 servers answer errors with HTTP 400;
/// the body decides, a 429 counts as `slow_down`, and a 5xx is transient.
pub fn parse_poll(status: u16, body: &str) -> Result<PollOutcome, ApiError> {
    if status == 429 {
        return Ok(PollOutcome::SlowDown);
    }
    if status >= 500 {
        return Err(ApiError::Network(format!("server answered HTTP {status}")));
    }
    let parsed: PollBody = serde_json::from_str(body)
        .map_err(|_| ApiError::Protocol(format!("poll answered HTTP {status} without JSON")))?;
    if let Some(err) = parsed.error.as_deref() {
        return match err {
            "authorization_pending" => Ok(PollOutcome::Pending),
            "slow_down" => Ok(PollOutcome::SlowDown),
            "expired_token" | "expired" => Ok(PollOutcome::Expired),
            "access_denied" if parsed.error_description.as_deref() == Some("too_many_devices") => {
                Ok(PollOutcome::TooManyDevices)
            }
            "access_denied" | "denied" => Ok(PollOutcome::Denied),
            "invalid_grant" => Ok(PollOutcome::Invalid),
            other => Err(ApiError::Protocol(format!("poll error {other}"))),
        };
    }
    match parsed.credential {
        Some(c) if !c.is_empty() && (200..300).contains(&status) => Ok(PollOutcome::Approved(Credential {
            credential: c,
            device_id: parsed.device.and_then(|d| d.id).or(parsed.device_id),
        })),
        _ => Err(ApiError::Protocol(format!(
            "poll answered HTTP {status} with neither error nor credential"
        ))),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TicketBody {
    ticket: String,
    #[serde(default)]
    expires_at: Option<serde_json::Value>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    relay_url: Option<String>,
}

/// A short-lived relay ticket (ES256 JWT, about 15 minutes).
///
/// Its expiry is measured on the monotonic clock from the moment it was
/// received, from the server's `expires_in`. The JWT `exp` is only compared
/// with the local clock to warn about skew: a PC whose clock runs fast must
/// not think every fresh ticket is already stale.
#[derive(Clone, PartialEq)]
pub struct Ticket {
    pub token: String,
    /// How long the ticket lives from `received`.
    pub lifetime: Duration,
    pub received: Instant,
    /// Local clock minus server clock, in seconds, from the JWT `iat`.
    pub clock_skew_s: Option<i64>,
    pub relay_url: Option<String>,
}

impl Ticket {
    pub fn expires_at(&self) -> Instant {
        self.received + self.lifetime
    }
}

impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticket")
            .field("token", &"<redacted>")
            .field("lifetime", &self.lifetime)
            .field("clock_skew_s", &self.clock_skew_s)
            .field("relay_url", &self.relay_url)
            .finish()
    }
}

/// The longest lifetime believed (the relay refuses tickets over 960 s).
const MAX_TICKET_LIFETIME: Duration = Duration::from_secs(3600);
/// Warn when the local clock is this far off the server's.
pub const SKEW_WARN_S: i64 = 60;

/// Reads the ticket answer. The lifetime comes from `expires_in`, then the
/// JWT's own `exp - iat` (both server time), and only as a last resort from
/// `expires_at` against the local clock. `now_ms` is the local wall clock,
/// used for the skew warning; `received` is the monotonic receive time.
pub fn parse_ticket(body: &str, now_ms: u64, received: Instant) -> Result<Ticket, ApiError> {
    let b: TicketBody = serde_json::from_str(body).map_err(|e| ApiError::Protocol(format!("ticket answer: {e}")))?;
    if b.ticket.is_empty() {
        return Err(ApiError::Protocol("empty ticket".into()));
    }
    let claims = jwt_claims(&b.ticket);
    let lifetime_s = b
        .expires_in
        .map(|s| s as f64)
        .or_else(|| claims.and_then(|(exp, iat)| Some(exp? - iat?)))
        .or_else(|| {
            let at = b.expires_at.as_ref().and_then(expiry_value_ms)?;
            Some((at as f64 - now_ms as f64) / 1000.0)
        })
        .filter(|s| s.is_finite() && *s > 0.0)
        .unwrap_or(900.0);
    let lifetime = Duration::from_secs_f64(lifetime_s).min(MAX_TICKET_LIFETIME);
    let clock_skew_s = claims
        .and_then(|(_, iat)| iat)
        .map(|iat| (now_ms as f64 / 1000.0 - iat).round() as i64);
    Ok(Ticket {
        token: b.ticket,
        lifetime,
        received,
        clock_skew_s,
        relay_url: b.relay_url.filter(|u| !u.is_empty()),
    })
}

fn expiry_value_ms(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => {
            let n = n.as_f64()?;
            if !n.is_finite() || n <= 0.0 {
                return None;
            }
            // Seconds until the year 33658; anything bigger is milliseconds.
            Some(if n < 1e12 { (n * 1000.0) as u64 } else { n as u64 })
        }
        serde_json::Value::String(s) => parse_rfc3339_ms(s),
        _ => None,
    }
}

/// The `exp` and `iat` claims of a JWT, in seconds, without verifying it
/// (the relay verifies). None when the token is not a readable JWT.
pub fn jwt_claims(token: &str) -> Option<(Option<f64>, Option<f64>)> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let num = |k: &str| v.get(k).and_then(|x| x.as_f64()).filter(|x| x.is_finite() && *x > 0.0);
    Some((num("exp"), num("iat")))
}

/// Minimal RFC 3339 reader: `YYYY-MM-DDTHH:MM:SS[.fff](Z|+HH:MM|-HH:MM)`.
pub fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.len() < 20 || !s.is_ascii() {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !matches!(&s[10..11], "T" | "t" | " ") || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut rest = &s[19..];
    let mut frac_ms = 0i64;
    if let Some(r) = rest.strip_prefix('.') {
        let digits: String = r.chars().take_while(|c| c.is_ascii_digit()).collect();
        let mut ms = digits.chars().take(3).collect::<String>();
        while ms.len() < 3 {
            ms.push('0');
        }
        frac_ms = ms.parse().ok()?;
        rest = &r[digits.len()..];
    }
    let offset_s = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let oh: i64 = o.get(1..3)?.parse().ok()?;
            let om: i64 = o.get(4..6)?.parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3600 + mi * 60 + se - offset_s;
    (secs >= 0).then(|| (secs * 1000 + frac_ms) as u64)
}

/// Howard Hinnant's days-from-civil.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `{"error": "..."}` from an error body.
fn error_code(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("error")?
        .as_str()
        .map(str::to_string)
}

/// A 503: `not_configured` means the feature is dark; `busy` and
/// `unavailable` (or no body) are transient.
fn service_unavailable(body: &str, retry_after: Option<Duration>) -> ApiError {
    match error_code(body).as_deref() {
        Some("not_configured") => ApiError::NotConfigured { retry_after },
        _ => ApiError::Unavailable { retry_after },
    }
}

fn retry_after_of(res: &reqwest::Response) -> Option<Duration> {
    parse_retry_after(
        res.headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
    )
}

/// HTTP client for the web app routes.
#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: String,
}

impl ApiClient {
    pub fn new(base: &str) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("SkyNexusConnector/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("http client");
        ApiClient {
            http,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub async fn pair_start(&self, device_name: &str) -> Result<PairStart, ApiError> {
        let req = PairStartRequest { device_name };
        let res = self.http.post(self.url(PAIR_START_PATH)).json(&req).send().await?;
        let status = res.status().as_u16();
        let retry_after = retry_after_of(&res);
        let body = res.text().await?;
        match status {
            200..=299 => serde_json::from_str(&body).map_err(|e| ApiError::Protocol(format!("pair start: {e}"))),
            503 => Err(service_unavailable(&body, retry_after)),
            429 => Err(ApiError::RateLimited { retry_after }),
            s => Err(ApiError::Protocol(format!("pair start answered HTTP {s}"))),
        }
    }

    pub async fn pair_poll(&self, device_code: &str) -> Result<PollOutcome, ApiError> {
        let res = self
            .http
            .post(self.url(PAIR_POLL_PATH))
            .json(&PairPollRequest { device_code })
            .send()
            .await?;
        let status = res.status().as_u16();
        let body = res.text().await?;
        parse_poll(status, &body)
    }

    pub async fn ticket(&self, credential: &str) -> Result<Ticket, ApiError> {
        let res = self
            .http
            .post(self.url(TICKET_PATH))
            .bearer_auth(credential)
            .header("X-Connector-Version", env!("CARGO_PKG_VERSION"))
            .send()
            .await?;
        let status = res.status().as_u16();
        let retry_after = retry_after_of(&res);
        let received = Instant::now();
        let body = res.text().await?;
        match status {
            200..=299 => parse_ticket(&body, crate::frame::now_ms(), received),
            401 => Err(ApiError::Unauthorized),
            403 => Err(ApiError::NotEnabled),
            429 => Err(ApiError::RateLimited { retry_after }),
            503 => Err(service_unavailable(&body, retry_after)),
            s => Err(ApiError::Protocol(format!("ticket answered HTTP {s}"))),
        }
    }

    /// Tells the server to revoke this device. Best effort: sign-out clears
    /// the local credential whatever the answer.
    pub async fn revoke(&self, credential: &str) -> Result<(), ApiError> {
        let res = self
            .http
            .delete(self.url(DEVICES_PATH))
            .bearer_auth(credential)
            .send()
            .await?;
        match res.status().as_u16() {
            200..=299 | 401 => Ok(()),
            s => Err(ApiError::Protocol(format!("revoke answered HTTP {s}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(iat: u64, exp: u64) -> String {
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            enc.encode(br#"{"alg":"ES256","typ":"JWT"}"#),
            enc.encode(format!(r#"{{"sub":"u1","role":"connector","iat":{iat},"exp":{exp}}}"#))
        )
    }

    #[test]
    fn pair_start_shape_and_browser_url() {
        let p: PairStart = serde_json::from_str(
            r#"{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"/connect","expires_in":600,"interval":5}"#,
        )
        .unwrap();
        assert_eq!(
            p.browser_url("https://app.skynexus.nl"),
            "https://app.skynexus.nl/connect?code=ABCD-EFGH"
        );
        assert_eq!(p.interval_s(), 5);

        let p: PairStart = serde_json::from_str(
            r#"{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"https://x/connect","verification_uri_complete":"https://x/connect?code=ABCD-EFGH"}"#,
        )
        .unwrap();
        assert_eq!(p.browser_url("https://ignored"), "https://x/connect?code=ABCD-EFGH");
        assert_eq!(p.interval_s(), DEFAULT_POLL_INTERVAL_S);
        assert_eq!(p.expires_in, 600);
    }

    #[test]
    fn poll_answers() {
        assert_eq!(
            parse_poll(400, r#"{"error":"authorization_pending"}"#).unwrap(),
            PollOutcome::Pending
        );
        assert_eq!(
            parse_poll(400, r#"{"error":"slow_down"}"#).unwrap(),
            PollOutcome::SlowDown
        );
        assert_eq!(parse_poll(429, "").unwrap(), PollOutcome::SlowDown);
        assert_eq!(
            parse_poll(400, r#"{"error":"expired_token"}"#).unwrap(),
            PollOutcome::Expired
        );
        assert_eq!(
            parse_poll(403, r#"{"error":"access_denied"}"#).unwrap(),
            PollOutcome::Denied
        );
        // the success shape
        match parse_poll(
            200,
            r#"{"credential":"snc_secret","device":{"id":"d1","name":"JOERI-PC"}}"#,
        )
        .unwrap()
        {
            PollOutcome::Approved(c) => {
                assert_eq!(c.credential, "snc_secret");
                assert_eq!(c.device_id.as_deref(), Some("d1"));
                assert!(!format!("{c:?}").contains("secret"));
            }
            o => panic!("{o:?}"),
        }
        assert_eq!(
            parse_poll(400, r#"{"error":"invalid_grant"}"#).unwrap(),
            PollOutcome::Invalid
        );
        assert_eq!(
            parse_poll(
                400,
                r#"{"error":"access_denied","error_description":"too_many_devices"}"#
            )
            .unwrap(),
            PollOutcome::TooManyDevices
        );
        let ra = Some(Duration::from_secs(60));
        assert!(matches!(
            service_unavailable(r#"{"error":"not_configured"}"#, ra),
            ApiError::NotConfigured { .. }
        ));
        let busy = service_unavailable(r#"{"error":"busy"}"#, ra);
        assert!(matches!(busy, ApiError::Unavailable { .. }));
        assert_eq!(busy.retry_after(), ra);
        assert!(matches!(service_unavailable("", None), ApiError::Unavailable { .. }));
        assert_eq!(parse_retry_after(Some(" 60 ")), ra);
        assert_eq!(parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT")), None);
        assert_eq!(parse_retry_after(None), None);
        assert!(
            matches!(parse_poll(500, "oops"), Err(ApiError::Network(_))),
            "5xx is transient"
        );
        assert!(matches!(
            parse_poll(503, r#"{"error":"unavailable"}"#),
            Err(ApiError::Network(_))
        ));
        assert!(parse_poll(400, r#"{"error":"something_new"}"#).is_err());
    }

    #[test]
    fn ticket_lifetime_comes_from_expires_in_on_the_monotonic_clock() {
        // The ticket shape, received by a PC whose clock runs 20 minutes fast:
        // the JWT exp is already "past" locally, but the ticket is good for 900 s.
        let iat = 1_793_836_800;
        let local_now_ms = (iat + 1200) * 1000;
        let received = Instant::now();
        let body = format!(
            r#"{{"ticket":"{}","role":"connector","expires_in":900,"expires_at":"2026-11-05T00:15:00.000Z"}}"#,
            jwt(iat, iat + 900)
        );
        let t = parse_ticket(&body, local_now_ms, received).unwrap();
        assert_eq!(t.lifetime, Duration::from_secs(900));
        assert_eq!(t.expires_at(), received + Duration::from_secs(900));
        assert_eq!(t.clock_skew_s, Some(1200), "fast clock is reported, not obeyed");
        assert_eq!(t.relay_url, None);
        assert!(!format!("{t:?}").contains(&t.token));
    }

    #[test]
    fn ticket_lifetime_fallbacks() {
        let r = Instant::now();
        // no expires_in: the JWT's own exp - iat, which is server time on both sides
        let body = format!(r#"{{"ticket":"{}"}}"#, jwt(1000, 1600));
        assert_eq!(
            parse_ticket(&body, 99_999_999_000, r).unwrap().lifetime,
            Duration::from_secs(600)
        );
        // an opaque ticket: expires_at against the local clock, the last resort
        let t = parse_ticket(r#"{"ticket":"opaque","expires_at":1800000000}"#, 1_799_999_100_000, r).unwrap();
        assert_eq!(t.lifetime, Duration::from_secs(900));
        assert_eq!(t.clock_skew_s, None);
        let t = parse_ticket(
            r#"{"ticket":"opaque","expires_at":"2027-01-15T12:00:00.250+01:00"}"#,
            parse_rfc3339_ms("2027-01-15T10:59:00.250Z").unwrap(),
            r,
        )
        .unwrap();
        assert_eq!(t.lifetime, Duration::from_secs(60));
        // nothing usable, or an expiry already past: assume 15 minutes
        assert_eq!(
            parse_ticket(r#"{"ticket":"opaque"}"#, 0, r).unwrap().lifetime,
            Duration::from_secs(900)
        );
        let t = parse_ticket(r#"{"ticket":"opaque","expires_at":1000}"#, 2_000_000, r).unwrap();
        assert_eq!(t.lifetime, Duration::from_secs(900));
        // an absurd lifetime is capped
        let t = parse_ticket(r#"{"ticket":"opaque","expires_in":999999,"relay_url":"wss://r"}"#, 0, r).unwrap();
        assert_eq!(t.lifetime, Duration::from_secs(3600));
        assert_eq!(t.relay_url.as_deref(), Some("wss://r"));
        assert!(parse_ticket(r#"{"ticket":""}"#, 0, r).is_err());
    }

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("2026-11-05T00:00:00Z"), Some(1_793_836_800_000));
        assert_eq!(parse_rfc3339_ms("2026-11-05T01:00:00+01:00"), Some(1_793_836_800_000));
        assert_eq!(parse_rfc3339_ms("2026-11-05T00:00:00.5Z"), Some(1_793_836_800_500));
        assert_eq!(parse_rfc3339_ms("not a date"), None);
    }
}
