//! The compact `gsx` block and `gsxParkings` list on the relay frames, and
//! the emitter that decides when each one goes out. The wire shape and its
//! caps follow the EFB parser (`src/lib/gsx/wire.ts`): a block over 1500
//! bytes would be dropped there, a parkings list over 8 KB rejected, so
//! both are trimmed here before they are sent.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::state::GsxState;
use crate::sim::SimExtras;

pub const BLOCK_MAX_BYTES: usize = 1500;
pub const PARKINGS_MAX_NAMES: usize = 400;
pub const PARKINGS_MAX_BYTES: usize = 8 * 1024;
const MAX_TEXT: usize = 24;
const MAX_PROGRESS: usize = 48;
const MAX_ID: usize = 32;
const PAX_MAX: u32 = 1000;

/// A changed block goes out at most this often.
const CHANGE_EVERY: Duration = Duration::from_secs(1);
/// An unchanged block still goes out this often, so a viewer that joins
/// late (or a relay that restarted) catches up within 5 s. Under 5 s so the
/// parked aircraft's 5 s frame heartbeat carries it every time.
const HEARTBEAT: Duration = Duration::from_millis(4500);
/// The gate list is re-sent this often while GSX is read.
const PARKINGS_EVERY: Duration = Duration::from_secs(60);
/// After the block went away, `null` is repeated on the heartbeat this
/// long (the relay's frame slot may replace a frame before it is sent),
/// then the field is left off.
const NULL_FOR: Duration = Duration::from_secs(15);

/// The services the EFB knows. Anything else is dropped.
const SERVICE_IDS: [&str; 12] = [
    "Boarding",
    "Deboarding",
    "Refueling",
    "Catering",
    "Departure",
    "OperateJetways",
    "OperateStairs",
    "GPU",
    "DeIce",
    "Lavatory",
    "Water",
    "Cleaning",
];

