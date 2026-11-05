//! The replay simulator: plays a recorded flight in real time (or faster).
//!
//! Two file shapes are accepted:
//! - a document `{"description": "...", "samples": [{"t": <ms from start>, "own": {...}, "traffic"?: [...]}]}`;
//! - JSON Lines of frames exactly as `--print-frames` writes them, so a
//!   session can be recorded and played back. Recordings with real callsigns
//!   stay out of the repo; the checked-in fixtures are synthetic.

use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{Sim, SimError};
use crate::frame::{now_ms, Frame, Own, SimKind, Traffic};

#[derive(Debug, Clone, Deserialize)]
pub struct Sample {
    /// Milliseconds from the start of the recording.
    pub t: u64,
    pub own: Own,
    #[serde(default)]
    pub traffic: Option<Vec<Traffic>>,
}

#[derive(Debug, Deserialize)]
struct Document {
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    samples: Vec<Sample>,
}

pub fn parse(text: &str) -> Result<Vec<Sample>, String> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        if let Ok(doc) = serde_json::from_str::<Document>(trimmed) {
            return normalise(doc.samples);
        }
    }
    let mut samples = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let f: Frame = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", n + 1))?;
        samples.push(Sample {
            t: f.t,
            own: f.own,
            traffic: f.traffic,
        });
    }
    normalise(samples)
}

fn normalise(mut samples: Vec<Sample>) -> Result<Vec<Sample>, String> {
    if samples.is_empty() {
        return Err("the replay has no samples".into());
    }
    samples.sort_by_key(|s| s.t);
    let t0 = samples[0].t;
    for s in samples.iter_mut() {
        s.t -= t0;
    }
    Ok(samples)
}

pub struct ReplaySim {
    samples: Vec<Sample>,
    next: usize,
    start: Instant,
    speed: f64,
    looped: bool,
    name: String,
}

impl ReplaySim {
    pub fn open(path: &Path, looped: bool, speed: f64) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let samples = parse(&text)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        Ok(Self::from_samples(samples, looped, speed, name))
    }

    pub fn from_samples(samples: Vec<Sample>, looped: bool, speed: f64, name: String) -> Self {
        ReplaySim {
            samples,
            next: 0,
            start: Instant::now(),
            speed: if speed > 0.0 { speed } else { 1.0 },
            looped,
            name,
        }
    }

    fn elapsed_ms(&self, now: Instant) -> u64 {
        (now.duration_since(self.start).as_secs_f64() * 1000.0 * self.speed) as u64
    }

    /// The newest sample due at `now`, with the newest traffic among the
    /// samples skipped over.
    pub fn poll_at(&mut self, now: Instant) -> Result<Option<Frame>, SimError> {
        if self.next >= self.samples.len() {
            if !self.looped {
                return Err(SimError::Finished);
            }
            // Pause one interval, then start over.
            let last = self.samples.last().map(|s| s.t).unwrap_or(0);
            if self.elapsed_ms(now) < last + 1000 {
                return Ok(None);
            }
            self.next = 0;
            self.start = now;
        }
        let elapsed = self.elapsed_ms(now);
        let mut latest: Option<&Sample> = None;
        let mut traffic: Option<Vec<Traffic>> = None;
        while self.next < self.samples.len() && self.samples[self.next].t <= elapsed {
            let s = &self.samples[self.next];
            if s.traffic.is_some() {
                traffic = s.traffic.clone();
            }
            latest = Some(s);
            self.next += 1;
        }
        Ok(latest.map(|s| {
            let mut f = Frame::new(SimKind::Replay, now_ms(), s.own.clone());
            f.traffic = traffic;
            f
        }))
    }

    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.samples.last().map(|s| s.t).unwrap_or(0))
    }
}

impl Sim for ReplaySim {
    fn kind(&self) -> SimKind {
        SimKind::Replay
    }

