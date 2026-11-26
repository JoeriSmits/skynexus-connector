//! Local HTTP client for TechLog (`/api/ping`, `/api/state`).
//!
//! `read` never returns an error: every outcome is a health [`Code`], so the
//! runner can report it to the EFB as-is.

use std::time::Duration;

use url::Url;

use super::model::{parse_state, Ping, State};

pub const DEFAULT_BASE: &str = "http://127.0.0.1:7720";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Outcome of one read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Code {
    Ok,
    /// Nothing listens on the TechLog port.
    NotRunning,
    /// TechLog.exe is up but its engine is not (ping `engine:false`, or 503).
    Starting,
    /// Something answers on the port but it is not TechLog.
    NotTechlog,
    /// 401 or 403 from `/api/state`.
    Auth,
    Http(u16),
    Timeout,
    /// `/api/state` did not match what we read; carries the JSON path.
    Schema(String),
}

impl Code {
    pub fn as_str(&self) -> &'static str {
        match self {
            Code::Ok => "OK",
            Code::NotRunning => "TL_NOT_RUNNING",
            Code::Starting => "TL_STARTING",
            Code::NotTechlog => "TL_NOT_TECHLOG",
            Code::Auth => "TL_AUTH",
            Code::Http(_) => "TL_HTTP",
            Code::Timeout => "TL_TIMEOUT",
            Code::Schema(_) => "TL_SCHEMA",
        }
    }

    /// The failing JSON path, for `Schema` only.
    pub fn path(&self) -> Option<&str> {
        match self {
            Code::Schema(p) => Some(p),
            _ => None,
        }
    }

    /// The HTTP status, for `Http` only.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Code::Http(s) => Some(*s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TechlogClient {
    http: reqwest::Client,
    base: Url,
}

impl TechlogClient {
    pub fn new(base: Url) -> Self {
        Self::with_timeout(base, REQUEST_TIMEOUT)
    }

    /// Like `new` with a custom whole-request timeout (used by tests).
    pub fn with_timeout(base: Url, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("SkyNexusConnector/", env!("CARGO_PKG_VERSION")))
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT)
            // Localhost traffic must never go through a system proxy, and
            // never follow a redirect off the TechLog port.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client");
        TechlogClient { http, base }
    }

    /// Ping, then fetch state. The version is returned whenever the ping
    /// identified TechLog, even if the state read then fails.
    pub async fn read(&self) -> (Code, Option<String>, Option<State>) {
        let ping = match self.ping().await {
            Ok(p) => p,
            Err(code) => return (code, None, None),
        };
        let version = ping.version;
        match self.state().await {
            Ok(state) => (Code::Ok, version, Some(state)),
            Err(code) => (code, version, None),
        }
    }

    async fn ping(&self) -> Result<Ping, Code> {
        let url = self.base.join("api/ping").expect("ping url");
        let resp = self.http.get(url).send().await.map_err(transport_code)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Code::Http(status.as_u16()));
        }
        let body = resp.text().await.map_err(transport_code)?;
        let ping: Ping = serde_json::from_str(&body).map_err(|_| Code::NotTechlog)?;
        if ping.engine == Some(false) {
            return Err(Code::Starting);
        }
        if ping.app.as_deref() != Some("TechLog") {
            return Err(Code::NotTechlog);
        }
        Ok(ping)
    }

    async fn state(&self) -> Result<State, Code> {
        let url = self.base.join("api/state").expect("state url");
        let resp = self.http.get(url).send().await.map_err(transport_code)?;
        let status = resp.status();
        match status.as_u16() {
            503 => return Err(Code::Starting),
            401 | 403 => return Err(Code::Auth),
            _ if !status.is_success() => return Err(Code::Http(status.as_u16())),
            _ => {}
        }
        let body = resp.text().await.map_err(transport_code)?;
        parse_state(&body).map_err(|e| Code::Schema(e.path))
    }
}

