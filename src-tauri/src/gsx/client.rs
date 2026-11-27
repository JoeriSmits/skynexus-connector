//! Client for GSX's local Remote API (WebSocket, protocol 2). Connects and
//! reads the hello (all detection needs), subscribes to `state` only when
//! GSX is read, and sends commands with ids `sn.<n>`.

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::detect::Hello;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// GSX says hello right after connecting; no hello in this time is Invalid.
const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
/// Messages kept for `next` while a command waits; the oldest are dropped.
const BACKLOG_MAX: usize = 256;
const SEND_TIMEOUT: Duration = Duration::from_secs(3);
const RESULT_TIMEOUT: Duration = Duration::from_secs(3);

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Why a command got no result.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("timeout")]
    Timeout,
    #[error("connection closed")]
    Closed,
    #[error("send failed ({0})")]
    Send(String),
}

/// GSX's answer to a command.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandResult {
    pub ok: bool,
    /// GSX's error code when `ok` is false.
    pub code: Option<String>,
    pub message: Option<String>,
    /// The whole `result` message.
    pub raw: Value,
}

pub struct GsxClient {
    ws: Ws,
    next_id: u64,
    /// Messages that arrived while a command waited for its result.
    backlog: VecDeque<Value>,
    result_timeout: Duration,
}

/// Parses a `hello` message; None when it is not one we understand.
fn parse_hello(v: &Value) -> Option<Hello> {
    if v.get("type")?.as_str()? != "hello" {
        return None;
    }
    // A number, or the same number as a digit string.
    let protocol = match v.get("protocol")? {
        Value::Number(n) => u32::try_from(n.as_u64()?).ok()?,
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => s.parse().ok()?,
        _ => return None,
    };
    let engine = v.get("engine").and_then(Value::as_str).unwrap_or("").to_string();
    let auth_required = v.get("authRequired").and_then(Value::as_bool).unwrap_or(false);
    Some(Hello::Valid {
        protocol,
        engine,
        auth_required,
    })
}

