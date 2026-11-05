//! X-Plane 12.1.1+ through its built-in web API (localhost only, no plugin).
//!
//! 1. `GET /api/capabilities` tells whether the API is there.
//! 2. `GET /api/v1/datarefs?filter[name]=...` turns each dataref name into an id.
//! 3. The WebSocket `ws://localhost:8086/api/v1` gets one `dataref_subscribe_values`
//!    request; X-Plane then pushes `dataref_update_values` at about 10 Hz with
//!    only the values that changed. Byte-array datarefs arrive base64 encoded.
//!
//! Docs: <https://developer.x-plane.com/article/x-plane-web-api/>

use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;
use tungstenite::{Message, WebSocket};

use super::xplane::{
    OwnRaw, TcasRaw, OWN_DATAREFS, TCAS_ELE, TCAS_FLIGHT_ID, TCAS_LAT, TCAS_LON, TCAS_MODE_S, TCAS_PSI, TCAS_V,
    TCAS_VX, TCAS_VZ,
};
use super::{Sim, SimError};
use crate::frame::{now_ms, Frame, SimKind};

pub const DEFAULT_PORT: u16 = 8086;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Own(usize),
    TcasLat,
    TcasLon,
    TcasEle,
    TcasPsi,
    TcasV,
    TcasVx,
    TcasVz,
    TcasModeS,
    TcasFlightId,
}

pub fn tcas_roles() -> [(&'static str, Role); 9] {
    [
        (TCAS_LAT, Role::TcasLat),
        (TCAS_LON, Role::TcasLon),
        (TCAS_ELE, Role::TcasEle),
        (TCAS_PSI, Role::TcasPsi),
        (TCAS_V, Role::TcasV),
        (TCAS_VX, Role::TcasVx),
        (TCAS_VZ, Role::TcasVz),
        (TCAS_MODE_S, Role::TcasModeS),
        (TCAS_FLIGHT_ID, Role::TcasFlightId),
    ]
}

/// `{"api": {"versions": ["v1", "v2"]}, "x-plane": {"version": "12.1.4"}}`
pub fn parse_capabilities(body: &str) -> Option<(Vec<String>, Option<String>)> {
    let v: Value = serde_json::from_str(body).ok()?;
    let versions = v
        .pointer("/api/versions")?
        .as_array()?
        .iter()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect();
    let xp = v
        .pointer("/x-plane/version")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((versions, xp))
}

/// `{"data": [{"id": 123, "name": "...", "value_type": "double"}]}` -> the id
/// of the entry whose name matches exactly.
pub fn parse_lookup(body: &str, name: &str) -> Option<u64> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.get("data")?
        .as_array()?
        .iter()
        .find(|d| d.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|d| d.get("id")?.as_u64())
}

/// What the `/api/capabilities` answer means. `None` is "nothing listens".
pub fn capabilities_verdict(answer: Option<(u16, &str)>) -> Result<(Vec<String>, Option<String>), String> {
    match answer {
        None => Err("not running".into()),
        Some((200, body)) => parse_capabilities(body).ok_or_else(|| "unexpected /api/capabilities answer".into()),
        Some((403, _)) => {
            Err("X-Plane refuses the connection: untick \"Disable incoming traffic\" in Settings > Network".into())
        }
        Some((404, _)) => Err("no web API (X-Plane older than 12.1.1); trying UDP".into()),
        Some((status, _)) => Err(format!("web API answered HTTP {status}")),
    }
}

pub fn subscribe_message(req_id: u64, ids: &[u64]) -> String {
    let datarefs: Vec<Value> = ids.iter().map(|id| serde_json::json!({ "id": id })).collect();
    serde_json::json!({
        "req_id": req_id,
        "type": "dataref_subscribe_values",
        "params": { "datarefs": datarefs },
    })
    .to_string()
}

#[derive(Debug, PartialEq)]
pub enum WsMessage {
    Result {
        req_id: u64,
        success: bool,
        error: Option<String>,
    },
    Update(HashMap<u64, Value>),
    Other,
}

pub fn parse_ws(text: &str) -> WsMessage {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return WsMessage::Other;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("result") => WsMessage::Result {
            req_id: v.get("req_id").and_then(Value::as_u64).unwrap_or(0),
            success: v.get("success").and_then(Value::as_bool).unwrap_or(false),
            error: v
                .get("error_message")
                .or_else(|| v.get("error_code"))
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        Some("dataref_update_values") => {
            let data = v
                .get("data")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, val)| Some((k.parse::<u64>().ok()?, val.clone())))
                        .collect()
                })
                .unwrap_or_default();
            WsMessage::Update(data)
        }
        _ => WsMessage::Other,
    }
}

fn as_f64s(v: &Value) -> Vec<f64> {
    match v {
        Value::Array(a) => a.iter().map(|x| x.as_f64().unwrap_or(f64::NAN)).collect(),
        Value::Number(n) => vec![n.as_f64().unwrap_or(f64::NAN)],
        _ => Vec::new(),
    }
}