/// The outcome of the last command, echoed to the EFB. Task C2's executor
/// produces these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdResult {
    pub id: String,
    pub ok: bool,
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GsxSvc {
    pub id: String,
    pub s: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GsxPax {
    pub board: u32,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GsxLast {
    pub id: String,
    pub ok: bool,
    /// Always on the wire: `null` when there is no code.
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GsxBlock {
    pub v: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_type: Option<String>,
    pub svc: Vec<GsxSvc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pax: Option<GsxPax>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<GsxLast>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GsxParkings {
    pub apt: String,
    pub names: Vec<String>,
}

/// Serde for `Option<Option<T>>`: absent (outer None), `null` (Some(None))
/// or the value. Pair with `default` and `skip_serializing_if = "Option::is_none"`.
pub mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(v: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

/// Trimmed, at most `max` UTF-16 code units (the EFB measures JavaScript
/// string length), and None for empty text or control chars.
fn clean(s: &str, max: usize) -> Option<String> {
    let t = s.trim();
    if t.is_empty() || t.chars().any(|c| c.is_control()) {
        return None;
    }
    let mut units = 0;
    let cut: String = t
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max
        })
        .collect();
    Some(cut.trim_end().to_string()).filter(|s| !s.is_empty())
}

fn icao(s: &str) -> Option<String> {
    let u = s.trim().to_ascii_uppercase();
    (u.len() == 4 && u.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())).then_some(u)
}

/// "31/174 passengers" -> (31, 174).
fn pax_from_progress(text: &str) -> Option<GsxPax> {
    let (a, b) = text.trim().split_once('/')?;
    let board: u32 = a.trim().parse().ok()?;
    let digits: String = b.trim().chars().take_while(char::is_ascii_digit).collect();
    let total: u32 = digits.parse().ok()?;
    Some(GsxPax { board, total })
}

fn valid_pax(p: GsxPax) -> Option<GsxPax> {
    (p.board <= p.total && p.total <= PAX_MAX).then_some(p)
}

fn size(b: &GsxBlock) -> usize {
    serde_json::to_string(b).map(|s| s.len()).unwrap_or(usize::MAX)
}

/// Builds the block from the GSX state. Pax comes from the boarding
/// service's progress text, else from the sim's L:vars. A block over
/// 1500 bytes is trimmed: progress texts first, then optional strings,
/// then services from the end.
pub fn block(state: &GsxState, extras: &SimExtras, last: Option<CmdResult>) -> GsxBlock {
    let mut svc: Vec<GsxSvc> = Vec::new();
    let mut boarding_pax = None;
    for s in state.services() {
        if !SERVICE_IDS.contains(&s.id.as_str()) || svc.iter().any(|e| e.id == s.id) {
            continue;
        }
        if !(1..=6).contains(&s.state) {
            continue;
        }
        if s.id == "Boarding" {
            boarding_pax = pax_from_progress(&s.progress);
        }
        svc.push(GsxSvc {
            id: s.id,
            s: s.state,
            p: clean(&s.progress, MAX_PROGRESS),
            c: Some(s.can_trigger),
        });
    }
    let l_pax = match (extras.pax_board, extras.pax_total) {
        (Some(board), Some(total)) => Some(GsxPax { board, total }),
        _ => None,
    };
    let last = last.and_then(|r| {
        let ok_id = r.id.chars().count() <= MAX_ID && r.id.chars().all(|c| (' '..='~').contains(&c));
        (!r.id.trim().is_empty() && ok_id).then(|| GsxLast {
            id: r.id,
            ok: r.ok,
            code: r.code.as_deref().and_then(|c| clean(c, MAX_ID)),
        })
    });
    let mut b = GsxBlock {
        v: 1,
        ver: state.version().and_then(|v| clean(v, MAX_TEXT)),
        apt: state.airport().and_then(icao),
        gate: state.gate().and_then(|g| clean(g, MAX_TEXT)),
        gate_type: state.gate_type().and_then(|g| clean(g, MAX_TEXT)),
        svc,
        pax: boarding_pax.and_then(valid_pax).or_else(|| l_pax.and_then(valid_pax)),
        last,
    };
    trim(&mut b);
    b
}

fn trim(b: &mut GsxBlock) {
    if size(b) <= BLOCK_MAX_BYTES {
        return;
    }
    for s in b.svc.iter_mut() {
        s.p = None;
    }
    if size(b) <= BLOCK_MAX_BYTES {
        return;
    }
    b.ver = None;
    b.gate_type = None;
    while size(b) > BLOCK_MAX_BYTES && b.svc.pop().is_some() {}
    if size(b) > BLOCK_MAX_BYTES {
        b.last = None;
        b.gate = None;
    }
}

/// A gate name exactly as GSX has it, or None. Never cut or trimmed: the
/// EFB sends the name back and only an exact match is selected.
fn exact_name(s: &str) -> Option<String> {
    let ok = !s.trim().is_empty()
        && s.trim() == s
        && s.encode_utf16().count() <= MAX_TEXT
        && !s.chars().any(char::is_control);
    ok.then(|| s.to_string())
}

/// The gate names of GSX's airport, capped to what the EFB accepts. A name
/// over 24 UTF-16 units is left out, not cut. None when there is no valid
/// airport or no gate.
pub fn parkings(state: &GsxState) -> Option<GsxParkings> {
    let apt = state.airport().and_then(icao)?;
    let mut names: Vec<String> = state
        .parkings()
        .iter()
        .filter_map(|n| exact_name(n))
        .take(PARKINGS_MAX_NAMES)
        .collect();
    let mut p = GsxParkings { apt, names: vec![] };
    loop {
        p.names = names.clone();
        let bytes = serde_json::to_string(&p).map(|s| s.len()).unwrap_or(usize::MAX);
        if bytes <= PARKINGS_MAX_BYTES || names.is_empty() {
            break;
        }
        let keep = names.len() - (names.len() / 10).max(1);
        names.truncate(keep);
    }
    (!p.names.is_empty()).then_some(p)
}

/// Decides when the block and the gate list go on a frame.
#[derive(Debug, Default)]
pub struct Emitter {
    sent: Option<GsxBlock>,
    sent_at: Option<Instant>,
    parkings: Option<(String, Instant)>,
    /// When the block went away, and when `null` last went out.
    gone: Option<(Instant, Instant)>,
}

impl Emitter {
    /// `None`: leave the field off this frame. `Some(Some(b))`: send the
    /// block. `Some(None)`: send `null` (GSX went away): at once, then on
    /// the heartbeat for 15 s, then nothing.
    pub fn on_frame(&mut self, now: Instant, current: Option<GsxBlock>) -> Option<Option<GsxBlock>> {
        let Some(cur) = current else {
            self.sent_at = None;
            self.parkings = None;
            if self.sent.take().is_some() {
                self.gone = Some((now, now));
                return Some(None);
            }
            let (since, last) = self.gone?;
            if now.saturating_duration_since(since) >= NULL_FOR {
                self.gone = None;
                return None;
            }
            if now.saturating_duration_since(last) < HEARTBEAT {
                return None;
            }
            self.gone = Some((since, now));
            return Some(None);
        };
        self.gone = None;
        let age = self.sent_at.map(|t| now.saturating_duration_since(t));
        let due = match (&self.sent, age) {
            (None, _) | (_, None) => true,
            (Some(prev), Some(age)) if *prev == cur => age >= HEARTBEAT,
            (Some(_), Some(age)) => age >= CHANGE_EVERY,
        };
        if !due {
            return None;
        }
        self.sent = Some(cur.clone());
        self.sent_at = Some(now);
        Some(Some(cur))
    }

    /// True when the gate list of `apt` should go on this frame: the
    /// airport changed, or 60 s passed since the last send for it.
    pub fn parkings_due(&mut self, apt: &str, now: Instant) -> bool {
        let due = match &self.parkings {
            Some((a, at)) if a == apt => now.saturating_duration_since(*at) >= PARKINGS_EVERY,
            _ => true,
        };
        if due {
            self.parkings = Some((apt.to_string(), now));
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn state_with(services: Value, parkings: Vec<String>) -> GsxState {
        let mut st = GsxState::default();
        let pk: Vec<Value> = parkings.iter().map(|n| json!({"uiGateName": n})).collect();
        st.apply(&json!({"type":"hello","protocol":2,"engine":"4.0.6"}));
        st.apply(&json!({"type":"snapshot","state":3,
            "handlerData":{
                "airport":{"icao":"EHAM","parkings":pk},
                "gate":{"uiName":"","uiGateName":"Gate D4","uiType":"Gate"}},
            "services":services}));
        st
    }

    fn svc(id: &str, state: u8, progress: &str) -> Value {
        json!({"id":id,"stateRaw":state,"canTrigger":true,"progressText":progress})
    }

    fn blk(gate: &str) -> GsxBlock {
        GsxBlock {
            v: 1,
            ver: None,
            apt: None,
            gate: Some(gate.into()),
            gate_type: None,
            svc: vec![],
            pax: None,
            last: None,
        }
    }

    #[test]
    fn block_has_the_wire_shape() {
        let st = state_with(json!([svc("Boarding", 5, "31/174 passengers")]), vec![]);
        let last = CmdResult {
            id: "Boarding".into(),
            ok: false,
            code: None,
        };
        let b = block(&st, &SimExtras::default(), Some(last));
        let v: Value = serde_json::to_value(&b).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["ver"], "4.0.6");
        assert_eq!(v["apt"], "EHAM");
        assert_eq!(v["gate"], "Gate D4");
        assert_eq!(v["gateType"], "Gate");
        assert_eq!(
            v["svc"][0],
            json!({"id":"Boarding","s":5,"p":"31/174 passengers","c":true})
        );
        assert_eq!(v["pax"], json!({"board":31,"total":174}));
        assert_eq!(v["last"], json!({"id":"Boarding","ok":false,"code":null}));
        assert!(v.get("push").is_none());
    }

    #[test]
    fn pax_falls_back_to_lvars_and_is_validated() {
        let st = state_with(json!([svc("Boarding", 1, "")]), vec![]);
        let ex = SimExtras {
            couatl_started: None,
            pax_board: Some(10),
            pax_total: Some(150),
        };
        assert_eq!(block(&st, &ex, None).pax, Some(GsxPax { board: 10, total: 150 }));
        let bad = SimExtras {
            couatl_started: None,
            pax_board: Some(20),
            pax_total: Some(10),
        };
        assert_eq!(block(&st, &bad, None).pax, None, "board above total");
        let big = SimExtras {
            couatl_started: None,
            pax_board: Some(0),
            pax_total: Some(5000),
        };
        assert_eq!(block(&st, &big, None).pax, None, "above 1000");
    }

    #[test]
    fn thirteen_services_are_capped_to_the_twelve_known_ids() {
        let mut list: Vec<Value> = SERVICE_IDS.iter().map(|i| svc(i, 1, "")).collect();
        list.push(svc("Pushback", 1, ""));
        list.push(svc("GPU", 5, ""));
        let b = block(&state_with(json!(list), vec![]), &SimExtras::default(), None);
        assert_eq!(b.svc.len(), 12);
        assert!(b.svc.iter().all(|s| s.id != "Pushback"));
    }

    #[test]
    fn long_progress_is_cut_to_48_and_full_services_fit_the_budget() {
        let long = "x".repeat(200);
        let list: Vec<Value> = SERVICE_IDS.iter().map(|i| svc(i, 5, &long)).collect();
        let b = block(&state_with(json!(list), vec![]), &SimExtras::default(), None);
        assert_eq!(b.svc.len(), 12);
        assert!(b.svc.iter().all(|s| s.p.as_ref().unwrap().chars().count() == 48));
        assert!(size(&b) <= BLOCK_MAX_BYTES);
    }

    #[test]
    fn an_oversized_block_drops_progress_then_services() {
        let mut b = blk("A1");
        b.ver = Some("v".into());
        b.svc = (0..12)
            .map(|i| GsxSvc {
                id: format!("S{i}"),
                s: 5,
                p: Some("p".repeat(200)),
                c: Some(true),
            })
            .collect();
        trim(&mut b);
        assert!(size(&b) <= BLOCK_MAX_BYTES);
        assert!(b.svc.iter().all(|s| s.p.is_none()), "progress goes first");
        assert_eq!(b.svc.len(), 12);
        b.svc = (0..300)
            .map(|i| GsxSvc {
                id: format!("Svc{i}"),
                s: 5,
                p: None,
                c: None,
            })
            .collect();
        trim(&mut b);
        assert!(size(&b) <= BLOCK_MAX_BYTES);
        assert!(!b.svc.is_empty());
    }

    #[test]
    fn a_bad_airport_is_left_out() {
        let mut st = GsxState::default();
        st.apply(&json!({"type":"snapshot","state":3,
            "handlerData":{"airport":{"icao":"AMS"}}}));
        assert_eq!(block(&st, &SimExtras::default(), None).apt, None);
        assert_eq!(parkings(&st), None);
    }

    #[test]
    fn unchanged_block_goes_out_at_0_and_4_5_s_only() {
        let (mut e, t0) = (Emitter::default(), Instant::now());
        let b = blk("A1");
        let mut sent = vec![];
        for i in 0..=12u64 {
            let now = t0 + Duration::from_millis(i * 500);
            if e.on_frame(now, Some(b.clone())).is_some() {
                sent.push(i * 500);
            }
        }
        assert_eq!(sent, vec![0, 4500]);
    }

    #[test]
    fn a_parked_5_s_heartbeat_carries_the_block_every_time() {
        let (mut e, t0) = (Emitter::default(), Instant::now());
        let b = blk("A1");
        for i in 0..6u64 {
            let now = t0 + Duration::from_secs(i * 5);
            assert!(e.on_frame(now, Some(b.clone())).is_some(), "{i}");
        }
    }

    #[test]
    fn clean_counts_utf16_units_like_the_efb() {
        // U+1F6EB is two UTF-16 code units.
        let plane = "\u{1F6EB}";
        let s = clean(&plane.repeat(30), 48).unwrap();
        assert_eq!(s.encode_utf16().count(), 48);
        assert_eq!(s.chars().count(), 24);
        assert_eq!(clean(&format!("a{plane}"), 2).as_deref(), Some("a"));
        assert_eq!(clean("abc", 2).as_deref(), Some("ab"));
        assert_eq!(clean(plane, 1), None, "nothing fits");
    }

    #[test]
    fn a_change_waits_for_the_one_second_gap() {
        let (mut e, t0) = (Emitter::default(), Instant::now());
        assert!(e.on_frame(t0, Some(blk("A1"))).is_some());
        let at = |ms| t0 + Duration::from_millis(ms);
        assert_eq!(e.on_frame(at(400), Some(blk("A2"))), None);
        assert_eq!(e.on_frame(at(500), Some(blk("A2"))), None);
        assert_eq!(e.on_frame(at(1000), Some(blk("A2"))), Some(Some(blk("A2"))));
    }

    #[test]
    fn gone_gives_one_null_and_a_restart_gives_null_then_a_full_block() {
        let (mut e, t0) = (Emitter::default(), Instant::now());
        let at = |ms| t0 + Duration::from_millis(ms);
        assert!(e.on_frame(at(0), Some(blk("A1"))).is_some());
        assert_eq!(e.on_frame(at(100), None), Some(None));
        assert_eq!(e.on_frame(at(200), None), None);
        assert_eq!(e.on_frame(at(300), None), None);
        assert_eq!(e.on_frame(at(400), Some(blk("A1"))), Some(Some(blk("A1"))));
        assert_eq!(e.on_frame(at(500), None), Some(None), "restart: null first");
    }

    #[test]
    fn null_repeats_on_the_heartbeat_for_15_s_then_goes_absent() {
        let (mut e, t0) = (Emitter::default(), Instant::now());
        assert!(e.on_frame(t0, Some(blk("A1"))).is_some());
        let mut nulls = vec![];
        for i in 1..=50u64 {
            let ms = i * 500;
            let got = e.on_frame(t0 + Duration::from_millis(ms), None);
            assert!(matches!(got, None | Some(None)), "{ms}");
            if got.is_some() {
                nulls.push(ms);
            }
        }
        // Gone at 500 ms: null then, every 4.5 s, and nothing from 15.5 s.
        assert_eq!(nulls, vec![500, 5000, 9500, 14000]);
    }

    #[test]
    fn a_block_that_comes_back_stops_the_nulls() {
        let (mut e, t0) = (Emitter::default(), Instant::now());
        let at = |ms| t0 + Duration::from_millis(ms);
        assert!(e.on_frame(at(0), Some(blk("A1"))).is_some());
        assert_eq!(e.on_frame(at(500), None), Some(None));
        assert_eq!(e.on_frame(at(1000), Some(blk("A1"))), Some(Some(blk("A1"))));
        assert_eq!(e.on_frame(at(1500), Some(blk("A1"))), None);
        assert_eq!(e.on_frame(at(2000), None), Some(None));
        assert_eq!(e.on_frame(at(5000), None), None);
        assert_eq!(e.on_frame(at(6500), None), Some(None), "4.5 s after the new gone");
    }

    #[test]
    fn parkings_go_out_on_change_and_every_60_s() {
        let (mut e, t0) = (Emitter::default(), Instant::now());
        let at = |s| t0 + Duration::from_secs(s);
        assert!(e.parkings_due("EHAM", at(0)));
        assert!(!e.parkings_due("EHAM", at(1)));
        assert!(!e.parkings_due("EHAM", at(59)));
        assert!(e.parkings_due("EHAM", at(60)));
        assert!(!e.parkings_due("EHAM", at(61)));
        assert!(e.parkings_due("EGLL", at(62)), "airport changed");
    }

    #[test]
    fn parkings_are_capped_by_count_and_bytes() {
        let names: Vec<String> = (0..600).map(|i| format!("Stand {i}")).collect();
        let p = parkings(&state_with(json!([]), names)).unwrap();
        assert_eq!(p.names.len(), PARKINGS_MAX_NAMES);
        // 400 names of 24 units are over 8 KB: some are dropped, none cut.
        let long: Vec<String> = (0..400).map(|i| format!("{i:03}{}", "y".repeat(21))).collect();
        let p = parkings(&state_with(json!([]), long)).unwrap();
        assert!(p.names.len() < 400);
        assert!(p.names.iter().all(|n| n.chars().count() == 24));
        assert!(serde_json::to_string(&p).unwrap().len() <= PARKINGS_MAX_BYTES);
    }

    #[test]
    fn gate_names_over_24_units_are_dropped_not_cut() {
        let plane = "\u{1F6EB}";
        let names = vec![
            "D 7".to_string(),
            "x".repeat(24),
            "x".repeat(25),
            plane.repeat(12),
            plane.repeat(13),
            " D 8".to_string(),
        ];
        let p = parkings(&state_with(json!([]), names)).unwrap();
        assert_eq!(p.names, vec!["D 7".to_string(), "x".repeat(24), plane.repeat(12)]);
    }

    fn frame() -> crate::frame::Frame {
        use crate::frame::*;
        let own = Own {
            lat: 52.3,
            lon: 4.7,
            alt_ft: 0.0,
            gs_kt: 0.0,
            hdg: 0.0,
            trk: 0.0,
            vs_fpm: 0.0,
            on_ground: true,
            fuel_kg: None,
            callsign: None,
            tl: None,
        };
        Frame::new(SimKind::Msfs, 1, own)
    }

    #[test]
    fn frame_serialises_gsx_absent_null_or_object() {
        let mut f = frame();
        assert!(!f.to_json().contains("gsx"));
        f.gsx = Some(None);
        assert!(f.to_json().contains("\"gsx\":null"));
        assert!(f.to_json_with_tl(Some("0123456789ab")).contains("\"gsx\":null"));
        let back: crate::frame::Frame = serde_json::from_str(&f.to_json()).unwrap();
        assert_eq!(back.gsx, Some(None));
        f.gsx = Some(Some(blk("A1")));
        let v: Value = serde_json::from_str(&f.to_json()).unwrap();
        assert_eq!(v["gsx"]["gate"], "A1");
        let v: Value = serde_json::from_str(&f.to_json_with_tl(Some("0123456789ab"))).unwrap();
        assert_eq!(v["gsx"]["gate"], "A1");
        assert_eq!(v["own"]["tl"], "0123456789ab");
        let back: crate::frame::Frame = serde_json::from_str(&f.to_json()).unwrap();
        assert_eq!(back.gsx, Some(Some(blk("A1"))));
    }

    #[test]
    fn traffic_is_dropped_before_the_gsx_blocks() {
        use crate::frame::*;
        let list: Vec<Value> = SERVICE_IDS.iter().map(|i| svc(i, 5, "50/100 passengers")).collect();
        let names: Vec<String> = (0..400).map(|i| format!("Stand {i:03} North")).collect();
        let st = state_with(json!(list), names);
        let mut f = frame();
        f.gsx = Some(Some(block(&st, &SimExtras::default(), None)));
        f.gsx_parkings = parkings(&st);
        f.traffic = Some(
            (0..150)
                .map(|i| Traffic {
                    id: format!("t{i}"),
                    callsign: Some(format!("TST{i:03}")),
                    lat: 52.3 + i as f64 * 0.001,
                    lon: 4.7,
                    alt_ft: 3000.0,
                    gs_kt: 180.0,
                    hdg: 70.0,
                    on_ground: Some(false),
                })
                .collect(),
        );
        let before = f.gsx.clone();
        let parkings_before = f.gsx_parkings.clone();
        let f = f.fit_to_budget();
        assert!(f.to_json().len() <= MAX_FRAME_BYTES);
        assert!(f.to_json().contains("\"gsxParkings\":{\"apt\":\"EHAM\""));
        assert!(f.to_json_with_tl(Some("0123456789ab")).contains("\"gsxParkings\""));
        assert_eq!(f.gsx, before);
        assert_eq!(f.gsx_parkings, parkings_before);
        assert!(f.traffic.as_ref().is_some_and(|t| t.len() < 150), "traffic trimmed");
    }
}
