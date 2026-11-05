//! SimConnect constants, message layouts and decoding, kept free of FFI so
//! it compiles and is tested on every platform. The FFI half is `msfs.rs`
//! (Windows only).
//!
//! Layouts follow `SimConnect.h` from the MSFS SDK, which packs structs to
//! 1 byte (`#pragma pack(push, 1)`), so every read here is unaligned.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::frame::{infer_on_ground, valid_position, Own, Traffic};

pub const SIMCONNECT_DLL: &str = "SimConnect.dll";

/// The status line when the DLL is not beside the exe.
pub const MISSING_DLL_HINT: &str = "SimConnect.dll missing next to the app; reinstall the SkyNexus Connector";

/// The only place `SimConnect.dll` is ever loaded from: the folder of the
/// running exe, as an absolute path. A relative or parentless exe path gives
/// None, so the loader never falls back to the current directory or `PATH`
/// (DLL search-order hijacking).
pub fn simconnect_path(exe: &Path) -> Option<PathBuf> {
    if !exe.is_absolute() {
        return None;
    }
    let dir = exe.parent().filter(|d| !d.as_os_str().is_empty())?;
    Some(dir.join(SIMCONNECT_DLL))
}

// SIMCONNECT_RECV_ID
pub const RECV_ID_NULL: u32 = 0;
pub const RECV_ID_EXCEPTION: u32 = 1;
pub const RECV_ID_OPEN: u32 = 2;
pub const RECV_ID_QUIT: u32 = 3;
pub const RECV_ID_SIMOBJECT_DATA: u32 = 8;
pub const RECV_ID_SIMOBJECT_DATA_BYTYPE: u32 = 9;
pub const RECV_ID_SYSTEM_STATE: u32 = 15;

/// The system state that tells a flight from the menus: `dwInteger` is 1
/// while the user is in control of an aircraft, 0 in the menus and while a
/// flight loads.
pub const SYSTEM_STATE_SIM: &str = "Sim";

// SIMCONNECT_DATATYPE
pub const DATATYPE_FLOAT64: u32 = 4;
pub const DATATYPE_STRING32: u32 = 6;

// SIMCONNECT_PERIOD
pub const PERIOD_SECOND: u32 = 4;
pub const PERIOD_VISUAL_FRAME: u32 = 2;

/// Visual frames skipped between own-aircraft samples at `--hz 2`: every
/// 10th frame is 3 to 6 Hz at 30 to 60 fps, and the pacer trims it to 2 Hz.
pub const VISUAL_FRAME_INTERVAL: u32 = 10;

/// The own-aircraft request period and interval for a rate: once per
/// second at 1 Hz; every 10th visual frame above that (SimConnect has no
/// sub-second fixed period). The CHANGED flag stays on in both.
pub fn own_request_period(hz: u32) -> (u32, u32) {
    if hz <= 1 {
        (PERIOD_SECOND, 0)
    } else {
        (PERIOD_VISUAL_FRAME, VISUAL_FRAME_INTERVAL)
    }
}

/// A traffic target this close to the own aircraft is the own aircraft
/// itself (or its shadow from a pilot client): 30 m and 100 ft.
pub const OWN_ECHO_M: f64 = 30.0;
pub const OWN_ECHO_FT: f64 = 100.0;

/// Drops targets that sit on the own aircraft.
pub fn drop_own_echo(traffic: Vec<Traffic>, own: &Own) -> Vec<Traffic> {
    traffic
        .into_iter()
        .filter(|t| {
            let dlat = (t.lat - own.lat).to_radians();
            let dlon = (t.lon - own.lon).to_radians() * ((t.lat + own.lat) / 2.0).to_radians().cos();
            let metres = 6_371_000.0 * dlat.hypot(dlon);
            !(metres < OWN_ECHO_M && (t.alt_ft - own.alt_ft).abs() < OWN_ECHO_FT)
        })
        .collect()
}

// SIMCONNECT_DATA_REQUEST_FLAG
pub const DATA_REQUEST_FLAG_CHANGED: u32 = 0x1;

// SIMCONNECT_SIMOBJECT_TYPE
pub const SIMOBJECT_TYPE_AIRCRAFT: u32 = 2;
pub const SIMOBJECT_TYPE_HELICOPTER: u32 = 3;

pub const OBJECT_ID_USER: u32 = 0;
pub const UNUSED: u32 = 0xFFFF_FFFF;

