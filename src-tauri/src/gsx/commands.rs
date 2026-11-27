//! The EFB's writes to GSX. The relay delivers `{type:"cmd", id, verb,
//! args}` from the account's viewers; only the SkyNexus allowlist is
//! known, and each verb is translated here, never passed through to GSX:
//!
//! | SkyNexus verb  | Args            | GSX                                          |
//! |----------------|-----------------|----------------------------------------------|
//! | `gsx.gate`     | `{gate}` 1..24  | `gate.select {gate, force: false}`           |
//! | `gsx.simbrief` | none            | `command.run {command: "RELOAD_SIMBRIEF"}`   |
//! | `gsx.read`     | `{on}`          | nothing: the runner flips the read switch    |
//!
//! The GSX args follow Any2GSX's client (`GsxApiClient.GateSelect` and
//! `CommandRun`, which maps `GsxCommandRunType.ReloadSimbrief` (4) to
//! `"RELOAD_SIMBRIEF"`). `force` is false so a gate the pilot picked in
//! GSX is not overridden. Anything else, `gsx.service` and `gsx.push`
//! included, is ignored: SkyNexus does not call services or push back.

use serde_json::{json, Value};

use super::block::CmdResult;
use super::client::{CommandError, GsxClient};

/// The command was not run: GSX is not being read (switched off, or not
/// `GSX_OK`).
pub const NOT_READING: &str = "not_reading";
/// The command was not run: another one was still waiting for GSX.
pub const BUSY: &str = "busy";
/// GSX did not answer in time.
pub const TIMEOUT: &str = "timeout";
/// The gate is not one of GSX's parkings for its airport; GSX was not asked.
pub const UNKNOWN_GATE: &str = "unknown_gate";

const MAX_ID: usize = 32;
const MAX_GATE: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Select this gate in GSX.
    Gate(String),
    /// Let GSX load the latest SimBrief plan.
    Simbrief,
    /// Read GSX or not, until the next ticket says otherwise.
    Read(bool),
}

impl Cmd {
    /// The SkyNexus verb, for the log.
    pub fn verb(&self) -> &'static str {
        match self {
            Cmd::Gate(_) => "gsx.gate",
            Cmd::Simbrief => "gsx.simbrief",
            Cmd::Read(_) => "gsx.read",
        }
    }
}

fn valid_id(id: &str) -> bool {
    (1..=MAX_ID).contains(&id.len()) && id.chars().all(|c| (' '..='~').contains(&c))
}

/// A `cmd` message the Connector knows, with its id; None for anything
/// else (other types, unknown verbs, bad ids or args).
pub fn parse_cmd(v: &Value) -> Option<(String, Cmd)> {
    if v.get("type")?.as_str()? != "cmd" {
        return None;
    }
    let id = v.get("id")?.as_str()?;
    if !valid_id(id) {
        return None;
    }
    let args = v.get("args");
    let cmd = match v.get("verb")?.as_str()? {
        "gsx.gate" => {
            let gate = args?.get("gate")?.as_str()?;
            let n = gate.encode_utf16().count();
            if gate.trim().is_empty() || n > MAX_GATE || gate.chars().any(char::is_control) {
                return None;
            }
            Cmd::Gate(gate.to_string())
        }
        "gsx.simbrief" => Cmd::Simbrief,
        "gsx.read" => Cmd::Read(args?.get("on")?.as_bool()?),
        _ => return None,
    };
    Some((id.to_string(), cmd))
}

/// A result the Connector decided itself (`not_reading`, `busy`).
pub fn refused(id: String, code: &str) -> CmdResult {
    CmdResult {
        id,
        ok: false,
        code: Some(code.to_string()),
    }
}

/// A `gsx.gate` must be exactly one of `shown` (the trimmed gate list the
/// block carries, [`super::block::parkings`]), else `Err(unknown_gate)`.
/// The gate GSX is asked for is its own, untrimmed name from `raw`
/// (GSX's parkings as it sent them). Other commands pass through.
pub fn resolve_gate(id: &str, cmd: Cmd, shown: Option<&[String]>, raw: &[String]) -> Result<Cmd, CmdResult> {
    let Cmd::Gate(gate) = cmd else { return Ok(cmd) };
    if !shown.is_some_and(|n| n.contains(&gate)) {
        return Err(refused(id.to_string(), UNKNOWN_GATE));
    }
    let original = raw.iter().find(|r| r.trim() == gate).cloned();
    Ok(Cmd::Gate(original.unwrap_or(gate)))
}

