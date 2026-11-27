//! Simulator adapters and the driver that paces their output into frames.
//!
//! Auto-detect order: MSFS (SimConnect, Windows only), the X-Plane 12.1.1+
//! web API, then X-Plane UDP RREF (11 and 12.0). `--replay <file>` skips
//! detection. Detection only tries to connect; it never scans processes,
//! and nothing is installed in any sim.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::frame::{now_ms, Frame, Own, SimKind, Traffic};
use rate::{interval_for, RateSelector};

#[cfg(windows)]
pub mod msfs;
pub mod msfs_proto;
pub mod rate;
pub mod replay;
pub mod xplane;
pub mod xplane_udp;
pub mod xplane_web;

#[derive(Debug, thiserror::Error)]
pub enum SimError {
    #[error("the simulator closed the connection")]
    Disconnected,
    #[error("the replay finished")]
    Finished,
    #[error("{0}")]
    Other(String),
}

/// One simulator connection. `poll` never blocks for long: it drains what
/// the sim sent since the last call and returns the newest state, if any.
/// A returned frame without `traffic` means "no new traffic snapshot".
pub trait Sim: Send {
    fn kind(&self) -> SimKind;
    fn describe(&self) -> String {
        self.kind().label().to_string()
    }
    fn poll(&mut self) -> Result<Option<Frame>, SimError>;
    /// False while the sim runs but has no flight loaded (menus, loading):
    /// connected, with nothing to stream. Only MSFS can tell.
    fn flying(&self) -> bool {
        true
    }
    /// The loaded aircraft's title, for the TechLog presence check. Only
    /// MSFS has one (its `TITLE`); None outside a flight.
    fn aircraft_title(&self) -> Option<String> {
        None
    }
    /// GSX L:vars; all None unless MSFS has read them.
    fn extras(&self) -> SimExtras {
        SimExtras::default()
    }
}