/// The user aircraft's object id inside by-type answers.
pub const USER_OBJECT_IN_BYTYPE: u32 = 1;

// Our ids.
pub const DEF_OWN: u32 = 1;
pub const DEF_TRAFFIC: u32 = 2;
pub const REQ_OWN: u32 = 1;
pub const REQ_TRAFFIC_AIRCRAFT: u32 = 2;
pub const REQ_TRAFFIC_HELICOPTER: u32 = 3;
pub const REQ_SIM_STATE: u32 = 4;

/// Traffic radius in metres (the API maximum is 200 000).
pub const TRAFFIC_RADIUS_M: u32 = 80_000;
pub const TRAFFIC_EVERY: Duration = Duration::from_secs(2);

/// A datum: simvar name, units, type, epsilon for the changed-only flag.
pub struct Datum {
    pub name: &'static str,
    pub units: Option<&'static str>,
    pub datatype: u32,
    pub epsilon: f32,
}

const fn f64d(name: &'static str, units: &'static str, epsilon: f32) -> Datum {
    Datum {
        name,
        units: Some(units),
        datatype: DATATYPE_FLOAT64,
        epsilon,
    }
}

const ATC_ID: Datum = Datum {
    name: "ATC ID",
    units: None,
    datatype: DATATYPE_STRING32,
    epsilon: 0.0,
};

/// Own aircraft, in definition order: 9 doubles then a 32-byte string.
pub const OWN_DATUMS: [Datum; 10] = [
    f64d("PLANE LATITUDE", "degrees", 0.000_01),
    f64d("PLANE LONGITUDE", "degrees", 0.000_01),
    f64d("PLANE ALTITUDE", "feet", 1.0),
    f64d("GROUND VELOCITY", "knots", 0.5),
    f64d("PLANE HEADING DEGREES TRUE", "degrees", 0.5),
    f64d("GPS GROUND TRUE TRACK", "degrees", 0.5),
    f64d("VERTICAL SPEED", "feet per minute", 20.0),
    f64d("SIM ON GROUND", "bool", 0.0),
    f64d("PLANE ALT ABOVE GROUND", "feet", 1.0),
    ATC_ID,
];
pub const OWN_DATA_LEN: usize = 9 * 8 + 32;

/// Traffic, in definition order: 7 doubles then a 32-byte string.
pub const TRAFFIC_DATUMS: [Datum; 8] = [
    f64d("PLANE LATITUDE", "degrees", 0.0),
    f64d("PLANE LONGITUDE", "degrees", 0.0),
    f64d("PLANE ALTITUDE", "feet", 0.0),
    f64d("GROUND VELOCITY", "knots", 0.0),
    f64d("PLANE HEADING DEGREES TRUE", "degrees", 0.0),
    f64d("SIM ON GROUND", "bool", 0.0),
    f64d("PLANE ALT ABOVE GROUND", "feet", 0.0),
    ATC_ID,
];
pub const TRAFFIC_DATA_LEN: usize = 7 * 8 + 32;

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn f64_at(b: &[u8], off: usize) -> Option<f64> {
    Some(f64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

fn str_at(b: &[u8], off: usize, len: usize) -> Option<String> {
    let raw = b.get(off..off + len)?;
    let s: String = raw
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as char)
        .collect::<String>()
        .trim()
        .to_string();
    (!s.is_empty()).then_some(s)
}

/// `SIMCONNECT_RECV`: dwSize, dwVersion, dwID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvHeader {
    pub size: u32,
    pub version: u32,
    pub id: u32,
}

pub fn parse_header(b: &[u8]) -> Option<RecvHeader> {
    Some(RecvHeader {
        size: u32_at(b, 0)?,
        version: u32_at(b, 4)?,
        id: u32_at(b, 8)?,
    })
}

/// `SIMCONNECT_RECV_SIMOBJECT_DATA` (and `_BYTYPE`, same layout): the header,
/// then dwRequestID, dwObjectID, dwDefineID, dwFlags, dwentrynumber, dwoutof,
/// dwDefineCount, then the data block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectData<'a> {
    pub request_id: u32,
    pub object_id: u32,
    pub define_id: u32,
    pub entry: u32,
    pub out_of: u32,
    pub data: &'a [u8],
}

pub const OBJECT_DATA_OFFSET: usize = 12 + 7 * 4;

