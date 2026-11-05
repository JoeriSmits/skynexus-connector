//! What both X-Plane adapters share: the datarefs and the conversion from
//! raw dataref values to a frame.

use crate::frame::units::{MPS_TO_KT, M_TO_FT};
use crate::frame::{valid_position, Own, Traffic};

/// Own-aircraft datarefs, in the order of [`OwnRaw::set`].
pub const OWN_DATAREFS: [&str; 9] = [
    "sim/flightmodel/position/latitude",
    "sim/flightmodel/position/longitude",
    "sim/flightmodel/position/elevation",
    "sim/flightmodel/position/y_agl",
    "sim/flightmodel/position/groundspeed",
    "sim/flightmodel/position/true_psi",
    "sim/flightmodel/position/hpath",
    "sim/flightmodel/position/vh_ind_fpm",
    "sim/flightmodel/failures/onground_any",
];

/// TCAS target arrays (filled by xPilot through XPMP2, LiveTraffic and X-Plane AI).
/// Slot 0 is the user aircraft.
pub const TCAS_LAT: &str = "sim/cockpit2/tcas/targets/position/lat";
pub const TCAS_LON: &str = "sim/cockpit2/tcas/targets/position/lon";
pub const TCAS_ELE: &str = "sim/cockpit2/tcas/targets/position/ele";
pub const TCAS_PSI: &str = "sim/cockpit2/tcas/targets/position/psi";
/// Total speed in m/s, vertical component included, so it reads a little
/// high in a climb or descent. Used only when `vx`/`vz` are not available.
pub const TCAS_V: &str = "sim/cockpit2/tcas/targets/position/V_msc";
/// Horizontal velocity components (m/s, local OpenGL axes): ground speed is
/// `hypot(vx, vz)`. Looked up at connect; older sims may lack them.
pub const TCAS_VX: &str = "sim/cockpit2/tcas/targets/position/vx";
pub const TCAS_VZ: &str = "sim/cockpit2/tcas/targets/position/vz";
pub const TCAS_MODE_S: &str = "sim/cockpit2/tcas/targets/modeS_id";
pub const TCAS_FLIGHT_ID: &str = "sim/cockpit2/tcas/targets/flight_id";

/// Bytes per target in the `flight_id` byte array.
pub const FLIGHT_ID_BYTES: usize = 8;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct OwnRaw {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub elevation_m: Option<f64>,
    pub agl_m: Option<f64>,
    pub gs_mps: Option<f64>,
    pub psi: Option<f64>,
    pub hpath: Option<f64>,
    pub vs_fpm: Option<f64>,
    pub on_ground: Option<f64>,
}

impl OwnRaw {
    /// Sets the value for `OWN_DATAREFS[i]`.
    pub fn set(&mut self, i: usize, v: f64) {
        let slot = match i {
            0 => &mut self.lat,
            1 => &mut self.lon,
            2 => &mut self.elevation_m,
            3 => &mut self.agl_m,
            4 => &mut self.gs_mps,
            5 => &mut self.psi,
            6 => &mut self.hpath,
            7 => &mut self.vs_fpm,
            8 => &mut self.on_ground,
            _ => return,
        };
        *slot = Some(v);
    }

    pub fn to_own(&self) -> Option<Own> {
        let (lat, lon) = (self.lat?, self.lon?);
        if !valid_position(lat, lon) || (lat == 0.0 && lon == 0.0) {
            return None;
        }
        let gs_kt = self.gs_mps.unwrap_or(0.0) * MPS_TO_KT;
        let hdg = self.psi.unwrap_or(0.0);
        // hpath wanders while standing still; use the heading below 2 kt.
        let trk = match self.hpath {
            Some(t) if gs_kt >= 2.0 => t,
            _ => hdg,
        };
        let on_ground = match self.on_ground {
            Some(g) => g >= 0.5,
            None => self.agl_m.is_some_and(|a| a < 1.0),
        };
        Some(Own {
            lat,
            lon,
            alt_ft: self.elevation_m.unwrap_or(0.0) * M_TO_FT,
            gs_kt,
            hdg,
            trk,
            vs_fpm: self.vs_fpm.unwrap_or(0.0),
            on_ground,
            callsign: None,
        })
    }
}

/// TCAS arrays as last received.
#[derive(Debug, Default, Clone)]
pub struct TcasRaw {
    pub lat: Vec<f64>,
    pub lon: Vec<f64>,
    pub ele_m: Vec<f64>,
    pub psi: Vec<f64>,
    pub v_mps: Vec<f64>,
    pub vx: Vec<f64>,
    pub vz: Vec<f64>,
    pub mode_s: Vec<f64>,
    pub flight_id: Vec<u8>,
}

impl TcasRaw {
    pub fn has_positions(&self) -> bool {
        !self.lat.is_empty() && !self.lon.is_empty()
    }

