//! The own-aircraft frame rate: 4 Hz while the aircraft taxis or docks, the
//! air rate (`--hz`, 1 by default) otherwise.
//!
//! The EFB's docking board counts down the last 55 m to the stop point and
//! says STOP within 1 m. At 1 Hz a docking aircraft moves 1.5 to 2.5 m per
//! frame and a taxiing one about 7.7 m at 15 kt, so on the ground and slow
//! the connector sends 4 frames per second. In the air only the map reads
//! the stream, and 1 Hz is enough. The relay allows at most 5 frames per
//! second per connector, so the fast rate never goes above 4 Hz.
//!
//! The switch has hysteresis so a ground roll near the threshold does not
//! flap: fast below 40 kt on the ground, back to the air rate above 45 kt or
//! once airborne.

use std::time::Duration;

use crate::frame::Own;

/// The taxi and docking rate. The relay's limit is 5 frames per second.
pub const GROUND_HZ: u32 = 4;
// The relay allows at most 5 frames per second per connector.
const _: () = assert!(GROUND_HZ < 5);
/// One frame every 250 ms while taxiing and docking.
pub const GROUND_INTERVAL: Duration = Duration::from_millis(1000 / GROUND_HZ as u64);
/// On the ground and slower than this: the fast rate starts.
pub const FAST_BELOW_KT: f64 = 40.0;
/// Faster than this (or airborne): back to the air rate.
pub const SLOW_ABOVE_KT: f64 = 45.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RateMode {
    /// Airborne, or on the ground and fast (take-off and landing rolls).
    #[default]
    Air,
    /// On the ground and slow: taxi and docking.
    Ground,
}

/// Picks the rate mode from own-aircraft samples, with hysteresis.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateSelector {
    mode: RateMode,
}

impl RateSelector {
    pub fn mode(&self) -> RateMode {
        self.mode
    }

    /// Feeds one sample and returns the mode it leaves. A speed that is not
    /// a number never selects the fast rate.
    pub fn update(&mut self, own: &Own) -> RateMode {
        let ground = match self.mode {
            RateMode::Air => own.on_ground && own.gs_kt < FAST_BELOW_KT,
            RateMode::Ground => own.on_ground && own.gs_kt <= SLOW_ABOVE_KT,
        };
        self.mode = if ground { RateMode::Ground } else { RateMode::Air };
        self.mode
    }
}

/// The time between own-aircraft frames in a mode. `air` is the air
/// interval; the ground interval is 250 ms, or `air` if that is shorter
/// (a fast replay in tests).
pub fn interval_for(mode: RateMode, air: Duration) -> Duration {
    match mode {
        RateMode::Air => air,
        RateMode::Ground => GROUND_INTERVAL.min(air),
    }
}

/// Frames per second for an interval, for the tray ("Streaming MSFS 4 Hz").
pub fn hz_of(interval: Duration) -> u32 {
    let ms = interval.as_millis().max(1);
    u32::try_from((1000 + ms / 2) / ms).unwrap_or(u32::MAX).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(on_ground: bool, gs_kt: f64) -> Own {
        Own {
            lat: 50.0,
            lon: 8.5,
            alt_ft: 364.0,
            gs_kt,
            hdg: 250.0,
            trk: 250.0,
            vs_fpm: 0.0,
            on_ground,
            fuel_kg: None,
            callsign: None,
            tl: None,
        }
    }

    #[test]
    fn on_the_ground_and_slow_is_250_ms() {
        let r = RateSelector::default();
        assert_eq!(r.mode(), RateMode::Air, "starts at the air rate");
        for gs in [0.0, 2.0, 15.0, 39.9] {
            let mut r = RateSelector::default();
            assert_eq!(r.update(&own(true, gs)), RateMode::Ground, "{gs} kt");
        }
        let air = Duration::from_secs(1);
        assert_eq!(interval_for(RateMode::Ground, air), Duration::from_millis(250));
        assert_eq!(
            interval_for(RateMode::Ground, Duration::from_millis(500)),
            Duration::from_millis(250)
        );
        assert_eq!(hz_of(interval_for(RateMode::Ground, air)), 4);
    }

    #[test]
    fn airborne_or_fast_is_the_air_rate() {
        let air = Duration::from_secs(1);
        for (on_ground, gs) in [(false, 0.0), (false, 20.0), (false, 140.0), (true, 40.0), (true, 140.0)] {
            let mut r = RateSelector::default();
            assert_eq!(r.update(&own(on_ground, gs)), RateMode::Air, "{on_ground} {gs} kt");
        }
        assert_eq!(interval_for(RateMode::Air, air), Duration::from_secs(1));
        assert_eq!(hz_of(interval_for(RateMode::Air, air)), 1);
        assert_eq!(
            interval_for(RateMode::Air, Duration::from_millis(500)),
            Duration::from_millis(500),
            "--hz 2 in the air"
        );
        assert_eq!(hz_of(Duration::from_millis(500)), 2);
    }

    #[test]
    fn hysteresis_between_40_and_45_kt() {
        let mut r = RateSelector::default();
        // a landing roll slowing down: fast only once below 40 kt
        for (gs, want) in [
            (130.0, RateMode::Air),
            (60.0, RateMode::Air),
            (44.0, RateMode::Air),
            (41.0, RateMode::Air),
            (38.0, RateMode::Ground),
        ] {
            assert_eq!(r.update(&own(true, gs)), want, "slowing, {gs} kt");
        }
        // a fast taxi in the 40 to 45 kt band stays fast
        for gs in [42.0, 45.0, 43.0] {
            assert_eq!(r.update(&own(true, gs)), RateMode::Ground, "band, {gs} kt");
        }
        // the take-off roll: back above 45 kt
        assert_eq!(r.update(&own(true, 45.5)), RateMode::Air);
        assert_eq!(
            r.update(&own(true, 43.0)),
            RateMode::Air,
            "no flapping back in the band"
        );
        assert_eq!(r.update(&own(true, 39.0)), RateMode::Ground);
        // lift-off at any speed
        assert_eq!(r.update(&own(false, 30.0)), RateMode::Air);
    }

    #[test]
    fn a_speed_that_is_not_a_number_is_never_fast() {
        let mut r = RateSelector::default();
        assert_eq!(r.update(&own(true, f64::NAN)), RateMode::Air);
        r.update(&own(true, 5.0));
        assert_eq!(r.update(&own(true, f64::NAN)), RateMode::Air);
    }
}