fn as_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => base64::engine::general_purpose::STANDARD.decode(s).unwrap_or_default(),
        Value::Array(a) => a.iter().map(|x| x.as_u64().unwrap_or(0) as u8).collect(),
        _ => Vec::new(),
    }
}

/// Folds one update into the raw state. Returns (own changed, traffic changed).
pub fn apply_update(
    update: &HashMap<u64, Value>,
    roles: &HashMap<u64, Role>,
    own: &mut OwnRaw,
    tcas: &mut TcasRaw,
) -> (bool, bool) {
    let (mut own_changed, mut tcas_changed) = (false, false);
    for (id, value) in update {
        let Some(role) = roles.get(id) else { continue };
        match role {
            Role::Own(i) => {
                if let Some(x) = value.as_f64().or_else(|| as_f64s(value).first().copied()) {
                    own.set(*i, x);
                    own_changed = true;
                }
            }
            Role::TcasFlightId => {
                tcas.flight_id = as_bytes(value);
                tcas_changed = true;
            }
            other => {
                let target = match other {
                    Role::TcasLat => &mut tcas.lat,
                    Role::TcasLon => &mut tcas.lon,
                    Role::TcasEle => &mut tcas.ele_m,
                    Role::TcasPsi => &mut tcas.psi,
                    Role::TcasV => &mut tcas.v_mps,
                    Role::TcasVx => &mut tcas.vx,
                    Role::TcasVz => &mut tcas.vz,
                    _ => &mut tcas.mode_s,
                };
                *target = as_f64s(value);
                tcas_changed = true;
            }
        }
    }
    (own_changed, tcas_changed)
}

pub struct XplaneWeb {
    ws: WebSocket<TcpStream>,
    roles: HashMap<u64, Role>,
    own: OwnRaw,
    tcas: TcasRaw,
    version: Option<String>,
}

impl XplaneWeb {
    pub fn connect(port: u16) -> Result<Self, String> {
        let mut headers = reqwest::header::HeaderMap::new();
        let json = reqwest::header::HeaderValue::from_static("application/json");
        headers.insert(reqwest::header::ACCEPT, json.clone());
        headers.insert(reqwest::header::CONTENT_TYPE, json);
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(1500))
            .default_headers(headers)
            .build()
            .map_err(|e| e.to_string())?;
        let base = format!("http://127.0.0.1:{port}");
        let answer = http.get(format!("{base}/api/capabilities")).send().ok().map(|res| {
            let status = res.status().as_u16();
            (status, res.text().unwrap_or_default())
        });
        let (versions, version) = capabilities_verdict(answer.as_ref().map(|(s, b)| (*s, b.as_str())))?;
        if !versions.iter().any(|v| v == "v1") {
            return Err(format!("web API without v1 (has {versions:?})"));
        }

        let lookup = |name: &str| -> Option<u64> {
            let body = http
                .get(format!("{base}/api/v1/datarefs"))
                .query(&[("filter[name]", name), ("fields", "id,name")])
                .send()
                .ok()?
                .text()
                .ok()?;
            parse_lookup(&body, name)
        };
        let mut roles = HashMap::new();
        for (i, name) in OWN_DATAREFS.iter().enumerate() {
            match lookup(name) {
                Some(id) => {
                    roles.insert(id, Role::Own(i));
                }
                None if i < 2 => return Err(format!("dataref {name} not found")),
                None => log::warn!("xplane: dataref {name} not found"),
            }
        }
        for (name, role) in tcas_roles() {
            match lookup(name) {
                Some(id) => {
                    roles.insert(id, role);
                }
                None => log::info!("xplane: {name} not found, no traffic from it"),
            }
        }

        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| e.to_string())?;
        let (mut ws, _) = tungstenite::client(format!("ws://127.0.0.1:{port}/api/v1"), stream)
            .map_err(|e| format!("websocket: {e}"))?;
        let ids: Vec<u64> = roles.keys().copied().collect();
        ws.send(Message::text(subscribe_message(1, &ids)))
            .map_err(|e| e.to_string())?;
        ws.get_ref().set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(XplaneWeb {
            ws,
            roles,
            own: OwnRaw::default(),
            tcas: TcasRaw::default(),
            version,
        })
    }
}

impl Sim for XplaneWeb {
    fn kind(&self) -> SimKind {
        SimKind::Xplane
    }

    fn describe(&self) -> String {
        match &self.version {
            Some(v) => format!("X-Plane {v} (web API)"),
            None => "X-Plane (web API)".into(),
        }
    }