/// The GSX verb and args for a command; None for `Read`, which GSX never sees.
fn gsx_call(cmd: &Cmd) -> Option<(&'static str, Value)> {
    match cmd {
        Cmd::Gate(gate) => Some(("gate.select", json!({ "gate": gate, "force": false }))),
        Cmd::Simbrief => Some(("command.run", json!({ "command": "RELOAD_SIMBRIEF" }))),
        Cmd::Read(_) => None,
    }
}

/// Runs one command on GSX and waits for its result (3 s). GSX's own
/// error code is passed on; no answer in time is `timeout`, a socket that
/// failed meanwhile `not_reading`. `Read` sends nothing and is ok.
pub async fn execute(client: &mut GsxClient, id: String, cmd: Cmd) -> CmdResult {
    let Some((verb, args)) = gsx_call(&cmd) else {
        return CmdResult {
            id,
            ok: true,
            code: None,
        };
    };
    match client.command(verb, args).await {
        Ok(r) => {
            if !r.ok {
                let code = r.code.as_deref().unwrap_or("-");
                log::info!("gsx: {} refused by GSX ({code})", cmd.verb());
            }
            CmdResult {
                id,
                ok: r.ok,
                code: if r.ok { None } else { r.code },
            }
        }
        Err(e) => {
            log::info!("gsx: {} got no result ({e})", cmd.verb());
            let code = match e {
                CommandError::Timeout => TIMEOUT,
                CommandError::Closed | CommandError::Send(_) => NOT_READING,
            };
            refused(id, code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    type ServerWs = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    const HELLO: &str = r#"{"type":"hello","protocol":2,"engine":"4.0.8"}"#;

    fn cmd(verb: &str, args: Value) -> Value {
        json!({"type":"cmd","id":"c1","verb":verb,"args":args})
    }

    #[test]
    fn the_three_verbs_parse() {
        let gate = cmd("gsx.gate", json!({"gate":"Gate 14"}));
        assert_eq!(parse_cmd(&gate), Some(("c1".into(), Cmd::Gate("Gate 14".into()))));
        let sb = cmd("gsx.simbrief", json!({}));
        assert_eq!(parse_cmd(&sb), Some(("c1".into(), Cmd::Simbrief)));
        let off = cmd("gsx.read", json!({"on":false}));
        assert_eq!(parse_cmd(&off), Some(("c1".into(), Cmd::Read(false))));
    }

    #[test]
    fn services_pushback_and_strangers_are_ignored() {
        let svc = cmd("gsx.service", json!({"service":"Boarding"}));
        assert_eq!(parse_cmd(&svc), None);
        assert_eq!(parse_cmd(&cmd("gsx.push", json!({}))), None);
        assert_eq!(parse_cmd(&cmd("gate.select", json!({"gate":"A"}))), None);
        let frame = json!({"type":"status","id":"c1","verb":"gsx.simbrief"});
        assert_eq!(parse_cmd(&frame), None);
    }

    #[test]
    fn bad_ids_and_args_are_ignored() {
        let with_id = |id: Value| json!({"type":"cmd","id":id,"verb":"gsx.simbrief"});
        assert!(parse_cmd(&with_id(json!("x".repeat(32)))).is_some());
        for id in [json!(""), json!("x".repeat(33)), json!("caf\u{e9}"), json!(7)] {
            assert_eq!(parse_cmd(&with_id(id.clone())), None, "{id}");
        }
        let gate = |g: Value| cmd("gsx.gate", json!({ "gate": g }));
        assert!(parse_cmd(&gate(json!("G".repeat(24)))).is_some());
        for g in [json!(""), json!("  "), json!("G".repeat(25)), json!("A\n1"), json!(14)] {
            assert_eq!(parse_cmd(&gate(g.clone())), None, "{g}");
        }
        assert_eq!(parse_cmd(&cmd("gsx.gate", json!({}))), None);
        assert_eq!(parse_cmd(&cmd("gsx.read", json!({"on":"yes"}))), None);
        assert_eq!(parse_cmd(&json!({"type":"cmd","id":"c","verb":"gsx.read"})), None);
    }

    #[test]
    fn only_a_gate_on_the_list_is_selected() {
        let names = ["D 7".to_string(), "D 8".to_string()];
        let gate = |g: &str| Cmd::Gate(g.into());
        let ok = resolve_gate("c1", gate("D 8"), Some(&names), &names);
        assert_eq!(ok, Ok(gate("D 8")));
        let unknown = Err(refused("c1".into(), UNKNOWN_GATE));
        for g in ["d 8", "D8", "D 8 ", "Gate 14"] {
            assert_eq!(resolve_gate("c1", gate(g), Some(&names), &names), unknown, "{g}");
        }
        assert_eq!(resolve_gate("c1", gate("D 8"), None, &[]), unknown, "no list yet");
        assert_eq!(resolve_gate("c1", Cmd::Simbrief, None, &[]), Ok(Cmd::Simbrief));
    }

    #[test]
    fn a_padded_gsx_name_is_matched_trimmed_and_sent_as_gsx_has_it() {
        let shown = ["D 7".to_string(), "D 8".to_string()];
        let raw = [" D 7 ".to_string(), "D 8".to_string()];
        let got = resolve_gate("c1", Cmd::Gate("D 7".into()), Some(&shown), &raw);
        assert_eq!(got, Ok(Cmd::Gate(" D 7 ".into())));
        let got = resolve_gate("c1", Cmd::Gate("D 8".into()), Some(&shown), &raw);
        assert_eq!(got, Ok(Cmd::Gate("D 8".into())));
    }

    /// A GSX that answers the first command it gets with `answer` (or never).
    async fn gsx(answer: Option<Value>) -> (u16, tokio::sync::oneshot::Receiver<Value>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let mut ws: ServerWs = tokio_tungstenite::accept_async(s).await.unwrap();
            ws.send(Message::text(HELLO)).await.unwrap();
            let mut tx = Some(tx);
            while let Some(Ok(m)) = ws.next().await {
                let Message::Text(t) = m else { continue };
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] != "command" || v["verb"] == "state.get" {
                    continue;
                }
                if let Some(mut a) = answer.clone() {
                    a["id"] = v["id"].clone();
                    ws.send(Message::text(a.to_string())).await.unwrap();
                }
                if let Some(tx) = tx.take() {
                    let _ = tx.send(v);
                }
            }
        });
        (port, rx)
    }

    async fn client(port: u16) -> GsxClient {
        GsxClient::connect(port).await.unwrap_or_else(|_| panic!("refused")).0
    }

    fn ok() -> Option<Value> {
        Some(json!({"type":"result","ok":true}))
    }

    #[tokio::test]
    async fn a_gate_selects_that_gate() {
        let (port, sent) = gsx(ok()).await;
        let mut c = client(port).await;
        let r = execute(&mut c, "c1".into(), Cmd::Gate("Gate 14".into())).await;
        assert_eq!(
            r,
            CmdResult {
                id: "c1".into(),
                ok: true,
                code: None
            }
        );
        let v = sent.await.unwrap();
        assert_eq!(v["verb"], "gate.select");
        assert_eq!(v["args"], json!({"gate":"Gate 14","force":false}));
    }

    #[tokio::test]
    async fn simbrief_runs_reload_simbrief() {
        let (port, sent) = gsx(ok()).await;
        let mut c = client(port).await;
        let r = execute(&mut c, "c2".into(), Cmd::Simbrief).await;
        assert!(r.ok);
        let v = sent.await.unwrap();
        assert_eq!(v["verb"], "command.run");
        assert_eq!(v["args"], json!({"command":"RELOAD_SIMBRIEF"}));
    }

    #[tokio::test]
    async fn gsx_codes_are_passed_on() {
        let no = json!({"type":"result","ok":false,"error":{"code":"NO_GATE"}});
        let (port, _sent) = gsx(Some(no)).await;
        let mut c = client(port).await;
        let r = execute(&mut c, "c3".into(), Cmd::Gate("Z 99".into())).await;
        assert_eq!(r, refused("c3".into(), "NO_GATE"));
    }

    #[tokio::test]
    async fn no_result_in_time_is_timeout() {
        let (port, _sent) = gsx(None).await;
        let mut c = client(port).await;
        c.set_result_timeout(Duration::from_millis(100));
        let r = execute(&mut c, "c4".into(), Cmd::Simbrief).await;
        assert_eq!(r, refused("c4".into(), TIMEOUT));
    }

    #[tokio::test]
    async fn read_sends_nothing_to_gsx() {
        let (port, mut sent) = gsx(ok()).await;
        let mut c = client(port).await;
        let r = execute(&mut c, "c5".into(), Cmd::Read(true)).await;
        assert!(r.ok);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(sent.try_recv().is_err());
    }
}