/// A transport failure. Any failure while connecting (refused, unreachable,
/// connect timeout) means TechLog is not running: Windows only reports a
/// refused localhost port after ~2 s, which looks like a timeout. Only a
/// timeout after the connection was made is a real `Timeout`.
fn transport_code(e: reqwest::Error) -> Code {
    if e.is_connect() {
        Code::NotRunning
    } else if e.is_timeout() {
        Code::Timeout
    } else {
        Code::NotRunning
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::techlog::model::tests::fixture;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> TechlogClient {
        TechlogClient::with_timeout(Url::parse(&server.uri()).unwrap(), Duration::from_millis(300))
    }

    async fn mock(server: &MockServer, p: &str, resp: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(resp)
            .mount(server)
            .await;
    }

    fn ping_ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string(fixture("ping.json"))
    }

    #[test]
    fn wire_codes() {
        assert_eq!(Code::Ok.as_str(), "OK");
        assert_eq!(Code::NotRunning.as_str(), "TL_NOT_RUNNING");
        assert_eq!(Code::Starting.as_str(), "TL_STARTING");
        assert_eq!(Code::NotTechlog.as_str(), "TL_NOT_TECHLOG");
        assert_eq!(Code::Auth.as_str(), "TL_AUTH");
        assert_eq!(Code::Http(500).as_str(), "TL_HTTP");
        assert_eq!(Code::Timeout.as_str(), "TL_TIMEOUT");
        assert_eq!(Code::Schema("x".into()).as_str(), "TL_SCHEMA");
        assert_eq!(Code::Http(500).http_status(), Some(500));
        assert_eq!(Code::Schema("dispatch".into()).path(), Some("dispatch"));
        assert_eq!(Code::Ok.path(), None);
    }

    #[tokio::test]
    async fn happy_path() {
        let s = MockServer::start().await;
        mock(&s, "/api/ping", ping_ok()).await;
        mock(
            &s,
            "/api/state",
            ResponseTemplate::new(200).set_body_string(fixture("state-fenix-go-mel.json")),
        )
        .await;
        let (code, version, state) = client(&s).read().await;
        assert_eq!(code, Code::Ok);
        assert_eq!(version.as_deref(), Some("2.1.2"));
        assert!(state.is_some());
    }

    #[tokio::test]
    async fn connect_timeout_is_not_running() {
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        let err = http.get("http://10.255.255.1:81/").send().await.unwrap_err();
        // Offline CI may fail the route at once; both are connect failures.
        assert!(err.is_connect(), "{err:?}");
        assert_eq!(transport_code(err), Code::NotRunning);
    }

    #[tokio::test]
    async fn refused_port_is_not_running() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        drop(listener);
        let (code, version, state) = TechlogClient::new(url).read().await;
        assert_eq!(code, Code::NotRunning);
        assert!(version.is_none() && state.is_none());
    }

    #[tokio::test]
    async fn engine_false_is_starting() {
        let s = MockServer::start().await;
        mock(
            &s,
            "/api/ping",
            ResponseTemplate::new(200).set_body_string(r#"{"ok":true,"engine":false}"#),
        )
        .await;
        let (code, _, state) = client(&s).read().await;
        assert_eq!(code, Code::Starting);
        assert!(state.is_none());
    }

    #[tokio::test]
    async fn other_app_or_non_json_is_not_techlog() {
        for body in [
            r#"{"ok":true,"app":"Other","version":"1.0.0"}"#,
            r#"{"ok":true}"#,
            "<html>hi</html>",
        ] {
            let s = MockServer::start().await;
            mock(&s, "/api/ping", ResponseTemplate::new(200).set_body_string(body)).await;
            assert_eq!(client(&s).read().await.0, Code::NotTechlog, "{body}");
        }
    }

    #[tokio::test]
    async fn ping_non_2xx_is_http() {
        let s = MockServer::start().await;
        mock(&s, "/api/ping", ResponseTemplate::new(404)).await;
        assert_eq!(client(&s).read().await.0, Code::Http(404));
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let s = MockServer::start().await;
        mock(
            &s,
            "/api/ping",
            ResponseTemplate::new(302).insert_header("location", "http://example.com/api/ping"),
        )
        .await;
        assert_eq!(client(&s).read().await.0, Code::Http(302));
    }

    #[tokio::test]
    async fn ping_timeout() {
        let s = MockServer::start().await;
        mock(&s, "/api/ping", ping_ok().set_delay(Duration::from_secs(2))).await;
        assert_eq!(client(&s).read().await.0, Code::Timeout);
    }

    #[tokio::test]
    async fn state_status_codes() {
        for (status, want) in [
            (503, Code::Starting),
            (401, Code::Auth),
            (403, Code::Auth),
            (500, Code::Http(500)),
        ] {
            let s = MockServer::start().await;
            mock(&s, "/api/ping", ping_ok()).await;
            mock(&s, "/api/state", ResponseTemplate::new(status)).await;
            let (code, version, state) = client(&s).read().await;
            assert_eq!(code, want, "status {status}");
            assert_eq!(version.as_deref(), Some("2.1.2"));
            assert!(state.is_none());
        }
    }

    #[tokio::test]
    async fn state_timeout() {
        let s = MockServer::start().await;
        mock(&s, "/api/ping", ping_ok()).await;
        mock(
            &s,
            "/api/state",
            ResponseTemplate::new(200)
                .set_body_string(fixture("state-fenix-go-mel.json"))
                .set_delay(Duration::from_secs(2)),
        )
        .await;
        assert_eq!(client(&s).read().await.0, Code::Timeout);
    }

    #[tokio::test]
    async fn state_missing_dispatch_is_schema_error() {
        let s = MockServer::start().await;
        mock(&s, "/api/ping", ping_ok()).await;
        let mut v: serde_json::Value = serde_json::from_str(&fixture("state-fenix-go-mel.json")).unwrap();
        v.as_object_mut().unwrap().remove("dispatch");
        mock(
            &s,
            "/api/state",
            ResponseTemplate::new(200).set_body_string(v.to_string()),
        )
        .await;
        let (code, _, _) = client(&s).read().await;
        assert_eq!(code.as_str(), "TL_SCHEMA");
        assert_eq!(code.path(), Some("dispatch"));
    }
}