    fn poll(&mut self) -> Result<Option<Frame>, SimError> {
        let (mut own_changed, mut tcas_changed) = (false, false);
        for _ in 0..500 {
            match self.ws.read() {
                Ok(Message::Text(t)) => match parse_ws(t.as_str()) {
                    WsMessage::Update(u) => {
                        let (o, t) = apply_update(&u, &self.roles, &mut self.own, &mut self.tcas);
                        own_changed |= o;
                        tcas_changed |= t;
                    }
                    WsMessage::Result {
                        success: false, error, ..
                    } => {
                        log::warn!("xplane: subscribe refused: {}", error.unwrap_or_default());
                    }
                    _ => {}
                },
                Ok(Message::Close(_)) => return Err(SimError::Disconnected),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Err(SimError::Disconnected)
                }
                Err(e) => return Err(SimError::Other(e.to_string())),
            }
        }
        if !own_changed && !tcas_changed {
            return Ok(None);
        }
        let Some(own) = self.own.to_own() else {
            return Ok(None);
        };
        let mut f = Frame::new(SimKind::Xplane, now_ms(), own);
        if tcas_changed && self.tcas.has_positions() {
            f.traffic = Some(self.tcas.targets());
        }
        Ok(Some(f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_and_lookup() {
        let (v, xp) = parse_capabilities(r#"{"api":{"versions":["v1","v2"]},"x-plane":{"version":"12.1.4"}}"#).unwrap();
        assert_eq!(v, vec!["v1", "v2"]);
        assert_eq!(xp.as_deref(), Some("12.1.4"));
        let body = r#"{"data":[{"id":1873042848,"is_writable":true,"name":"sim/flightmodel/position/latitude","value_type":"double"}]}"#;
        assert_eq!(
            parse_lookup(body, "sim/flightmodel/position/latitude"),
            Some(1_873_042_848)
        );
        assert_eq!(parse_lookup(body, "sim/flightmodel/position/longitude"), None);
        assert_eq!(parse_lookup(r#"{"data":[]}"#, "x"), None);
    }

    #[test]
    fn capabilities_reasons() {
        assert_eq!(capabilities_verdict(None).unwrap_err(), "not running");
        assert!(capabilities_verdict(Some((404, "")))
            .unwrap_err()
            .contains("older than 12.1.1"));
        assert!(capabilities_verdict(Some((403, "")))
            .unwrap_err()
            .contains("Disable incoming traffic"));
        assert_eq!(
            capabilities_verdict(Some((500, ""))).unwrap_err(),
            "web API answered HTTP 500"
        );
        assert!(capabilities_verdict(Some((200, "nope"))).is_err());
        let (v, _) = capabilities_verdict(Some((200, r#"{"api":{"versions":["v1"]}}"#))).unwrap();
        assert_eq!(v, vec!["v1"]);
    }

    #[test]
    fn subscribe_request_shape() {
        let v: Value = serde_json::from_str(&subscribe_message(7, &[11, 22])).unwrap();
        assert_eq!(v["req_id"], 7);
        assert_eq!(v["type"], "dataref_subscribe_values");
        assert_eq!(v["params"]["datarefs"][1]["id"], 22);
    }

    #[test]
    fn result_messages() {
        assert_eq!(
            parse_ws(r#"{"req_id":1,"type":"result","success":true}"#),
            WsMessage::Result {
                req_id: 1,
                success: true,
                error: None
            }
        );
        assert_eq!(
            parse_ws(
                r#"{"req_id":2,"type":"result","success":false,"error_code":"invalid_dataref_id","error_message":"Dataref 9 not found"}"#
            ),
            WsMessage::Result {
                req_id: 2,
                success: false,
                error: Some("Dataref 9 not found".into())
            }
        );
        assert_eq!(parse_ws("garbage"), WsMessage::Other);
    }

    #[test]
    fn updates_fill_own_and_tcas_state() {
        let roles: HashMap<u64, Role> = [
            (100, Role::Own(0)),
            (101, Role::Own(1)),
            (102, Role::Own(2)),
            (110, Role::TcasLat),
            (111, Role::TcasLon),
            (116, Role::TcasFlightId),
        ]
        .into_iter()
        .collect();
        let mut ids = vec![0u8; 16];
        ids[8..14].copy_from_slice(b"TEST22");
        let b64 = base64::engine::general_purpose::STANDARD.encode(&ids);
        let msg = format!(
            r#"{{"type":"dataref_update_values","data":{{"100":50.0379,"101":8.5622,"102":111.0,"110":[50.0,50.2],"111":[8.5,8.9],"116":"{b64}","999":1}}}}"#
        );
        let WsMessage::Update(u) = parse_ws(&msg) else { panic!() };
        let (mut own, mut tcas) = (OwnRaw::default(), TcasRaw::default());
        assert_eq!(apply_update(&u, &roles, &mut own, &mut tcas), (true, true));
        let o = own.to_own().unwrap();
        assert_eq!(o.lat, 50.0379);
        let t = tcas.targets();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].callsign.as_deref(), Some("TEST22"));
        assert_eq!(t[0].lat, 50.2);

        // a later update with only one changed value keeps the rest
        let WsMessage::Update(u) = parse_ws(r#"{"type":"dataref_update_values","data":{"100":50.04}}"#) else {
            panic!()
        };
        assert_eq!(apply_update(&u, &roles, &mut own, &mut tcas), (true, false));
        assert_eq!(own.to_own().unwrap().lon, 8.5622);
    }

    #[test]
    fn connect_fails_cleanly_without_xplane() {
        // Port 9 (discard) is closed on any dev machine.
        assert!(XplaneWeb::connect(9).is_err());
    }
}