pub fn parse_object_data(b: &[u8]) -> Option<ObjectData<'_>> {
    Some(ObjectData {
        request_id: u32_at(b, 12)?,
        object_id: u32_at(b, 16)?,
        define_id: u32_at(b, 20)?,
        entry: u32_at(b, 28)?,
        out_of: u32_at(b, 32)?,
        data: b.get(OBJECT_DATA_OFFSET..)?,
    })
}

/// `SIMCONNECT_RECV_EXCEPTION`: dwException, dwSendID, dwIndex.
pub fn parse_exception(b: &[u8]) -> Option<(u32, u32, u32)> {
    Some((u32_at(b, 12)?, u32_at(b, 16)?, u32_at(b, 20)?))
}

/// `SIMCONNECT_RECV_SYSTEM_STATE`: dwRequestID, dwInteger (then fFloat and
/// szString, unused here).
pub fn parse_system_state(b: &[u8]) -> Option<(u32, u32)> {
    Some((u32_at(b, 12)?, u32_at(b, 16)?))
}

pub fn decode_own(d: &[u8]) -> Option<Own> {
    if d.len() < OWN_DATA_LEN {
        return None;
    }
    let v = |i: usize| f64_at(d, i * 8);
    let (lat, lon) = (v(0)?, v(1)?);
    // 0,0 is what an object without a position reports.
    if !valid_position(lat, lon) || (lat == 0.0 && lon == 0.0) {
        return None;
    }
    Some(Own {
        lat,
        lon,
        alt_ft: v(2)?,
        gs_kt: v(3)?,
        hdg: v(4)?,
        trk: v(5)?,
        vs_fpm: v(6)?,
        on_ground: v(7)? >= 0.5,
        callsign: str_at(d, 9 * 8, 32),
    })
}

pub fn decode_traffic(object_id: u32, d: &[u8]) -> Option<Traffic> {
    if d.len() < TRAFFIC_DATA_LEN {
        return None;
    }
    let v = |i: usize| f64_at(d, i * 8);
    let (lat, lon) = (v(0)?, v(1)?);
    // 0,0 is what an object without a position reports.
    if !valid_position(lat, lon) || (lat == 0.0 && lon == 0.0) {
        return None;
    }
    let gs_kt = v(3)?;
    // vPilot never sets SIM ON GROUND for injected aircraft, so also infer it.
    let on_ground = v(5)? >= 0.5 || infer_on_ground(v(6)?, gs_kt);
    Some(Traffic {
        id: object_id.to_string(),
        callsign: str_at(d, 7 * 8, 32),
        lat,
        lon,
        alt_ft: v(2)?,
        gs_kt,
        hdg: v(4)?,
        on_ground: Some(on_ground),
    })
}

/// Collects by-type answers (one message per object, `entry` of `out_of`)
/// into snapshots per request, and merges aircraft and helicopters.
#[derive(Debug, Default)]
pub struct TrafficAssembler {
    pending: HashMap<u32, Vec<Traffic>>,
    done: HashMap<u32, (Vec<Traffic>, Instant)>,
}

impl TrafficAssembler {
    /// Feeds one by-type message. Returns the merged snapshot when a request
    /// completed with it.
    pub fn push(&mut self, msg: &ObjectData<'_>, now: Instant) -> Option<Vec<Traffic>> {
        if msg.entry <= 1 {
            self.pending.insert(msg.request_id, Vec::new());
        }
        let list = self.pending.entry(msg.request_id).or_default();
        if msg.object_id != USER_OBJECT_IN_BYTYPE {
            if let Some(t) = decode_traffic(msg.object_id, msg.data) {
                list.push(t);
            }
        }
        if msg.out_of == 0 || msg.entry >= msg.out_of {
            let snapshot = self.pending.remove(&msg.request_id).unwrap_or_default();
            self.done.insert(msg.request_id, (snapshot, now));
            return Some(self.merged(now));
        }
        None
    }

