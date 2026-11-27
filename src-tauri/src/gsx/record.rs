//! `--gsx-record FILE`: connect to GSX's local Remote API and write every
//! text message to FILE as one JSONL line `{"at": ms, "msg": <raw>}`, so
//! the integration has real fixtures. Remote API only: no SimConnect.

use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

const RETRY: Duration = Duration::from_secs(2);
/// If GSX sends no `hello`, subscribe anyway after this long.
const HELLO_WAIT: Duration = Duration::from_secs(3);
const SUMMARY_EVERY: u64 = 50;

const SUBSCRIBE: &str = r#"{"type":"subscribe","id":"subscribe","channels":["state","prompts","toasts"]}"#;
const STATE_GET: &str = r#"{"type":"command","id":"state.get.1","verb":"state.get","args":{}}"#;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// One JSONL line: the raw message embedded as JSON when it parses, else as
/// a string, so nothing is lost.
fn line_for(at: u128, raw: &str) -> String {
    let msg =
        serde_json::from_str::<serde_json::Value>(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()));
    serde_json::json!({ "at": at, "msg": msg }).to_string()
}

fn msg_type(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    Some(v.get("type")?.as_str()?.to_string())
}

/// Records until Ctrl+C.
pub async fn run(path: &Path, port: u16) -> anyhow::Result<()> {
    let url = format!("ws://127.0.0.1:{port}");
    record_until(path, &url, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

/// Records from `url` into `path` (appending) until `stop` resolves.
/// Reconnects every 2 s while GSX is unreachable.
pub async fn record_until(path: &Path, url: &str, stop: impl Future<Output = ()>) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    log::info!("gsx-record: writing {} from {url}, Ctrl+C to stop", path.display());
    tokio::pin!(stop);
    let mut announced = false;
    let mut total = 0u64;
    loop {
        let conn = tokio::select! {
            _ = &mut stop => break,
            c = tokio_tungstenite::connect_async(url) => c,
        };
        match conn {
            Ok((ws, _)) => {
                announced = false;
                log::info!("gsx-record: connected to {url}");
                let done = session(ws, &mut file, &mut total, &mut stop).await;
                if done {
                    break;
                }
                log::info!("gsx-record: connection closed, reconnecting");
            }
            Err(_) => {
                if !announced {
                    log::info!("gsx-record: GSX not reachable at {url}, retrying every 2 s");
                    announced = true;
                }
            }
        }
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep(RETRY) => {}
        }
    }
    log::info!("gsx-record: stopped after {total} messages");
    Ok(())
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Runs one connection. Returns true when `stop` fired.
async fn session(
    mut ws: Ws,
    file: &mut std::fs::File,
    total: &mut u64,
    stop: &mut (impl Future<Output = ()> + Unpin),
) -> bool {
    let mut subscribed = false;
    let hello_deadline = tokio::time::sleep(HELLO_WAIT);
    tokio::pin!(hello_deadline);
    loop {
        tokio::select! {
            _ = &mut *stop => return true,
            _ = &mut hello_deadline, if !subscribed => {
                subscribed = true;
                if subscribe(&mut ws).await.is_err() {
                    return false;
                }
            }
            next = ws.next() => {
                let text = match next {
                    Some(Ok(Message::Text(t))) => t.to_string(),
                    Some(Ok(Message::Ping(p))) => {
                        let _ = ws.send(Message::Pong(p)).await;
                        continue;
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return false,
                    Some(Ok(_)) => continue,
                };
                let line = line_for(now_ms(), &text);
                if writeln!(file, "{line}").and_then(|_| file.flush()).is_err() {
                    log::warn!("gsx-record: write failed");
                }
                *total += 1;
                if *total / SUMMARY_EVERY != (*total - 1) / SUMMARY_EVERY {
                    log::info!("gsx-record: {total} messages (last: {})",
                        msg_type(&text).unwrap_or_else(|| "?".into()));
                }
                if !subscribed && msg_type(&text).as_deref() == Some("hello") {
                    subscribed = true;
                    if subscribe(&mut ws).await.is_err() {
                        return false;
                    }
                }
            }
        }
    }
}

async fn subscribe(ws: &mut Ws) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    ws.send(Message::text(SUBSCRIBE)).await?;
    ws.send(Message::text(STATE_GET)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn writes_every_text_message_as_a_line() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
            let hello = r#"{"v":1,"type":"hello","protocol":"x"}"#;
            ws.send(Message::text(hello)).await.unwrap();
            let mut got = Vec::new();
            while got.len() < 2 {
                if let Some(Ok(Message::Text(t))) = ws.next().await {
                    got.push(t.to_string());
                }
            }
            ws.send(Message::text(r#"{"type":"patch","n":1}"#)).await.unwrap();
            ws.send(Message::text("not json")).await.unwrap();
            let _ = ws.close(None).await;
            got
        });
        let path = std::env::temp_dir().join(format!("gsx-rec-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let stop = tokio::time::sleep(Duration::from_millis(800));
        record_until(&path, &format!("ws://127.0.0.1:{port}"), stop)
            .await
            .unwrap();
        let got = server.await.unwrap();
        assert!(got[0].contains("\"subscribe\""));
        assert!(got[0].contains("\"channels\""));
        assert!(got[1].contains("state.get"));
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["msg"]["type"], "hello");
        assert!(lines[0]["at"].as_u64().unwrap() > 0);
        assert_eq!(lines[1]["msg"]["type"], "patch");
        assert_eq!(lines[2]["msg"], "not json");
    }
}
