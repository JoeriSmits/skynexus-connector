//! The merged GSX state: snapshot plus patches from the Remote API.
//! Lenient by design: the patch paths are not verified against a real
//! capture, so unknown types and keys are ignored and a malformed message
//! never panics or clears good state.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceView {
    pub id: String,
    /// GSX raw state: 1 available .. 6 completed.
    pub state: u8,
    pub can_trigger: bool,
    pub operator: String,
    pub progress: String,
}

/// Top-level keys of the normalised tree and the JSON type each must have.
fn key_ok(key: &str, v: &Value) -> bool {
    match key {
        "services" => v.is_array(),
        "airport" | "aircraft" | "parking" | "menu" | "simbrief" => v.is_object() || v.is_null(),
        _ => true,
    }
}

fn unescape(seg: &str) -> String {
    seg.replace("~1", "/").replace("~0", "~")
}

/// Child of `node` named `seg`: an object key, an array index, or (arrays
/// of objects) the element whose `id` equals `seg`.
fn child_mut<'a>(node: &'a mut Value, seg: &str) -> Option<&'a mut Value> {
    match node {
        Value::Object(m) => m.get_mut(seg),
        Value::Array(a) => match seg.parse::<usize>() {
            Ok(i) => a.get_mut(i),
            Err(_) => a.iter_mut().find(|e| e.get("id").and_then(Value::as_str) == Some(seg)),
        },
        _ => None,
    }
}

/// Sets the value at `segs` below `root`. Missing object keys are created
/// at the last step only; anything that does not fit is ignored.
fn set_path(root: &mut Value, segs: &[String], value: Value) {
    let Some((last, parents)) = segs.split_last() else {
        return;
    };
    let mut node = root;
    for seg in parents {
        match child_mut(node, seg) {
            Some(n) => node = n,
            None => return,
        }
    }
    match node {
        Value::Object(m) => {
            m.insert(last.clone(), value);
        }
        other => {
            if let Some(slot) = child_mut(other, last) {
                *slot = value;
            }
        }
    }
}

/// Merged state of one GSX session. A `hello` starts a new session and
/// calls `reset`, so a reconnect never keeps the old services as live.
/// The supervisor should also call `reset` when the socket drops.
#[derive(Debug, Default)]
pub struct GsxState {
    version: Option<String>,
    /// Snapshot state with `handlerData` flattened into the root, so a patch
    /// to `/airport` and one to `/handlerData/airport` land in the same place.
    root: Value,
}

impl GsxState {
    pub fn apply(&mut self, msg: &Value) {
        match msg.get("type").and_then(Value::as_str) {
            // A hello without a numeric protocol is malformed: ignore it.
            Some("hello") if msg.get("protocol").is_some_and(Value::is_u64) => {
                self.reset();
                if let Some(e) = msg.get("engine").and_then(Value::as_str) {
                    self.version = Some(e.to_string());
                }
            }
            Some("snapshot") => {
                if let Some(Value::Object(state)) = msg.get("state") {
                    self.root = Value::Object(Default::default());
                    self.merge_object(state);
                }
            }
            Some("patch") => self.patch(msg),
            // Results are the client's business; events carry nothing we show.
            _ => {}
        }
    }

    /// Forgets everything: snapshot data and the engine version.
    pub fn reset(&mut self) {
        self.root = Value::Null;
        self.version = None;
    }

    fn merge_object(&mut self, obj: &serde_json::Map<String, Value>) {
        for (k, v) in obj {
            if k == "handlerData" {
                if let Value::Object(h) = v {
                    self.merge_object(h);
                }
            } else if key_ok(k, v) {
                self.set(std::slice::from_ref(k), v.clone());
            }
        }
    }

    fn set(&mut self, segs: &[String], value: Value) {
        if !self.root.is_object() {
            self.root = Value::Object(Default::default());
        }
        set_path(&mut self.root, segs, value);
    }

    fn patch(&mut self, msg: &Value) {
        let (Some(path), Some(value)) = (msg.get("path").and_then(Value::as_str), msg.get("value")) else {
            return;
        };
        let mut segs: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(unescape).collect();
        if segs.first().map(String::as_str) == Some("handlerData") {
            segs.remove(0);
            if segs.is_empty() {
                if let Value::Object(h) = value {
                    self.merge_object(h);
                }
                return;
            }
        }
        if segs.len() == 1 && !key_ok(&segs[0], value) {
            return;
        }
        // A deep patch needs the top-level key to exist already.
        self.set(&segs, value.clone());
    }

    fn text(&self, path: &[&str]) -> Option<&str> {
        let mut n = &self.root;
        for p in path {
            n = n.get(p)?;
        }
        n.as_str().filter(|s| !s.is_empty())
    }

    pub fn airport(&self) -> Option<&str> {
        self.text(&["airport", "icao"])
    }

    pub fn gate(&self) -> Option<&str> {
        self.text(&["parking", "uiGateName"])
    }

    pub fn gate_type(&self) -> Option<&str> {
        self.text(&["parking", "uiType"])
    }

    pub fn services(&self) -> Vec<ServiceView> {
        let Some(list) = self.root.get("services").and_then(Value::as_array) else {
            return vec![];
        };
        list.iter()
            .filter_map(|s| {
                let id = s.get("id")?.as_str()?.to_string();
                let str_of = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                Some(ServiceView {
                    id,
                    state: s.get("stateRaw").and_then(Value::as_u64).unwrap_or(0).min(255) as u8,
                    can_trigger: s.get("canTrigger").and_then(Value::as_bool).unwrap_or(false),
                    operator: str_of("operator"),
                    progress: str_of("progressText"),
                })
            })
            .collect()
    }