/// What the sim reports besides the own ship: GSX Pro's L:vars, read by
/// MSFS every 5 s. None for a value that is unreadable (X-Plane, replays,
/// a refused definition).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SimExtras {
    pub couatl_started: Option<bool>,
    pub pax_board: Option<u32>,
    pub pax_total: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum Source {
    /// Detect a sim; `hz` is the own-aircraft rate asked of it.
    Auto {
        hz: u32,
    },
    Replay {
        path: PathBuf,
        looped: bool,
        speed: f64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SimState {
    Searching,
    /// Searching, but something needs fixing first (for example a missing SimConnect.dll).
    Unavailable {
        reason: String,
    },
    Connected {
        kind: SimKind,
        detail: String,
    },
    /// Connected, but the sim is in its menus or loading a flight.
    NoFlight {
        kind: SimKind,
    },
    Finished,
}

/// Paces frames: own position when it changes, at most every 250 ms while
/// taxiing and docking and at the air interval otherwise (see [`rate`]), a
/// heartbeat while parked so the map does not go stale, and traffic at most
/// every 2 s.
pub struct Pacer {
    /// The air interval (`--hz`); the ground interval follows from it.
    air_interval: Duration,
    rate: RateSelector,
    heartbeat: Duration,
    traffic_every: Duration,
    traffic_max_age: Duration,
    kind: SimKind,
    own: Option<Own>,
    traffic: Option<(Vec<Traffic>, Instant)>,
    last_sent_at: Option<Instant>,
    last_sent_own: Option<Own>,
    last_traffic_at: Option<Instant>,
}

impl Pacer {
    pub fn new(air_interval: Duration) -> Self {
        Pacer {
            air_interval,
            rate: RateSelector::default(),
            heartbeat: Duration::from_secs(5),
            traffic_every: Duration::from_secs(2),
            traffic_max_age: Duration::from_secs(5),
            kind: SimKind::Replay,
            own: None,
            traffic: None,
            last_sent_at: None,
            last_sent_own: None,
            last_traffic_at: None,
        }
    }

    pub fn reset(&mut self) {
        *self = Pacer::new(self.air_interval);
    }

    /// The time between own-aircraft frames right now.
    pub fn interval(&self) -> Duration {
        interval_for(self.rate.mode(), self.air_interval)
    }

    pub fn ingest(&mut self, f: Frame, now: Instant) {
        let f = f.rounded();
        self.kind = f.sim;
        self.rate.update(&f.own);
        self.own = Some(f.own);
        if let Some(t) = f.traffic {
            self.traffic = Some((t, now));
        }
    }

    pub fn due(&mut self, now: Instant) -> Option<Frame> {
        let own = self.own.clone()?;
        if let Some(at) = self.last_sent_at {
            if now.duration_since(at) < self.interval() {
                return None;
            }
        }
        let own_changed = self.last_sent_own.as_ref().is_none_or(|last| !last.same_motion(&own));
        let heartbeat = self
            .last_sent_at
            .is_none_or(|at| now.duration_since(at) >= self.heartbeat);
        let fresh_traffic = self
            .traffic
            .as_ref()
            .filter(|(_, at)| now.duration_since(*at) <= self.traffic_max_age)
            .map(|(t, _)| t.clone());
        let traffic_due = fresh_traffic.is_some()
            && self
                .last_traffic_at
                .is_none_or(|at| now.duration_since(at) >= self.traffic_every);
        if !(own_changed || heartbeat || traffic_due) {
            return None;
        }
        let mut frame = Frame::new(self.kind, now_ms(), own.clone());
        if traffic_due {
            frame.traffic = fresh_traffic;
            self.last_traffic_at = Some(now);
        }
        let frame = frame.fit_to_budget();
        self.last_sent_at = Some(now);
        self.last_sent_own = Some(own);
        frame.is_valid().then_some(frame)
    }
}

/// Why no sim connected: a log line, and a status hint for the user when
/// the cause is something they must fix.
struct NoSim {
    reason: String,
    hint: Option<String>,
}

fn connect(source: &Source) -> Result<Box<dyn Sim>, NoSim> {
    match source {
        Source::Replay { path, looped, speed } => replay::ReplaySim::open(path, *looped, *speed)
            .map(|s| Box::new(s) as Box<dyn Sim>)
            .map_err(|reason| NoSim { reason, hint: None }),
        Source::Auto { hz } => {
            let _ = hz; // only MSFS (Windows) takes a rate
            let mut why = Vec::new();
            #[allow(unused_mut)]
            let mut hint = None;
            #[cfg(windows)]
            match msfs::MsfsSim::connect(*hz) {
                Ok(s) => return Ok(Box::new(s)),
                Err(e) => {
                    if matches!(e, msfs::ConnectError::MissingDll(_)) {
                        hint = Some(msfs_proto::MISSING_DLL_HINT.to_string());
                    }
                    why.push(format!("MSFS: {e}"));
                }
            }
            match xplane_web::XplaneWeb::connect(xplane_web::DEFAULT_PORT) {
                Ok(s) => return Ok(Box::new(s)),
                Err(e) => why.push(format!("X-Plane web API: {e}")),
            }
            match xplane_udp::XplaneUdp::connect(xplane_udp::DEFAULT_PORT) {
                Ok(s) => return Ok(Box::new(s)),
                Err(e) => why.push(format!("X-Plane UDP: {e}")),
            }
            Err(NoSim {
                reason: why.join("; "),
                hint,
            })
        }
    }
}

/// Runs on its own thread until `stop` is set: finds a sim, polls it, and
/// hands paced frames to `out`, with the rate in Hz they are paced at.
/// `interval` is the air interval. `title` gets the aircraft title each time
/// it changes (None when there is none, or no sim); `extras` likewise.
pub fn run_driver(
    source: Source,
    interval: Duration,
    stop: &AtomicBool,
    mut out: impl FnMut(&Frame, u32),
    mut state: impl FnMut(SimState),
    mut title: impl FnMut(Option<String>),
    mut extras: impl FnMut(SimExtras),
) {
    let mut last_extras = SimExtras::default();
    let mut set_extras = |e: SimExtras| {
        if e != last_extras {
            last_extras = e.clone();
            extras(e);
        }
    };
    let mut last_title: Option<String> = None;
    let mut set_title = |t: Option<String>| {
        if t != last_title {
            last_title = t.clone();
            title(t);
        }
    };
    let mut pacer = Pacer::new(interval);
    let mut sim: Option<Box<dyn Sim>> = None;
    let mut next_search = Instant::now();
    let mut last_reason = String::new();
    let mut flying = true;
    state(SimState::Searching);
    while !stop.load(Ordering::Relaxed) {
        let Some(active) = sim.as_mut() else {
            if Instant::now() < next_search {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            match connect(&source) {
                Ok(s) => {
                    log::info!("sim: connected to {}", s.describe());
                    flying = s.flying();
                    state(if flying {
                        SimState::Connected {
                            kind: s.kind(),
                            detail: s.describe(),
                        }
                    } else {
                        SimState::NoFlight { kind: s.kind() }
                    });
                    pacer.reset();
                    sim = Some(s);
                }
                Err(NoSim { reason, hint }) => {
                    if reason != last_reason {
                        log::info!("sim: none found ({reason})");
                        last_reason = reason;
                        state(match hint {
                            Some(reason) => SimState::Unavailable { reason },
                            None => SimState::Searching,
                        });
                    }
                    next_search = Instant::now() + Duration::from_secs(5);
                }
            }
            continue;
        };
        let polled = active.poll();
        set_title(active.aircraft_title().filter(|_| active.flying()));
        set_extras(active.extras());
        if active.flying() != flying {
            flying = !flying;
            let kind = active.kind();
            if flying {
                log::info!("sim: flight loaded");
                state(SimState::Connected {
                    kind,
                    detail: active.describe(),
                });
            } else {
                log::info!("sim: no flight loaded (starting, menus or loading)");
                // Nothing parked on the map from the last flight.
                pacer.reset();
                state(SimState::NoFlight { kind });
            }
        }
        match polled {
            Ok(Some(f)) => {
                if flying {
                    pacer.ingest(f, Instant::now());
                }
            }
            Ok(None) => {}
            Err(SimError::Finished) => {
                log::info!("sim: replay finished");
                set_title(None);
                set_extras(SimExtras::default());
                state(SimState::Finished);
                sim = None;
                next_search = Instant::now() + Duration::from_secs(3600 * 24 * 365);
                continue;
            }
            Err(e) => {
                log::info!("sim: lost ({e}); searching again");
                set_title(None);
                set_extras(SimExtras::default());
                state(SimState::Searching);
                sim = None;
                last_reason.clear();
                next_search = Instant::now() + Duration::from_secs(2);
                continue;
            }
        }
        if flying {
            if let Some(frame) = pacer.due(Instant::now()) {
                out(&frame, rate::hz_of(pacer.interval()));
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(lat: f64) -> Own {
        Own {
            lat,
            lon: 8.5,
            alt_ft: 1000.0,
            gs_kt: 150.0,
            hdg: 250.0,
            trk: 250.0,
            vs_fpm: 0.0,
            on_ground: false,
            fuel_kg: None,
            callsign: None,
            tl: None,
        }
    }

    fn tfc() -> Vec<Traffic> {
        vec![Traffic {
            id: "1".into(),
            callsign: Some("TEST11".into()),
            lat: 50.1,
            lon: 8.6,
            alt_ft: 3000.0,
            gs_kt: 200.0,
            hdg: 90.0,
            on_ground: None,
        }]
    }

    fn frame(lat: f64, traffic: Option<Vec<Traffic>>) -> Frame {
        let mut f = Frame::new(SimKind::Msfs, 0, own(lat));
        f.traffic = traffic;
        f
    }

    #[test]
    fn paces_own_position_at_the_rate() {
        let t0 = Instant::now();
        let s = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p = Pacer::new(Duration::from_secs(1));
        assert!(p.due(t0).is_none(), "nothing before the first sample");
        p.ingest(frame(50.0, None), t0);
        assert!(p.due(t0).is_some());
        p.ingest(frame(50.001, None), s(400));
        assert!(p.due(s(400)).is_none(), "faster than 1 Hz is held back");
        assert_eq!(p.due(s(1000)).unwrap().own.lat, 50.001);
    }

    #[test]
    fn parked_aircraft_sends_only_a_heartbeat() {
        let t0 = Instant::now();
        let s = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p = Pacer::new(Duration::from_secs(1));
        p.ingest(frame(50.0, None), t0);
        assert!(p.due(t0).is_some());
        for ms in [1000, 2000, 3000, 4000] {
            p.ingest(frame(50.0, None), s(ms));
            assert!(p.due(s(ms)).is_none(), "unchanged at {ms}");
        }
        assert!(p.due(s(5000)).is_some(), "heartbeat after 5 s");
    }

    #[test]
    fn apu_fuel_burn_alone_keeps_a_parked_aircraft_on_the_heartbeat() {
        let t0 = Instant::now();
        let s = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p = Pacer::new(Duration::from_secs(1));
        let parked = |fuel: f64| {
            let mut f = frame(50.0, None);
            f.own.fuel_kg = Some(fuel);
            f
        };
        p.ingest(parked(8000.0), t0);
        assert_eq!(p.due(t0).unwrap().own.fuel_kg, Some(8000.0));
        for (i, ms) in [1000, 2000, 3000, 4000].into_iter().enumerate() {
            p.ingest(parked(8000.0 - 2.0 * (i as f64 + 1.0)), s(ms));
            assert!(p.due(s(ms)).is_none(), "fuel alone does not send at {ms}");
        }
        assert_eq!(
            p.due(s(5000)).unwrap().own.fuel_kg,
            Some(7992.0),
            "the heartbeat carries the latest fuel"
        );
    }

    fn taxiing(lat: f64, gs_kt: f64) -> Frame {
        let mut f = frame(lat, None);
        f.own.on_ground = true;
        f.own.alt_ft = 364.0;
        f.own.gs_kt = gs_kt;
        f
    }

    #[test]
    fn taxiing_is_paced_at_250_ms_and_flying_at_1_s() {
        let t0 = Instant::now();
        let s = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p = Pacer::new(Duration::from_secs(1));
        p.ingest(taxiing(50.0, 8.0), t0);
        assert_eq!(p.interval(), Duration::from_millis(250));
        assert!(p.due(t0).is_some());
        p.ingest(taxiing(50.00002, 8.0), s(200));
        assert!(p.due(s(200)).is_none(), "faster than 4 Hz is held back");
        assert!(p.due(s(250)).is_some(), "4 Hz on the ground");
        p.ingest(taxiing(50.00004, 8.0), s(500));
        assert!(p.due(s(500)).is_some());

        // take-off roll above 45 kt, then airborne: 1 Hz again
        p.ingest(taxiing(50.001, 60.0), s(750));
        assert_eq!(p.interval(), Duration::from_secs(1));
        assert!(p.due(s(750)).is_none(), "1 s after the last frame, not 250 ms");
        assert!(p.due(s(1500)).is_some());
        p.ingest(frame(50.01, None), s(2000));
        assert_eq!(p.interval(), Duration::from_secs(1));

        // --hz 2 in the air still taxis at 4 Hz
        let mut p = Pacer::new(Duration::from_millis(500));
        p.ingest(frame(50.0, None), t0);
        assert_eq!(p.interval(), Duration::from_millis(500));
        p.ingest(taxiing(50.0, 10.0), t0);
        assert_eq!(p.interval(), Duration::from_millis(250));
        p.reset();
        assert_eq!(
            p.interval(),
            Duration::from_millis(500),
            "a new sim starts at the air rate"
        );
    }

    #[test]
    fn parked_on_the_ground_still_sends_only_a_5_s_heartbeat() {
        let t0 = Instant::now();
        let s = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p = Pacer::new(Duration::from_secs(1));
        p.ingest(taxiing(50.0, 0.0), t0);
        assert!(p.due(t0).is_some());
        for ms in (250..5000).step_by(250) {
            p.ingest(taxiing(50.0, 0.0), s(ms));
            assert!(p.due(s(ms)).is_none(), "unchanged at {ms}");
        }
        assert!(p.due(s(5000)).is_some(), "heartbeat after 5 s");
    }

    #[test]
    fn traffic_rides_along_every_two_seconds_while_fresh() {
        let t0 = Instant::now();
        let s = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p = Pacer::new(Duration::from_secs(1));
        p.ingest(frame(50.0, Some(tfc())), t0);
        assert!(p.due(t0).unwrap().traffic.is_some());
        p.ingest(frame(50.001, Some(tfc())), s(1000));
        assert!(p.due(s(1000)).unwrap().traffic.is_none());
        p.ingest(frame(50.002, None), s(2000));
        assert!(p.due(s(2000)).unwrap().traffic.is_some());
        // the last snapshot was taken at 1000; at 7000 it is too old to send
        p.ingest(frame(50.003, None), s(7000));
        assert!(p.due(s(7000)).unwrap().traffic.is_none());
    }

    #[test]
    fn driver_plays_a_replay_and_reports_finished() {
        let dir = std::env::temp_dir().join(format!("snx-driver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("short.jsonl");
        let lines: Vec<String> = (0..3).map(|i| frame(50.0 + i as f64 * 0.01, None).to_json()).collect();
        std::fs::write(&path, lines.join("\n")).unwrap();
        let stop = AtomicBool::new(false);
        let mut frames = Vec::new();
        let mut states = Vec::new();
        run_driver(
            Source::Replay {
                path,
                looped: false,
                speed: 50.0,
            },
            Duration::from_millis(1),
            &stop,
            |f, _| frames.push(f.clone()),
            |s| {
                if s == SimState::Finished {
                    stop.store(true, Ordering::Relaxed);
                }
                states.push(s);
            },
            |t| assert_eq!(t, None, "a replay has no aircraft title"),
            |e| assert_eq!(e, SimExtras::default()),
        );
        assert!(matches!(
            states[1],
            SimState::Connected {
                kind: SimKind::Replay,
                ..
            }
        ));
        assert_eq!(states.last(), Some(&SimState::Finished));
        assert!(!frames.is_empty());
        assert!(frames.iter().all(|f| f.sim == SimKind::Replay));
    }
}
