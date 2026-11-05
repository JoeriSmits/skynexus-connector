//! The relay frame schema, version 1.
//!
//! `{v: 1, t: ms, sim: "msfs"|"xplane"|"replay", own: {...}, traffic?: [...]}`
//!
//! Field names are camelCase on the wire. The relay forwards only frames that
//! pass its guard, so everything here keeps to finite numbers and sane ranges.

use serde::{Deserialize, Serialize};

/// The schema version this connector speaks.
pub const FRAME_VERSION: u8 = 1;

/// The relay refuses frames above 16 KB. Stay under it with some headroom.
pub const MAX_FRAME_BYTES: usize = 15 * 1024;

/// The relay rejects a frame with more than 150 targets; the byte budget
/// usually trims further.
pub const MAX_TRAFFIC: usize = 150;

/// The relay guard's string limits, both printable ASCII after trimming:
/// callsign 1..16 (otherwise the relay drops the field), traffic id 1..64
/// (otherwise the relay rejects the whole frame).
pub const MAX_CALLSIGN: usize = 16;
pub const MAX_ID: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SimKind {
    Msfs,
    Xplane,
    Replay,
}

impl SimKind {
    pub fn label(self) -> &'static str {
        match self {
            SimKind::Msfs => "MSFS",
            SimKind::Xplane => "X-Plane",
            SimKind::Replay => "Replay",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Own {
    pub lat: f64,
    pub lon: f64,
    /// Feet above mean sea level.
    pub alt_ft: f64,
    /// Ground speed in knots.
    pub gs_kt: f64,
    /// True heading, degrees 0..360.
    pub hdg: f64,
    /// True track over the ground, degrees 0..360.
    pub trk: f64,
    /// Vertical speed, feet per minute.
    pub vs_fpm: f64,
    pub on_ground: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Traffic {
    /// Stable within one sim session (SimConnect object id, TCAS slot or mode S id).
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub alt_ft: f64,
    pub gs_kt: f64,
    pub hdg: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_ground: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub v: u8,
    /// Unix time in milliseconds when the frame was sampled.
    pub t: u64,
    pub sim: SimKind,
    pub own: Own,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic: Option<Vec<Traffic>>,
}

impl Frame {
    pub fn new(sim: SimKind, t: u64, own: Own) -> Self {
        Frame {
            v: FRAME_VERSION,
            t,
            sim,
            own,
            traffic: None,
        }
    }

    /// Whether every number is finite and inside its range. Frames that fail
    /// are dropped before they reach the relay, which would refuse them anyway.
    pub fn is_valid(&self) -> bool {
        valid_position(self.own.lat, self.own.lon)
            && [
                self.own.alt_ft,
                self.own.gs_kt,
                self.own.hdg,
                self.own.trk,
                self.own.vs_fpm,
            ]
            .iter()
            .all(|n| n.is_finite())
            && self.traffic.as_ref().is_none_or(|list| {
                list.iter().all(|t| {
                    valid_position(t.lat, t.lon) && t.alt_ft.is_finite() && t.gs_kt.is_finite() && t.hdg.is_finite()
                })
            })
    }

    /// Rounds numbers to what the map can show, which keeps frames small:
    /// 5 decimals of a degree is about 1 m.
    pub fn rounded(mut self) -> Self {
        let o = &mut self.own;
        o.lat = round_to(o.lat, 5);
        o.lon = round_to(o.lon, 5);
        o.alt_ft = o.alt_ft.round();
        o.gs_kt = round_to(o.gs_kt, 1);
        o.hdg = round_to(norm_deg(o.hdg), 1);
        o.trk = round_to(norm_deg(o.trk), 1);
        o.vs_fpm = o.vs_fpm.round();
        if let Some(list) = self.traffic.as_mut() {
            for t in list.iter_mut() {
                t.lat = round_to(t.lat, 5);
                t.lon = round_to(t.lon, 5);
                t.alt_ft = t.alt_ft.round();
                t.gs_kt = t.gs_kt.round();
                t.hdg = round_to(norm_deg(t.hdg), 0);
            }
        }
        self
    }

    /// Keeps the frame inside the relay's guard: string lengths, and the
    /// size limit by dropping the farthest traffic first. Own position is
    /// never dropped.
    pub fn fit_to_budget(mut self) -> Self {
        let (lat, lon) = (self.own.lat, self.own.lon);
        self.own.callsign = clean_callsign(self.own.callsign.take());
        if let Some(list) = self.traffic.as_mut() {
            for t in list.iter_mut() {
                t.callsign = clean_callsign(t.callsign.take());
                t.id = clean_id(&t.id).unwrap_or_default();
            }
            list.retain(|t| valid_position(t.lat, t.lon) && !t.id.is_empty());
            list.sort_by(|a, b| approx_dist2(lat, lon, a.lat, a.lon).total_cmp(&approx_dist2(lat, lon, b.lat, b.lon)));
            list.truncate(MAX_TRAFFIC);
        }
        while self.to_json().len() > MAX_FRAME_BYTES {
            match self.traffic.as_mut() {
                Some(list) if !list.is_empty() => {
                    let drop = (list.len() / 4).max(1);
                    list.truncate(list.len() - drop);
                }
                _ => break,
            }
        }
        self
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("frame serialises")
    }
}

fn printable_ascii(c: char) -> bool {
    (' '..='~').contains(&c)
}

/// A callsign the relay keeps: trimmed, printable ASCII, 1..16 characters.
/// Anything else (an accented AI livery name, a control character) is
/// dropped rather than mangled; longer ones are cut to 16.
fn clean_callsign(s: Option<String>) -> Option<String> {
    let s = s?;
    let s = s.trim();
    if s.is_empty() || !s.chars().all(printable_ascii) {
        return None;
    }
    Some(s.chars().take(MAX_CALLSIGN).collect::<String>().trim_end().to_string())
}

/// A target id the relay accepts: trimmed, printable ASCII, 1..64 characters.
/// The adapters only make ASCII ids; anything else is dropped with its target.
fn clean_id(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty() && s.len() <= MAX_ID && s.chars().all(printable_ascii)).then(|| s.to_string())
}

pub fn valid_position(lat: f64, lon: f64) -> bool {
    lat.is_finite() && lon.is_finite() && (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)
}

/// Degrees into 0..360.
pub fn norm_deg(d: f64) -> f64 {
    let r = d.rem_euclid(360.0);
    if r >= 360.0 {
        0.0
    } else {
        r
    }
}

fn round_to(n: f64, decimals: i32) -> f64 {
    let f = 10f64.powi(decimals);
    (n * f).round() / f
}

/// Squared distance on an equirectangular projection, for ordering only.
fn approx_dist2(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    // Wrap across the antimeridian: 179.9 and -179.9 are 0.2 degrees apart.
    let dlon = (lon2 - lon1 + 540.0).rem_euclid(360.0) - 180.0;
    let x = dlon * ((lat1 + lat2) / 2.0).to_radians().cos();
    let y = lat2 - lat1;
    x * x + y * y
}

/// Unit conversions shared by the sim adapters.
pub mod units {
    pub const M_TO_FT: f64 = 3.280_839_895;
    pub const MPS_TO_KT: f64 = 1.943_844_492;
}

/// Ground state for traffic whose own on-ground flag is not trustworthy
/// (vPilot and xPilot injected aircraft leave it unset).
pub fn infer_on_ground(agl_ft: f64, gs_kt: f64) -> bool {
    agl_ft.is_finite() && gs_kt.is_finite() && agl_ft < 10.0 && gs_kt < 60.0
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own() -> Own {
        Own {
            lat: 50.033_33,
            lon: 8.570_55,
            alt_ft: 364.0,
            gs_kt: 0.0,
            hdg: 250.0,
            trk: 250.0,
            vs_fpm: 0.0,
            on_ground: true,
            callsign: Some("SNX101".into()),
        }
    }

    fn traffic(i: usize, dlat: f64) -> Traffic {
        Traffic {
            id: format!("t{i}"),
            callsign: Some(format!("TST{i:03}")),
            lat: 50.0 + dlat,
            lon: 8.5,
            alt_ft: 3000.0,
            gs_kt: 180.0,
            hdg: 70.0,
            on_ground: Some(false),
        }
    }

    #[test]
    fn serialises_to_the_task2_schema() {
        let mut f = Frame::new(SimKind::Msfs, 1_762_300_000_000, own());
        f.traffic = Some(vec![traffic(1, 0.1)]);
        let v: serde_json::Value = serde_json::from_str(&f.to_json()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["t"], 1_762_300_000_000u64);
        assert_eq!(v["sim"], "msfs");
        let o = &v["own"];
        for key in [
            "lat", "lon", "altFt", "gsKt", "hdg", "trk", "vsFpm", "onGround", "callsign",
        ] {
            assert!(o.get(key).is_some(), "own.{key} missing");
        }
        let t = &v["traffic"][0];
        for key in ["id", "callsign", "lat", "lon", "altFt", "gsKt", "hdg", "onGround"] {
            assert!(t.get(key).is_some(), "traffic.{key} missing");
        }
    }

    #[test]
    fn optional_fields_are_omitted_and_round_trip() {
        let mut o = own();
        o.callsign = None;
        let f = Frame::new(SimKind::Xplane, 1, o);
        let json = f.to_json();
        assert!(!json.contains("callsign"));
        assert!(!json.contains("traffic"));
        assert!(json.contains("\"sim\":\"xplane\""));
        let back: Frame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn validity_rejects_nan_and_out_of_range() {
        let mut f = Frame::new(SimKind::Replay, 1, own());
        assert!(f.is_valid());
        f.own.lat = 91.0;
        assert!(!f.is_valid());
        f.own.lat = 50.0;
        f.own.vs_fpm = f64::NAN;
        assert!(!f.is_valid());
        f.own.vs_fpm = 0.0;
        f.traffic = Some(vec![Traffic {
            lon: 200.0,
            ..traffic(1, 0.0)
        }]);
        assert!(!f.is_valid());
    }

    #[test]
    fn rounding_normalises_headings() {
        let mut o = own();
        o.hdg = -10.04;
        o.trk = 360.0;
        o.lat = 50.123_456_789;
        let f = Frame::new(SimKind::Replay, 1, o).rounded();
        assert_eq!(f.own.hdg, 350.0);
        assert_eq!(f.own.trk, 0.0);
        assert_eq!(f.own.lat, 50.123_46);
    }

    #[test]
    fn budget_keeps_the_nearest_traffic_under_the_limit() {
        let mut f = Frame::new(SimKind::Msfs, 1, own());
        f.traffic = Some((0..400).map(|i| traffic(i, i as f64 * 0.001)).collect());
        let f = f.fit_to_budget();
        let list = f.traffic.as_ref().unwrap();
        assert!(list.len() <= MAX_TRAFFIC);
        assert!(f.to_json().len() <= MAX_FRAME_BYTES);
        // own is at 50.033, so the slot 33 target (50.033) is nearest
        assert_eq!(list[0].id, "t33");
    }

    #[test]
    fn strings_fit_the_relay_guard() {
        let mut o = own();
        o.callsign = Some("  A-VERY-LONG-REGISTRATION-NAME ".into());
        let mut f = Frame::new(SimKind::Msfs, 1, o);
        f.traffic = Some(vec![
            Traffic {
                callsign: Some("".into()),
                ..traffic(1, 0.0)
            },
            Traffic {
                id: "   ".into(),
                ..traffic(2, 0.0)
            },
        ]);
        let f = f.fit_to_budget();
        assert_eq!(f.own.callsign.as_deref(), Some("A-VERY-LONG-REGI"));
        let list = f.traffic.unwrap();
        assert_eq!(list.len(), 1, "an empty id is dropped");
        assert_eq!(list[0].callsign, None, "an empty callsign is omitted");

        let mut f = Frame::new(SimKind::Msfs, 1, own());
        f.traffic = Some(vec![
            Traffic {
                callsign: Some("D-ÉCHO".into()),
                ..traffic(1, 0.0)
            },
            Traffic {
                callsign: Some("TEST\n1".into()),
                ..traffic(2, 0.0)
            },
            Traffic {
                id: "objé".into(),
                ..traffic(3, 0.0)
            },
            Traffic {
                id: "a\tb".into(),
                ..traffic(4, 0.0)
            },
            Traffic {
                id: "x".repeat(65),
                ..traffic(5, 0.0)
            },
            Traffic {
                id: " 42 ".into(),
                callsign: Some(" TEST42 ".into()),
                ..traffic(6, 0.0)
            },
        ]);
        let list = f.fit_to_budget().traffic.unwrap();
        let ids: Vec<&str> = list.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["t1", "t2", "42"],
            "non-ASCII, control and over-long ids drop their target"
        );
        assert_eq!(list[0].callsign, None, "a non-ASCII callsign is dropped, not mangled");
        assert_eq!(list[1].callsign, None, "a control character drops the callsign");
        assert_eq!(list[2].callsign.as_deref(), Some("TEST42"));
    }

    #[test]
    fn nearest_sort_wraps_the_antimeridian() {
        let mut o = own();
        o.lat = -17.0;
        o.lon = 179.9;
        let mut f = Frame::new(SimKind::Msfs, 1, o);
        let at = |id: &str, lon: f64| Traffic {
            id: id.into(),
            lat: -17.0,
            lon,
            ..traffic(0, 0.0)
        };
        f.traffic = Some(vec![at("far", 178.0), at("near", -179.9)]);
        let list = f.fit_to_budget().traffic.unwrap();
        assert_eq!(
            list[0].id, "near",
            "0.2 degrees across the date line beats 1.9 the other way"
        );
        assert!((approx_dist2(0.0, 179.9, 0.0, -179.9) - 0.04).abs() < 1e-9);
    }

    #[test]
    fn ground_inference() {
        assert!(infer_on_ground(2.0, 12.0));
        assert!(!infer_on_ground(2.0, 140.0));
        assert!(!infer_on_ground(400.0, 20.0));
        assert!(!infer_on_ground(f64::NAN, 0.0));
    }
}
