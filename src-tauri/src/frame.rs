//! The relay frame schema, version 1.
//!
//! `{v: 1, t: ms, sim: "msfs"|"xplane"|"replay", own: {...}, traffic?: [...]}`
//!
//! Field names are camelCase on the wire. The relay forwards only frames that
//! pass its guard, so everything here keeps to finite numbers and sane ranges.

use serde::{Deserialize, Serialize};

use crate::gsx::block::{GsxBlock, GsxParkings};

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
    /// Total fuel on board in kilograms, when the sim reports it. Older
    /// recordings and sims without it leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuel_kg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    /// The short hash (12 hex) of the TechLog snapshot SkyNexus last
    /// accepted, so open EFB tabs refetch only when TechLog changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tl: Option<String>,
}

impl Own {
    /// Whether the aircraft moved or changed state since `other`. Fuel and
    /// `tl` are left out on purpose: an APU burns fuel while parked, and that
    /// alone must not lift a parked aircraft off its 5 s heartbeat; a new
    /// TechLog hash is not motion either. Both ride along on every frame
    /// that is sent anyway.
    pub fn same_motion(&self, other: &Own) -> bool {
        self.lat == other.lat
            && self.lon == other.lon
            && self.alt_ft == other.alt_ft
            && self.gs_kt == other.gs_kt
            && self.hdg == other.hdg
            && self.trk == other.trk
            && self.vs_fpm == other.vs_fpm
            && self.on_ground == other.on_ground
            && self.callsign == other.callsign
    }
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
    /// GSX block: absent, `null` (GSX went away) or the block.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::gsx::block::double_option"
    )]
    pub gsx: Option<Option<GsxBlock>>,
    #[serde(rename = "gsxParkings", default, skip_serializing_if = "Option::is_none")]
    pub gsx_parkings: Option<GsxParkings>,
}

impl Frame {
    pub fn new(sim: SimKind, t: u64, own: Own) -> Self {
        Frame {
            v: FRAME_VERSION,
            t,
            sim,
            own,
            traffic: None,
            gsx: None,
            gsx_parkings: None,
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

    /// Rounds numbers to what the map can show, which keeps frames small.
    /// Own position keeps 6 decimals of a degree (about 0.1 m), so the EFB's
    /// docking board can say STOP within 1 m on real positions; traffic keeps
    /// 5 (about 1 m), which is all the map draws.
    pub fn rounded(mut self) -> Self {
        let o = &mut self.own;
        o.lat = round_to(o.lat, OWN_POSITION_DECIMALS);
        o.lon = round_to(o.lon, OWN_POSITION_DECIMALS);
        o.alt_ft = o.alt_ft.round();
        o.gs_kt = round_to(o.gs_kt, 1);
        o.hdg = round_to(norm_deg(o.hdg), 1);
        o.trk = round_to(norm_deg(o.trk), 1);
        o.vs_fpm = o.vs_fpm.round();
        // Whole kilograms; a value the relay would not take is dropped, never
        // the frame (fuel is optional on the wire).
        o.fuel_kg = o.fuel_kg.filter(|f| f.is_finite() && *f >= 0.0).map(f64::round);
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

    /// The JSON with `own.tl` set to `tl`, without copying the frame. A frame
    /// that already carries its own `tl` keeps it.
    pub fn to_json_with_tl(&self, tl: Option<&str>) -> String {
        if tl.is_none() || self.own.tl.is_some() {
            return self.to_json();
        }
        #[derive(Serialize)]
        struct OwnWithTl<'a> {
            #[serde(flatten)]
            own: &'a Own,
            tl: Option<&'a str>,
        }
        #[derive(Serialize)]
        struct View<'a> {
            v: u8,
            t: u64,
            sim: SimKind,
            own: OwnWithTl<'a>,
            #[serde(skip_serializing_if = "Option::is_none")]
            traffic: &'a Option<Vec<Traffic>>,
            #[serde(
                skip_serializing_if = "Option::is_none",
                serialize_with = "crate::gsx::block::double_option::serialize"
            )]
            gsx: &'a Option<Option<GsxBlock>>,
            #[serde(rename = "gsxParkings", skip_serializing_if = "Option::is_none")]
            gsx_parkings: &'a Option<GsxParkings>,
        }
        let view = View {
            v: self.v,
            t: self.t,
            sim: self.sim,
            own: OwnWithTl { own: &self.own, tl },
            traffic: &self.traffic,
            gsx: &self.gsx,
            gsx_parkings: &self.gsx_parkings,
        };
        serde_json::to_string(&view).expect("frame serialises")
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

/// Own-aircraft latitude and longitude decimals: 0.11 m of latitude.
pub const OWN_POSITION_DECIMALS: i32 = 6;

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
    pub const LB_TO_KG: f64 = 0.453_592_37;
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
            fuel_kg: None,
            callsign: Some("SNX101".into()),
            tl: None,
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
    fn fuel_goes_out_as_whole_kilograms_and_bad_fuel_is_dropped() {
        let mut o = own();
        o.fuel_kg = Some(12_345.6);
        let f = Frame::new(SimKind::Msfs, 1, o.clone()).rounded();
        assert!(f.to_json().contains("\"fuelKg\":12346.0"));
        assert!(f.is_valid());
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            o.fuel_kg = Some(bad);
            let f = Frame::new(SimKind::Msfs, 1, o.clone()).rounded();
            assert_eq!(f.own.fuel_kg, None, "{bad} is dropped");
            assert!(f.is_valid(), "the frame survives {bad}");
            assert!(!f.to_json().contains("fuelKg"));
        }
    }

    #[test]
    fn tl_is_sent_when_set_and_is_not_motion() {
        let mut o = own();
        assert!(!Frame::new(SimKind::Msfs, 1, o.clone()).to_json().contains("\"tl\""));
        let before = o.clone();
        o.tl = Some("0123456789ab".into());
        assert!(Frame::new(SimKind::Msfs, 1, o.clone())
            .to_json()
            .contains("\"tl\":\"0123456789ab\""));
        assert!(o.same_motion(&before), "a new TechLog hash alone is not motion");
    }

    #[test]
    fn with_tl_matches_a_frame_that_carries_it() {
        let mut f = Frame::new(SimKind::Msfs, 1, own());
        f.traffic = Some(vec![traffic(1, 0.1)]);
        let json = f.to_json_with_tl(Some("0123456789ab"));
        let mut with = f.clone();
        with.own.tl = Some("0123456789ab".into());
        assert_eq!(serde_json::from_str::<Frame>(&json).unwrap(), with);
        assert_eq!(f.to_json_with_tl(None), f.to_json());
    }

    #[test]
    fn a_frame_without_fuel_still_parses() {
        let json = r#"{"v":1,"t":1,"sim":"replay","own":{"lat":50.0,"lon":8.0,"altFt":364,"gsKt":0,"hdg":0,"trk":0,"vsFpm":0,"onGround":true}}"#;
        let f: Frame = serde_json::from_str(json).unwrap();
        assert_eq!(f.own.fuel_kg, None);
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
        o.lon = 8.987_654_321;
        let mut f = Frame::new(SimKind::Replay, 1, o);
        f.traffic = Some(vec![traffic(1, 0.123_456_789)]);
        let f = f.rounded();
        assert_eq!(f.own.hdg, 350.0);
        assert_eq!(f.own.trk, 0.0);
        assert_eq!(f.own.lat, 50.123_457, "own: 6 decimals, about 0.1 m");
        assert_eq!(f.own.lon, 8.987_654);
        let t = &f.traffic.unwrap()[0];
        assert_eq!(t.lat, 50.123_46, "traffic: 5 decimals");
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