    /// Horizontal speed from `vx`/`vz` when both arrived, else `V_msc`
    /// (which includes vertical speed).
    fn ground_speed_mps(&self, i: usize) -> f64 {
        match (self.vx.get(i), self.vz.get(i)) {
            (Some(x), Some(z)) if x.is_finite() && z.is_finite() => x.hypot(*z),
            _ => self.v_mps.get(i).copied().unwrap_or(0.0),
        }
    }

    /// Targets 1.. with a real position. Unused slots hold 9999999.9 or 0/0.
    pub fn targets(&self) -> Vec<Traffic> {
        let n = self.lat.len().min(self.lon.len());
        (1..n)
            .filter_map(|i| {
                let (lat, lon) = (self.lat[i], self.lon[i]);
                if !valid_position(lat, lon) || (lat == 0.0 && lon == 0.0) {
                    return None;
                }
                let gs_kt = self.ground_speed_mps(i) * MPS_TO_KT;
                let mode_s = self.mode_s.get(i).copied().unwrap_or(0.0) as i64;
                Some(Traffic {
                    id: if mode_s > 0 {
                        format!("{mode_s:06X}")
                    } else {
                        format!("slot{i}")
                    },
                    callsign: flight_id(&self.flight_id, i),
                    lat,
                    lon,
                    alt_ft: self.ele_m.get(i).copied().unwrap_or(0.0) * M_TO_FT,
                    gs_kt,
                    hdg: self.psi.get(i).copied().unwrap_or(0.0),
                    // XPMP2 does not fill weight_on_wheels; slow means taxiing.
                    on_ground: Some(gs_kt < 40.0),
                })
            })
            .collect()
    }
}

/// The callsign in slot `i` of the `flight_id` byte array (8 bytes per target).
pub fn flight_id(bytes: &[u8], i: usize) -> Option<String> {
    let chunk = bytes.get(i * FLIGHT_ID_BYTES..(i + 1) * FLIGHT_ID_BYTES)?;
    let s: String = chunk
        .iter()
        .take_while(|b| **b != 0)
        .filter(|b| b.is_ascii_graphic())
        .map(|b| *b as char)
        .collect();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_conversion() {
        let mut r = OwnRaw::default();
        assert!(r.to_own().is_none());
        for (i, v) in [50.0379, 8.5622, 111.0, 0.3, 0.2, 249.0, 12.0, 0.0, 1.0]
            .into_iter()
            .enumerate()
        {
            r.set(i, v);
        }
        let o = r.to_own().unwrap();
        assert!((o.alt_ft - 364.17).abs() < 0.1);
        assert!(o.on_ground);
        assert_eq!(o.trk, 249.0, "standing still: heading, not the noisy hpath");
        r.set(4, 80.0);
        r.set(8, 0.0);
        let o = r.to_own().unwrap();
        assert!((o.gs_kt - 155.5).abs() < 0.1);
        assert_eq!(o.trk, 12.0);
        assert!(!o.on_ground);
    }

    #[test]
    fn ground_speed_prefers_horizontal_components() {
        let t = TcasRaw {
            lat: vec![50.0, 50.1],
            lon: vec![8.5, 8.6],
            v_mps: vec![0.0, 110.0],
            vx: vec![0.0, 60.0],
            vz: vec![0.0, 80.0],
            ..TcasRaw::default()
        };
        assert!(
            (t.targets()[0].gs_kt - 100.0 * MPS_TO_KT).abs() < 1e-6,
            "hypot(60, 80), not V_msc"
        );
        let t = TcasRaw {
            vx: vec![],
            vz: vec![],
            ..t
        };
        assert!(
            (t.targets()[0].gs_kt - 110.0 * MPS_TO_KT).abs() < 1e-6,
            "V_msc fallback"
        );
    }

    #[test]
    fn tcas_targets_skip_empty_slots_and_read_callsigns() {
        let mut ids = vec![0u8; 64 * FLIGHT_ID_BYTES];
        ids[8..14].copy_from_slice(b"TEST11");
        ids[24..30].copy_from_slice(b"TEST33");
        let t = TcasRaw {
            lat: vec![50.0, 50.1, 9_999_999.9, 50.3, 0.0],
            lon: vec![8.5, 8.6, 9_999_999.9, 8.7, 0.0],
            ele_m: vec![100.0, 914.4, 0.0, 120.0, 0.0],
            psi: vec![0.0, 70.0, 0.0, 180.0, 0.0],
            v_mps: vec![0.0, 100.0, 0.0, 5.0, 0.0],
            vx: vec![],
            vz: vec![],
            mode_s: vec![0.0, 4_840_000.0, 0.0, 0.0, 0.0],
            flight_id: ids,
        };
        let list = t.targets();
        assert_eq!(list.len(), 2, "slot 0 is the user, slot 2 and 4 are empty");
        assert_eq!(list[0].callsign.as_deref(), Some("TEST11"));
        assert_eq!(list[0].id, "49DA40");
        assert!((list[0].alt_ft - 3000.0).abs() < 0.1);
        assert_eq!(list[0].on_ground, Some(false));
        assert_eq!(list[1].id, "slot3");
        assert_eq!(list[1].on_ground, Some(true));
    }
}
