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

#[cfg(windows)]
pub mod msfs;
pub mod msfs_proto;
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

/// Paces frames: own position at the configured rate when it changes, a
/// heartbeat while parked so the map does not go stale, and traffic at most
/// every 2 s.
pub struct Pacer {
    interval: Duration,
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
    pub fn new(interval: Duration) -> Self {
        Pacer {
            interval,
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
        *self = Pacer::new(self.interval);
    }

    pub fn ingest(&mut self, f: Frame, now: Instant) {
        let f = f.rounded();
        self.kind = f.sim;
        self.own = Some(f.own);
        if let Some(t) = f.traffic {
            self.traffic = Some((t, now));
        }
    }

    pub fn due(&mut self, now: Instant) -> Option<Frame> {
        let own = self.own.clone()?;
        if let Some(at) = self.last_sent_at {
            if now.duration_since(at) < self.interval {
                return None;
            }
        }
        let own_changed = self.last_sent_own.as_ref() != Some(&own);
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
/// hands paced frames to `out`.
pub fn run_driver(
    source: Source,
    interval: Duration,
    stop: &AtomicBool,
    mut out: impl FnMut(&Frame),
    mut state: impl FnMut(SimState),
) {
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
                state(SimState::Finished);
                sim = None;
                next_search = Instant::now() + Duration::from_secs(3600 * 24 * 365);
                continue;
            }
            Err(e) => {
                log::info!("sim: lost ({e}); searching again");
                state(SimState::Searching);
                sim = None;
                last_reason.clear();
                next_search = Instant::now() + Duration::from_secs(2);
                continue;
            }
        }
        if flying {
            if let Some(frame) = pacer.due(Instant::now()) {
                out(&frame);
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
            callsign: None,
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
            |f| frames.push(f.clone()),
            |s| {
                if s == SimState::Finished {
                    stop.store(true, Ordering::Relaxed);
                }
                states.push(s);
            },
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