    pub fn parkings(&self) -> Vec<String> {
        let list = self.root.pointer("/airport/parkings").and_then(Value::as_array);
        list.into_iter()
            .flatten()
            .filter_map(|p| p.get("uiGateName").or_else(|| p.get("index"))?.as_str())
            .map(str::to_string)
            .collect()
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Vec<Value> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/gsx/synthetic-turnaround.jsonl"
        );
        let text = std::fs::read_to_string(path).unwrap();
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["msg"].clone())
            .collect()
    }

    fn progress(s: &GsxState, id: &str) -> String {
        s.services().into_iter().find(|v| v.id == id).unwrap().progress
    }

    #[test]
    fn snapshot_gives_airport_gate_parkings_services() {
        let msgs = fixture();
        let mut s = GsxState::default();
        for m in &msgs[..2] {
            s.apply(m);
        }
        assert_eq!(s.version(), Some("4.0.8"));
        assert_eq!(s.airport(), Some("EHAM"));
        assert_eq!(s.gate(), Some("D 7"));
        assert_eq!(s.gate_type(), Some("GATE_MEDIUM"));
        assert_eq!(s.parkings().len(), 10);
        assert!(s.parkings().contains(&"D 7".to_string()));
        assert_eq!(s.services().len(), 12);
    }

    #[test]
    fn boarding_progress_follows_the_patches() {
        let msgs = fixture();
        let mut s = GsxState::default();
        let mut seen = vec![];
        for m in &msgs {
            s.apply(m);
            if m["type"] == "patch" && m["path"] == "/services" {
                seen.push(progress(&s, "Boarding"));
            }
        }
        assert_eq!(seen[0], "Requested");
        assert_eq!(seen[2], "31/174 passengers");
        assert_eq!(seen[6], "174/174 passengers");
    }

    #[test]
    fn airport_and_gate_patches_replace_the_old_ones() {
        let msgs = fixture();
        let mut s = GsxState::default();
        msgs.iter().for_each(|m| s.apply(m));
        assert_eq!(s.airport(), Some("EGLL"));
        assert_eq!(s.gate(), Some("Gate 14"));
        assert!(s.parkings().contains(&"Gate 521".to_string()));
        let b = s.services().into_iter().find(|v| v.id == "Boarding").unwrap();
        assert_eq!(b.state, 1);
        assert!(b.can_trigger);
    }

    #[test]
    fn patch_on_one_service_leaves_the_others() {
        let msgs = fixture();
        let mut s = GsxState::default();
        msgs[..2].iter().for_each(|m| s.apply(m));
        let before = s.services();
        let p = json!({"type":"patch","path":"/services/1/progressText","value":"x"});
        s.apply(&p);
        let after = s.services();
        assert_eq!(after[1].progress, "x");
        assert_eq!(before[0], after[0]);
        assert_eq!(before[2..], after[2..]);
        let by_id = json!({"type":"patch","path":"/services/GPU/stateRaw","value":5});
        s.apply(&by_id);
        let gpu = s.services().into_iter().find(|v| v.id == "GPU").unwrap();
        assert_eq!(gpu.state, 5);
        assert_eq!(s.services().len(), 12);
    }

    #[test]
    fn garbage_never_clears_good_state() {
        let msgs = fixture();
        let mut s = GsxState::default();
        msgs[..2].iter().for_each(|m| s.apply(m));
        let bad = [
            json!(null),
            json!("text"),
            json!({"type":"patch"}),
            json!({"type":"patch","path":5,"value":1}),
            json!({"type":"patch","path":"/services","value":"nope"}),
            json!({"type":"patch","path":"/airport","value":[1]}),
            json!({"type":"patch","path":"/services/99/x","value":1}),
            json!({"type":"patch","path":"/airport/icao/x/y","value":1}),
            json!({"type":"snapshot","state":"nope"}),
            json!({"type":"snapshot"}),
            json!({"type":"mystery","x":1}),
            json!({"type":"result","id":"a"}),
            json!({"type":"event","topic":"engine"}),
            json!({"type":"hello","engine":7}),
        ];
        for m in &bad {
            s.apply(m);
        }
        assert_eq!(s.airport(), Some("EHAM"));
        assert_eq!(s.gate(), Some("D 7"));
        assert_eq!(s.services().len(), 12);
        assert_eq!(s.version(), Some("4.0.8"));
    }

    #[test]
    fn handler_data_patch_merges_airport() {
        let mut s = GsxState::default();
        let p = json!({"type":"patch","path":"/handlerData","value":{"airport":{"icao":"LFPG","parkings":[]}}});
        s.apply(&p);
        assert_eq!(s.airport(), Some("LFPG"));
        let q = json!({"type":"patch","path":"/handlerData/airport/icao","value":"LFPO"});
        s.apply(&q);
        assert_eq!(s.airport(), Some("LFPO"));
    }

    #[test]
    fn hello_after_reconnect_drops_stale_state() {
        let msgs = fixture();
        let mut s = GsxState::default();
        msgs[..2].iter().for_each(|m| s.apply(m));
        assert_eq!(s.services().len(), 12);
        s.apply(&json!({"type":"hello","protocol":2,"engine":"4.0.9"}));
        assert!(s.services().is_empty());
        assert_eq!(s.airport(), None);
        assert_eq!(s.gate(), None);
        assert_eq!(s.version(), Some("4.0.9"));
    }

    #[test]
    fn reset_clears_everything() {
        let msgs = fixture();
        let mut s = GsxState::default();
        msgs[..2].iter().for_each(|m| s.apply(m));
        s.reset();
        assert!(s.services().is_empty());
        assert_eq!(s.version(), None);
        assert_eq!(s.airport(), None);
    }
}
