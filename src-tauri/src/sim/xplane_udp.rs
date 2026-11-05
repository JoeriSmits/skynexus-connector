//! X-Plane 11 and 12.0 through UDP RREF on port 49000 (no plugin, no web API).
//!
//! Request, little endian, 413 bytes: `"RREF\0"`, i32 frequency (Hz, 0 stops),
//! i32 index (echoed back), 400 bytes of NUL-padded dataref path.
//! Answer: `"RREF"` plus one byte, then any number of (i32 index, f32 value).
//!
//! RREF carries numbers only, so traffic comes from the legacy multiplayer
//! slots 1 to 19 without callsigns. The connector sends from an ephemeral
//! local port and X-Plane answers to it; nothing listens on a fixed port.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use super::xplane::{OwnRaw, OWN_DATAREFS};
use super::{Sim, SimError};
use crate::frame::units::{MPS_TO_KT, M_TO_FT};
use crate::frame::{now_ms, valid_position, Frame, SimKind, Traffic};

pub const DEFAULT_PORT: u16 = 49000;
pub const REQUEST_LEN: usize = 413;
const PATH_LEN: usize = 400;
/// Multiplayer slots X-Plane mirrors (the nearest 19 targets).
pub const MP_SLOTS: usize = 19;
const MP_FIELDS: [&str; 6] = ["lat", "lon", "el", "psi", "v_x", "v_z"];
const MP_BASE: i32 = 100;
const FREQ_HZ: i32 = 5;
const SILENCE_LIMIT: Duration = Duration::from_secs(10);

pub fn rref_request(freq: i32, index: i32, path: &str) -> [u8; REQUEST_LEN] {
    let mut buf = [0u8; REQUEST_LEN];
    buf[..5].copy_from_slice(b"RREF\0");
    buf[5..9].copy_from_slice(&freq.to_le_bytes());
    buf[9..13].copy_from_slice(&index.to_le_bytes());
    let p = path.as_bytes();
    let n = p.len().min(PATH_LEN - 1);
    buf[13..13 + n].copy_from_slice(&p[..n]);
    buf
}

/// The (index, value) pairs of one RREF answer, or None if it is not one.
pub fn parse_rref(packet: &[u8]) -> Option<Vec<(i32, f32)>> {
    if packet.len() < 5 || &packet[..4] != b"RREF" {
        return None;
    }
    let body = &packet[5..];
    Some(
        body.as_chunks::<8>()
            .0
            .iter()
            .map(|c| {
                (
                    i32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })
            .collect(),
    )
}

/// Every subscription as (index, path).
pub fn subscriptions() -> Vec<(i32, String)> {
    let mut subs: Vec<(i32, String)> = OWN_DATAREFS
        .iter()
        .enumerate()
        .map(|(i, p)| (i as i32, p.to_string()))
        .collect();
    for slot in 1..=MP_SLOTS {
        for (f, field) in MP_FIELDS.iter().enumerate() {
            subs.push((
                MP_BASE + (slot as i32) * 10 + f as i32,
                format!("sim/multiplayer/position/plane{slot}_{field}"),
            ));
        }
    }
    subs
}

#[derive(Debug, Default, Clone)]
pub struct UdpState {
    pub own: OwnRaw,
    /// [slot][field] for slots 1..=19.
    pub mp: [[Option<f64>; 6]; MP_SLOTS + 1],
}

impl UdpState {
    pub fn apply(&mut self, pairs: &[(i32, f32)]) -> bool {
        let mut any = false;
        for &(index, value) in pairs {
            let v = f64::from(value);
            if (0..OWN_DATAREFS.len() as i32).contains(&index) {
                self.own.set(index as usize, v);
                any = true;
            } else if index >= MP_BASE + 10 {
                let slot = ((index - MP_BASE) / 10) as usize;
                let field = ((index - MP_BASE) % 10) as usize;
                if (1..=MP_SLOTS).contains(&slot) && field < MP_FIELDS.len() {
                    self.mp[slot][field] = Some(v);
                    any = true;
                }
            }
        }
        any
    }

    /// Multiplayer slots with a real position that is not the user's own.
    pub fn traffic(&self) -> Vec<Traffic> {
        let own = self.own.to_own();
        (1..=MP_SLOTS)
            .filter_map(|slot| {
                let f = &self.mp[slot];
                let (lat, lon) = (f[0]?, f[1]?);
                if !valid_position(lat, lon) || (lat == 0.0 && lon == 0.0) {
                    return None;
                }
                if let Some(o) = &own {
                    // Empty slots can sit on the user aircraft.
                    if (o.lat - lat).abs() < 1e-5 && (o.lon - lon).abs() < 1e-5 {
                        return None;
                    }
                }
                let gs_kt = f[4].unwrap_or(0.0).hypot(f[5].unwrap_or(0.0)) * MPS_TO_KT;
                Some(Traffic {
                    id: format!("mp{slot}"),
                    callsign: None,
                    lat,
                    lon,
                    alt_ft: f[2].unwrap_or(0.0) * M_TO_FT,
                    gs_kt,
                    hdg: f[3].unwrap_or(0.0),
                    on_ground: Some(gs_kt < 40.0),
                })
            })
            .collect()
    }
}

pub struct XplaneUdp {
    socket: UdpSocket,
    target: SocketAddr,
    state: UdpState,
    last_packet: Instant,
    traffic_due: Instant,
}

impl XplaneUdp {
    pub fn connect(port: u16) -> Result<Self, String> {
        let socket = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let target: SocketAddr = ([127, 0, 0, 1], port).into();
        // Subscribe to latitude only, and wait briefly for an answer.
        socket
            .send_to(&rref_request(FREQ_HZ, 0, OWN_DATAREFS[0]), target)
            .map_err(|e| e.to_string())?;
        socket
            .set_read_timeout(Some(Duration::from_millis(1200)))
            .map_err(|e| e.to_string())?;
        let mut buf = [0u8; 2048];
        let got = loop {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) if from.port() == port => {
                    if parse_rref(&buf[..n]).is_some() {
                        break true;
                    }
                }
                Ok(_) => continue,
                Err(_) => break false,
            }
        };
        if !got {
            let _ = socket.send_to(&rref_request(0, 0, OWN_DATAREFS[0]), target);
            return Err("not running".into());
        }
        for (index, path) in subscriptions() {
            socket
                .send_to(&rref_request(FREQ_HZ, index, &path), target)
                .map_err(|e| e.to_string())?;
        }
        socket.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(XplaneUdp {
            socket,
            target,
            state: UdpState::default(),
            last_packet: Instant::now(),
            traffic_due: Instant::now(),
        })
    }
}