    /// Every completed request younger than 5 s (a request with no objects
    /// may never answer, so old snapshots age out).
    pub fn merged(&self, now: Instant) -> Vec<Traffic> {
        let mut keys: Vec<&u32> = self.done.keys().collect();
        keys.sort();
        keys.into_iter()
            .filter_map(|k| self.done.get(k))
            .filter(|(_, at)| now.duration_since(*at) <= Duration::from_secs(5))
            .flat_map(|(list, _)| list.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn own_block(lat: f64, lon: f64, on_ground: f64, callsign: &str) -> Vec<u8> {
        let mut d = Vec::new();
        for v in [lat, lon, 364.0, 12.0, 249.0, 250.0, 0.0, on_ground, 0.0] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        let mut s = [0u8; 32];
        s[..callsign.len()].copy_from_slice(callsign.as_bytes());
        d.extend_from_slice(&s);
        d
    }

    pub fn traffic_block(lat: f64, gs: f64, sim_on_ground: f64, agl: f64, callsign: &str) -> Vec<u8> {
        let mut d = Vec::new();
        for v in [lat, 8.6, 3000.0, gs, 70.0, sim_on_ground, agl] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        let mut s = [0u8; 32];
        s[..callsign.len()].copy_from_slice(callsign.as_bytes());
        d.extend_from_slice(&s);
        d
    }

    /// A whole `SIMCONNECT_RECV_SIMOBJECT_DATA[_BYTYPE]` message.
    pub fn object_message(id: u32, req: u32, obj: u32, def: u32, entry: u32, out_of: u32, data: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        let size = (OBJECT_DATA_OFFSET + data.len()) as u32;
        for v in [size, 6, id, req, obj, def, 0, entry, out_of, 1] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        m.extend_from_slice(data);
        m
    }

    #[test]
    fn simconnect_is_only_resolved_beside_an_absolute_exe() {
        // temp_dir() is absolute on every platform (C:\... on Windows, /... elsewhere).
        let dir = std::env::temp_dir().join("SkyNexus Connector");
        let exe = dir.join("skynexus-connector.exe");
        let dll = simconnect_path(&exe).unwrap();
        assert!(dll.is_absolute());
        assert_eq!(dll, dir.join("SimConnect.dll"));
        assert_eq!(dll.parent(), exe.parent());

        assert_eq!(
            simconnect_path(Path::new("skynexus-connector.exe")),
            None,
            "bare name: would search"
        );
        assert_eq!(
            simconnect_path(Path::new("bin/skynexus-connector.exe")),
            None,
            "relative"
        );
        assert_eq!(simconnect_path(Path::new("")), None);
        // a filesystem root has no parent folder to look in
        let root = exe.ancestors().last().unwrap();
        assert_eq!(simconnect_path(root), None);
    }

    #[test]
    fn data_lengths_match_the_definitions() {
        let len = |ds: &[Datum]| -> usize {
            ds.iter()
                .map(|d| if d.datatype == DATATYPE_FLOAT64 { 8 } else { 32 })
                .sum()
        };
        assert_eq!(len(&OWN_DATUMS), OWN_DATA_LEN);
        assert_eq!(len(&TRAFFIC_DATUMS), TRAFFIC_DATA_LEN);
    }

    #[test]
    fn decodes_own_aircraft_messages() {
        let m = object_message(
            RECV_ID_SIMOBJECT_DATA,
            REQ_OWN,
            OBJECT_ID_USER,
            DEF_OWN,
            1,
            1,
            &own_block(50.0379, 8.5622, 1.0, "DEMO01"),
        );
        let h = parse_header(&m).unwrap();
        assert_eq!(h.id, RECV_ID_SIMOBJECT_DATA);
        assert_eq!(h.size as usize, m.len());
        let od = parse_object_data(&m).unwrap();
        assert_eq!((od.request_id, od.define_id), (REQ_OWN, DEF_OWN));
        let own = decode_own(od.data).unwrap();
        assert_eq!(own.lat, 50.0379);
        assert!(own.on_ground);
        assert_eq!(own.trk, 250.0);
        assert_eq!(own.callsign.as_deref(), Some("DEMO01"));
        assert!(decode_own(&od.data[..50]).is_none(), "short block");
        assert!(decode_own(&own_block(f64::NAN, 8.0, 0.0, "")).is_none());
        assert_eq!(decode_own(&own_block(50.0, 8.0, 0.0, "")).unwrap().callsign, None);
    }

    #[test]
    fn null_island_is_not_a_position() {
        assert!(decode_own(&own_block(0.0, 0.0, 0.0, "")).is_none());
        assert!(decode_traffic(17, &traffic_block_at(0.0, 0.0)).is_none());
        assert!(
            decode_traffic(17, &traffic_block_at(0.0, 8.6)).is_some(),
            "the equator is fine"
        );
    }

    fn traffic_block_at(lat: f64, lon: f64) -> Vec<u8> {
        let mut d = Vec::new();
        for v in [lat, lon, 3000.0, 200.0, 70.0, 0.0, 3000.0] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(&[0u8; 32]);
        d
    }

    #[test]
    fn own_echo_is_dropped() {
        let own = decode_own(&own_block(50.0, 8.6, 0.0, "")).unwrap(); // alt 364 ft
        let at = |dlat_m: f64, alt_ft: f64| Traffic {
            id: "x".into(),
            callsign: None,
            lat: 50.0 + dlat_m / 111_195.0,
            lon: 8.6,
            alt_ft,
            gs_kt: 0.0,
            hdg: 0.0,
            on_ground: None,
        };
        let kept = drop_own_echo(vec![at(10.0, 380.0), at(50.0, 364.0), at(10.0, 1000.0)], &own);
        assert_eq!(kept.len(), 2, "10 m and 16 ft away is the own aircraft");
        assert!((kept[0].lat - 50.0 - 50.0 / 111_195.0).abs() < 1e-9);
    }

    #[test]
    fn own_request_rate() {
        assert_eq!(own_request_period(1), (PERIOD_SECOND, 0));
        assert_eq!(own_request_period(2), (PERIOD_VISUAL_FRAME, VISUAL_FRAME_INTERVAL));
    }

    #[test]
    fn exception_layout() {
        let mut m = Vec::new();
        for v in [24u32, 6, RECV_ID_EXCEPTION, 7, 3, 2] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(parse_exception(&m), Some((7, 3, 2)));
    }

    #[test]
    fn system_state_layout() {
        // header, dwRequestID, dwInteger, fFloat, then szString[260]
        let mut m = Vec::new();
        for v in [24u32 + 260, 6, RECV_ID_SYSTEM_STATE, REQ_SIM_STATE, 1] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        m.extend_from_slice(&0f32.to_le_bytes());
        m.extend_from_slice(&[0u8; 260]);
        assert_eq!(parse_header(&m).unwrap().id, RECV_ID_SYSTEM_STATE);
        assert_eq!(parse_system_state(&m), Some((REQ_SIM_STATE, 1)));
        assert_eq!(parse_system_state(&m[..18]), None, "short message");
    }

    #[test]
    fn assembles_by_type_snapshots_and_skips_the_user() {
        let t0 = Instant::now();
        let mut a = TrafficAssembler::default();
        let msg = |req, obj, entry, out_of, data: Vec<u8>| {
            object_message(
                RECV_ID_SIMOBJECT_DATA_BYTYPE,
                req,
                obj,
                DEF_TRAFFIC,
                entry,
                out_of,
                &data,
            )
        };
        let m1 = msg(
            REQ_TRAFFIC_AIRCRAFT,
            1,
            1,
            3,
            traffic_block(50.0, 0.0, 1.0, 0.0, "DEMO01"),
        );
        let m2 = msg(
            REQ_TRAFFIC_AIRCRAFT,
            17,
            2,
            3,
            traffic_block(50.1, 12.0, 0.0, 3.0, "TEST11"),
        );
        let m3 = msg(
            REQ_TRAFFIC_AIRCRAFT,
            18,
            3,
            3,
            traffic_block(50.2, 250.0, 0.0, 3000.0, "TEST22"),
        );
        assert!(a.push(&parse_object_data(&m1).unwrap(), t0).is_none());
        assert!(a.push(&parse_object_data(&m2).unwrap(), t0).is_none());
        let snap = a.push(&parse_object_data(&m3).unwrap(), t0).unwrap();
        assert_eq!(snap.len(), 2, "the user (object 1) is not traffic");
        assert_eq!(snap[0].id, "17");
        assert_eq!(snap[0].on_ground, Some(true), "vPilot: inferred from AGL and speed");
        assert_eq!(snap[1].on_ground, Some(false));
        assert_eq!(snap[1].callsign.as_deref(), Some("TEST22"));

        let h = msg(
            REQ_TRAFFIC_HELICOPTER,
            30,
            1,
            1,
            traffic_block(50.3, 90.0, 0.0, 500.0, "TEST33"),
        );
        let snap = a
            .push(&parse_object_data(&h).unwrap(), t0 + Duration::from_secs(1))
            .unwrap();
        assert_eq!(snap.len(), 3, "aircraft and helicopters merged");

        // six seconds later the aircraft snapshot has aged out
        assert_eq!(a.merged(t0 + Duration::from_secs(6)).len(), 1);
    }
}