    fn describe(&self) -> String {
        format!("Replay {} ({} s)", self.name, self.duration().as_secs())
    }

    fn poll(&mut self) -> Result<Option<Frame>, SimError> {
        self.poll_at(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{
      "description": "synthetic",
      "samples": [
        {"t": 1000, "own": {"lat": 50.0, "lon": 8.5, "altFt": 364, "gsKt": 0, "hdg": 249, "trk": 249, "vsFpm": 0, "onGround": true}},
        {"t": 2000, "own": {"lat": 50.001, "lon": 8.5, "altFt": 364, "gsKt": 10, "hdg": 249, "trk": 249, "vsFpm": 0, "onGround": true},
         "traffic": [{"id": "a", "callsign": "TEST11", "lat": 50.1, "lon": 8.6, "altFt": 3000, "gsKt": 180, "hdg": 70}]},
        {"t": 3000, "own": {"lat": 50.002, "lon": 8.5, "altFt": 364, "gsKt": 20, "hdg": 249, "trk": 249, "vsFpm": 0, "onGround": true}}
      ]
    }"#;

    #[test]
    fn parses_documents_and_rebases_time() {
        let s = parse(DOC).unwrap();
        assert_eq!(s.iter().map(|x| x.t).collect::<Vec<_>>(), vec![0, 1000, 2000]);
        assert!(s[1].traffic.is_some());
    }

    #[test]
    fn parses_print_frames_output() {
        let a = Frame::new(SimKind::Msfs, 5_000, parse(DOC).unwrap()[0].own.clone());
        let b = Frame { t: 6_000, ..a.clone() };
        let text = format!("{}\n\n{}\n", a.to_json(), b.to_json());
        let s = parse(&text).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].t, 1000);
        assert!(parse("").is_err());
        assert!(parse("{nope").is_err());
    }

    #[test]
    fn plays_in_time_and_carries_skipped_traffic() {
        let mut r = ReplaySim::from_samples(parse(DOC).unwrap(), false, 1.0, "doc".into());
        let t0 = r.start;
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert_eq!(r.poll_at(at(0)).unwrap().unwrap().own.lat, 50.0);
        assert!(r.poll_at(at(500)).unwrap().is_none());
        // jump past both remaining samples: the newest own, and the traffic from the skipped one
        let f = r.poll_at(at(2500)).unwrap().unwrap();
        assert_eq!(f.own.lat, 50.002);
        assert_eq!(f.sim, SimKind::Replay);
        assert_eq!(f.traffic.unwrap()[0].callsign.as_deref(), Some("TEST11"));
        assert!(matches!(r.poll_at(at(3000)), Err(SimError::Finished)));
    }

    #[test]
    fn loops_after_a_pause_and_honours_speed() {
        let mut r = ReplaySim::from_samples(parse(DOC).unwrap(), true, 2.0, "doc".into());
        let t0 = r.start;
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        r.poll_at(at(0)).unwrap();
        assert_eq!(r.poll_at(at(1000)).unwrap().unwrap().own.lat, 50.002, "2x speed");
        assert!(r.poll_at(at(1200)).unwrap().is_none(), "pausing before the loop");
        assert_eq!(r.poll_at(at(1600)).unwrap().unwrap().own.lat, 50.0, "started over");
    }

    #[test]
    fn the_checked_in_fixture_is_synthetic_and_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/eddf-departure.json");
        let samples = parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(samples.len() > 300);
        for s in &samples {
            let mut f = Frame::new(SimKind::Replay, 0, s.own.clone());
            f.traffic = s.traffic.clone();
            assert!(f.is_valid());
            let calls = std::iter::once(s.own.callsign.clone())
                .chain(s.traffic.iter().flatten().map(|t| t.callsign.clone()))
                .flatten();
            for c in calls {
                assert!(
                    c.starts_with("DEMO") || c.starts_with("TEST"),
                    "non-synthetic callsign {c}"
                );
            }
        }
    }
}