impl GsxClient {
    /// Connects to `ws://127.0.0.1:port` and reads the hello; sends nothing.
    /// `Err(Hello::Refused)` when nobody answers. Otherwise the hello GSX
    /// sent (`Invalid` when it was missing or malformed). Detection needs
    /// only this; [`GsxClient::subscribe`] starts the stream, and
    /// [`GsxClient::close`] says goodbye.
    pub async fn connect(port: u16) -> Result<(GsxClient, Hello), Hello> {
        let url = format!("ws://127.0.0.1:{port}");
        let ws = match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(&url)).await {
            Ok(Ok((ws, _))) => ws,
            _ => return Err(Hello::Refused),
        };
        let mut c = GsxClient {
            ws,
            next_id: 0,
            backlog: VecDeque::new(),
            result_timeout: RESULT_TIMEOUT,
        };
        let hello = match tokio::time::timeout(HELLO_TIMEOUT, c.read_json()).await {
            Ok(Some(v)) => parse_hello(&v).unwrap_or(Hello::Invalid),
            _ => Hello::Invalid,
        };
        Ok((c, hello))
    }

    /// Subscribes to the `state` channel and asks for a snapshot, which then
    /// arrives through `next`. Only for a valid, open hello.
    pub async fn subscribe(&mut self) -> anyhow::Result<()> {
        self.send(&json!({"type":"subscribe","id":"subscribe","channels":["state"]}))
            .await?;
        self.send(&json!({"type":"command","id":"state.get.1","verb":"state.get","args":{}}))
            .await
    }

    /// Sends a Close frame (time-boxed) and drops the socket.
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(SEND_TIMEOUT, self.ws.close(None)).await;
    }

    async fn send(&mut self, v: &Value) -> anyhow::Result<()> {
        let sent = self.ws.send(Message::text(v.to_string()));
        tokio::time::timeout(SEND_TIMEOUT, sent)
            .await
            .map_err(|_| anyhow::anyhow!("send timeout"))??;
        Ok(())
    }

    /// Next JSON text message; skips pings, binary and non-JSON text.
    /// None when the connection is gone.
    async fn read_json(&mut self) -> Option<Value> {
        loop {
            match self.ws.next().await? {
                Ok(Message::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        return Some(v);
                    }
                }
                Ok(Message::Ping(p)) => {
                    let _ = self.ws.send(Message::Pong(p)).await;
                }
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    /// The next message from GSX; None when the connection closed.
    pub async fn next(&mut self) -> Option<Value> {
        if let Some(v) = self.backlog.pop_front() {
            return Some(v);
        }
        self.read_json().await
    }

    #[cfg(test)]
    pub(crate) fn set_result_timeout(&mut self, d: Duration) {
        self.result_timeout = d;
    }

    /// Sends `verb` with `args` and waits for the `result` with the same id.
    /// Other messages in the meantime are kept for `next`.
    pub async fn command(&mut self, verb: &str, args: Value) -> Result<CommandResult, CommandError> {
        self.next_id += 1;
        let id = format!("sn.{}", self.next_id);
        self.send(&json!({"type":"command","id":id,"verb":verb,"args":args}))
            .await
            .map_err(|e| CommandError::Send(e.to_string()))?;
        let limit = self.result_timeout;
        let wait = async {
            loop {
                let v = self.read_json().await?;
                let is_ours = v.get("type").and_then(Value::as_str) == Some("result")
                    && v.get("id").and_then(Value::as_str) == Some(id.as_str());
                if is_ours {
                    return Some(v);
                }
                if self.backlog.len() >= BACKLOG_MAX {
                    self.backlog.pop_front();
                }
                self.backlog.push_back(v);
            }
        };
        let raw = match tokio::time::timeout(limit, wait).await {
            Ok(Some(v)) => v,
            Ok(None) => return Err(CommandError::Closed),
            Err(_) => return Err(CommandError::Timeout),
        };
        let err = raw.get("error");
        let text = |k: &str| err.and_then(|e| e.get(k)).and_then(Value::as_str).map(str::to_string);
        Ok(CommandResult {
            ok: raw.get("ok").and_then(Value::as_bool).unwrap_or(false),
            code: text("code"),
            message: text("message"),
            raw,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    type ServerWs = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    async fn server<F, Fut>(f: F) -> u16
    where
        F: FnOnce(ServerWs) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            f(tokio_tungstenite::accept_async(s).await.unwrap()).await;
        });
        port
    }

    async fn text(ws: &mut ServerWs) -> Value {
        loop {
            if let Some(Ok(Message::Text(t))) = ws.next().await {
                return serde_json::from_str(&t).unwrap();
            }
        }
    }

    const HELLO: &str = r#"{"v":2,"type":"hello","protocol":2,"engine":"4.0.8","authRequired":false}"#;

    #[tokio::test]
    async fn refused_port_is_refused() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(matches!(GsxClient::connect(port).await, Err(Hello::Refused)));
    }

    #[tokio::test]
    async fn subscribe_asks_for_state_only_then_a_snapshot() {
        let port = server(|mut ws| async move {
            ws.send(Message::text(HELLO)).await.unwrap();
            let a = text(&mut ws).await;
            let b = text(&mut ws).await;
            assert_eq!(a["type"], "subscribe");
            assert_eq!(a["channels"], json!(["state"]));
            assert_eq!(b["verb"], "state.get");
            ws.send(Message::text(r#"{"type":"snapshot","state":3}"#))
                .await
                .unwrap();
            let _ = ws.next().await;
        })
        .await;
        let (mut c, hello) = GsxClient::connect(port).await.unwrap_or_else(|_| panic!("refused"));
        let want = Hello::Valid {
            protocol: 2,
            engine: "4.0.8".into(),
            auth_required: false,
        };
        assert_eq!(hello, want);
        c.subscribe().await.unwrap();
        assert_eq!(c.next().await.unwrap()["type"], "snapshot");
    }

    #[tokio::test]
    async fn connect_sends_nothing_and_close_sends_a_close_frame() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let port = server(|mut ws| async move {
            ws.send(Message::text(HELLO)).await.unwrap();
            let mut got = vec![];
            while let Some(Ok(m)) = ws.next().await {
                got.push(m);
            }
            let _ = tx.send(got);
        })
        .await;
        let (c, hello) = GsxClient::connect(port).await.unwrap_or_else(|_| panic!("refused"));
        assert!(matches!(hello, Hello::Valid { .. }));
        c.close().await;
        let got = rx.await.unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(matches!(got[0], Message::Close(_)), "{got:?}");
    }

    #[tokio::test]
    async fn protocol_may_be_a_digit_string() {
        let cases = [
            (r#"{"type":"hello","protocol":"2","engine":"e"}"#, Some(2)),
            (r#"{"type":"hello","protocol":"12","engine":"e"}"#, Some(12)),
            (r#"{"type":"hello","protocol":"2a"}"#, None),
            (r#"{"type":"hello","protocol":"-1"}"#, None),
            (r#"{"type":"hello","protocol":""}"#, None),
            (r#"{"type":"hello","protocol":2.5}"#, None),
        ];
        for (raw, want) in cases {
            let v: Value = serde_json::from_str(raw).unwrap();
            let got = match parse_hello(&v) {
                Some(Hello::Valid { protocol, .. }) => Some(protocol),
                _ => None,
            };
            assert_eq!(got, want, "{raw}");
        }
    }

    #[tokio::test]
    async fn malformed_hello_is_invalid() {
        for bad in ["not json", r#"{"type":"hello"}"#, r#"{"type":"patch"}"#] {
            let port = server(move |mut ws| async move {
                ws.send(Message::text(bad)).await.unwrap();
                let _ = ws.next().await;
            })
            .await;
            let (_c, hello) = GsxClient::connect(port).await.unwrap_or_else(|_| panic!("refused"));
            assert_eq!(hello, Hello::Invalid, "{bad}");
        }
    }

    #[tokio::test]
    async fn auth_required_hello_does_not_subscribe() {
        let port = server(|mut ws| async move {
            let h = r#"{"type":"hello","protocol":2,"engine":"x","authRequired":true}"#;
            ws.send(Message::text(h)).await.unwrap();
            let got = tokio::time::timeout(Duration::from_millis(300), ws.next()).await;
            assert!(got.is_err() || !matches!(got, Ok(Some(Ok(Message::Text(_))))));
        })
        .await;
        let (_c, hello) = GsxClient::connect(port).await.unwrap_or_else(|_| panic!("refused"));
        assert!(matches!(
            hello,
            Hello::Valid {
                auth_required: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn command_matches_its_result_and_keeps_other_messages() {
        let port = server(|mut ws| async move {
            ws.send(Message::text(HELLO)).await.unwrap();
            text(&mut ws).await;
            text(&mut ws).await;
            let cmd = text(&mut ws).await;
            assert_eq!(cmd["id"], "sn.1");
            assert_eq!(cmd["verb"], "gate.select");
            assert_eq!(cmd["args"]["gate"], "D 7");
            ws.send(Message::text(r#"{"type":"patch","path":"/x","value":1}"#))
                .await
                .unwrap();
            ws.send(Message::text(r#"{"type":"result","id":"other","ok":true}"#))
                .await
                .unwrap();
            let r = r#"{"type":"result","id":"sn.1","ok":false,"error":{"code":"NO_GATE","message":"m"}}"#;
            ws.send(Message::text(r)).await.unwrap();
            let _ = ws.next().await;
        })
        .await;
        let (mut c, _) = GsxClient::connect(port).await.unwrap_or_else(|_| panic!("refused"));
        c.subscribe().await.unwrap();
        let r = c.command("gate.select", json!({"gate":"D 7"})).await.unwrap();
        assert!(!r.ok);
        assert_eq!(r.code.as_deref(), Some("NO_GATE"));
        assert_eq!(c.next().await.unwrap()["type"], "patch");
        assert_eq!(c.next().await.unwrap()["id"], "other");
    }

    #[tokio::test]
    async fn command_times_out_without_a_result() {
        let port = server(|mut ws| async move {
            ws.send(Message::text(HELLO)).await.unwrap();
            while ws.next().await.is_some() {}
        })
        .await;
        let (mut c, _) = GsxClient::connect(port).await.unwrap_or_else(|_| panic!("refused"));
        c.result_timeout = Duration::from_millis(200);
        let e = c.command("menu.close", json!({})).await.unwrap_err();
        assert!(matches!(e, CommandError::Timeout), "{e}");
    }

    #[tokio::test]
    async fn backlog_is_capped_dropping_the_oldest() {
        let port = server(|mut ws| async move {
            ws.send(Message::text(HELLO)).await.unwrap();
            text(&mut ws).await;
            text(&mut ws).await;
            text(&mut ws).await;
            for n in 0..300 {
                ws.send(Message::text(format!(r#"{{"type":"patch","n":{n}}}"#)))
                    .await
                    .unwrap();
            }
            ws.send(Message::text(r#"{"type":"result","id":"sn.1","ok":true}"#))
                .await
                .unwrap();
            let _ = ws.next().await;
        })
        .await;
        let (mut c, _) = GsxClient::connect(port).await.unwrap_or_else(|_| panic!("refused"));
        c.subscribe().await.unwrap();
        c.command("menu.close", json!({})).await.unwrap();
        assert_eq!(c.backlog.len(), BACKLOG_MAX);
        assert_eq!(c.next().await.unwrap()["n"], 300 - BACKLOG_MAX);
    }
}