impl Drop for XplaneUdp {
    fn drop(&mut self) {
        for (index, path) in subscriptions() {
            let _ = self.socket.send_to(&rref_request(0, index, &path), self.target);
        }
    }
}

impl Sim for XplaneUdp {
    fn kind(&self) -> SimKind {
        SimKind::Xplane
    }

    fn describe(&self) -> String {
        "X-Plane (UDP)".into()
    }

    fn poll(&mut self) -> Result<Option<Frame>, SimError> {
        let mut buf = [0u8; 4096];
        let mut changed = false;
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) if from == self.target => {
                    if let Some(pairs) = parse_rref(&buf[..n]) {
                        self.last_packet = Instant::now();
                        changed |= self.state.apply(&pairs);
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // Windows reports ICMP port unreachable as ConnectionReset.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return Err(SimError::Disconnected),
                Err(e) => return Err(SimError::Other(e.to_string())),
            }
        }
        if self.last_packet.elapsed() > SILENCE_LIMIT {
            return Err(SimError::Disconnected);
        }
        if !changed {
            return Ok(None);
        }
        let Some(own) = self.state.own.to_own() else {
            return Ok(None);
        };
        let mut f = Frame::new(SimKind::Xplane, now_ms(), own);
        if Instant::now() >= self.traffic_due {
            f.traffic = Some(self.state.traffic());
            self.traffic_due = Instant::now() + Duration::from_secs(2);
        }
        Ok(Some(f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout() {
        let r = rref_request(5, 42, "sim/flightmodel/position/latitude");
        assert_eq!(r.len(), 413);
        assert_eq!(&r[..5], b"RREF\0");
        assert_eq!(i32::from_le_bytes(r[5..9].try_into().unwrap()), 5);
        assert_eq!(i32::from_le_bytes(r[9..13].try_into().unwrap()), 42);
        assert_eq!(&r[13..46], b"sim/flightmodel/position/latitude");
        assert!(r[46..].iter().all(|b| *b == 0));
    }

    fn packet(pairs: &[(i32, f32)]) -> Vec<u8> {
        let mut p = b"RREF,".to_vec();
        for (i, v) in pairs {
            p.extend_from_slice(&i.to_le_bytes());
            p.extend_from_slice(&v.to_le_bytes());
        }
        p
    }

    #[test]
    fn answer_parsing() {
        let pairs = parse_rref(&packet(&[(0, 50.0379), (1, 8.5622)])).unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[1].0, 1);
        assert!((pairs[0].1 - 50.0379).abs() < 1e-4);
        assert!(parse_rref(b"DATA*....").is_none());
        // a trailing partial pair is ignored
        let mut p = packet(&[(2, 1.0)]);
        p.extend_from_slice(&[1, 2, 3]);
        assert_eq!(parse_rref(&p).unwrap().len(), 1);
    }

    #[test]
    fn subscriptions_cover_own_and_19_slots() {
        let subs = subscriptions();
        assert_eq!(subs.len(), 9 + 19 * 6);
        assert!(subs
            .iter()
            .any(|(i, p)| *i == 190 + 5 && p == "sim/multiplayer/position/plane9_v_z"));
        let mut idx: Vec<i32> = subs.iter().map(|s| s.0).collect();
        idx.sort();
        idx.dedup();
        assert_eq!(idx.len(), subs.len(), "indices are unique");
    }

    #[test]
    fn state_builds_own_and_traffic() {
        let mut s = UdpState::default();
        assert!(s.apply(&[(0, 50.0379), (1, 8.5622), (2, 111.0), (4, 70.0), (5, 249.0), (8, 0.0)]));
        // slot 1: real target; slot 2: parked on the user; slot 3: empty 0/0
        s.apply(&[
            (110, 50.2),
            (111, 8.9),
            (112, 914.4),
            (113, 70.0),
            (114, 60.0),
            (115, 80.0),
        ]);
        s.apply(&[(120, 50.0379), (121, 8.5622)]);
        s.apply(&[(130, 0.0), (131, 0.0)]);
        let own = s.own.to_own().unwrap();
        assert!((own.gs_kt - 136.1).abs() < 0.1);
        let t = s.traffic();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].id, "mp1");
        assert!((t[0].gs_kt - 194.4).abs() < 0.1, "hypot(60, 80) m/s");
        assert!((t[0].alt_ft - 3000.0).abs() < 0.5);
        assert!(!s.apply(&[(9999, 1.0)]));
    }

    #[test]
    fn connect_fails_without_xplane() {
        // Nothing answers on this port.
        let unused = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(XplaneUdp::connect(unused).is_err());
    }
}
