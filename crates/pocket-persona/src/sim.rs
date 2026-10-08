//! Deterministic facial behavior for the Pocket Persona renderer.
//!
//! The timings and amplitude-only viseme driver mirror Persona's React hooks,
//! but live in the native fixed-step core so the QuickJS guest only decides
//! policy.

const BLINK_MIN_INTERVAL: f32 = 2.0;
const BLINK_MAX_INTERVAL: f32 = 6.0;
const BLINK_DURATION: f32 = 0.24;
const LIP_AUDIBLE_THRESHOLD: f32 = 0.008;
const VISEME_COUNT: usize = 5;

#[derive(Clone)]
pub struct Pcg32 {
    state: u64,
}

impl Pcg32 {
    pub fn new(seed: u64) -> Self {
        let mut rng = Self {
            state: seed.wrapping_add(0x853c_49e6_748f_ea9b),
        };
        rng.next_u32();
        rng
    }

    fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        xorshifted.rotate_right((old >> 59) as u32)
    }

    pub fn next_f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }

    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + self.next_f32() * (hi - lo)
    }

    pub fn index(&mut self, len: usize) -> usize {
        if len <= 1 {
            0
        } else {
            ((self.next_f32() * len as f32) as usize).min(len - 1)
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FaceOutputs {
    pub blink: f32,
    pub blink_changed: bool,
    pub visemes: [f32; VISEME_COUNT],
    pub visemes_changed: bool,
}

pub struct FaceSim {
    rng: Pcg32,
    blink_wait: f32,
    blink_progress: Option<f32>,
    last_blink: f32,
    lip_smoothed: f32,
    lip_phase: f32,
    last_visemes: [f32; VISEME_COUNT],
}

impl FaceSim {
    pub fn new(seed: u64) -> Self {
        let mut rng = Pcg32::new(seed);
        let blink_wait = rng.range(BLINK_MIN_INTERVAL, BLINK_MAX_INTERVAL);
        Self {
            rng,
            blink_wait,
            blink_progress: None,
            last_blink: 0.0,
            lip_smoothed: 0.0,
            lip_phase: 0.0,
            last_visemes: [0.0; VISEME_COUNT],
        }
    }

    pub fn tick(&mut self, dt: f32, audio_level: f32, speaking: bool) -> FaceOutputs {
        let dt = dt.max(0.0);
        let blink = if let Some(progress) = self.blink_progress.as_mut() {
            *progress += dt / BLINK_DURATION;
            if *progress >= 1.0 {
                self.blink_progress = None;
                self.blink_wait = self.rng.range(BLINK_MIN_INTERVAL, BLINK_MAX_INTERVAL);
                0.0
            } else {
                (core::f32::consts::PI * *progress).sin()
            }
        } else {
            self.blink_wait -= dt;
            if self.blink_wait <= 0.0 {
                self.blink_progress = Some(f32::EPSILON);
            }
            0.0
        };

        let audible = speaking && audio_level > LIP_AUDIBLE_THRESHOLD;
        let normalized = if audible {
            (audio_level.clamp(0.0, 1.0) * 2.8).min(1.0)
        } else {
            0.0
        };
        let tau = if normalized > self.lip_smoothed {
            0.055
        } else {
            0.1
        };
        let smoothing = 1.0 - (-dt / tau).exp();
        self.lip_smoothed += (normalized - self.lip_smoothed) * smoothing;
        self.lip_phase += dt * (8.0 + self.lip_smoothed * 9.0);
        let active = self.lip_phase.floor() as usize % VISEME_COUNT;
        let mut visemes = [0.0; VISEME_COUNT];
        for (index, weight) in visemes.iter_mut().enumerate() {
            let shape =
                (1.0 - (index as isize - active as isize).unsigned_abs() as f32 * 0.72).max(0.0);
            let flutter = 0.74 + (self.lip_phase * 5.7 + index as f32).sin() * 0.18;
            *weight = (self.lip_smoothed * shape * flutter).min(0.62);
        }

        let blink_changed = blink.to_bits() != self.last_blink.to_bits();
        let visemes_changed = visemes
            .iter()
            .zip(self.last_visemes)
            .any(|(a, b)| a.to_bits() != b.to_bits());
        self.last_blink = blink;
        self.last_visemes = visemes;
        FaceOutputs {
            blink,
            blink_changed,
            visemes,
            visemes_changed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_seed_replays_exactly() {
        let mut a = FaceSim::new(7);
        let mut b = FaceSim::new(7);
        for frame in 0..3_600 {
            let level = if frame % 240 < 120 { 0.2 } else { 0.0 };
            let (a, b) = (
                a.tick(1.0 / 60.0, level, true),
                b.tick(1.0 / 60.0, level, true),
            );
            assert_eq!(a.blink.to_bits(), b.blink.to_bits());
            assert_eq!(a.visemes.map(f32::to_bits), b.visemes.map(f32::to_bits));
        }
    }

    #[test]
    fn blink_intervals_match_persona_bounds() {
        let mut sim = FaceSim::new(42);
        let mut starts = Vec::new();
        let mut previous = 0.0;
        for frame in 0..60 * 120 {
            let output = sim.tick(1.0 / 60.0, 0.0, false);
            if previous == 0.0 && output.blink > 0.0 {
                starts.push(frame as f32 / 60.0);
            }
            previous = output.blink;
        }
        assert!(starts.len() > 15);
        for pair in starts.windows(2) {
            let interval = pair[1] - pair[0] - BLINK_DURATION;
            assert!(
                (BLINK_MIN_INTERVAL - 1.0 / 60.0..=BLINK_MAX_INTERVAL + 1.0 / 60.0)
                    .contains(&interval),
                "blink wait {interval}"
            );
        }
    }

    #[test]
    fn lip_sync_rises_and_releases_smoothly() {
        let mut sim = FaceSim::new(1);
        let mut peak = 0.0f32;
        for _ in 0..60 {
            peak = peak.max(
                sim.tick(1.0 / 60.0, 0.3, true)
                    .visemes
                    .into_iter()
                    .fold(0.0, f32::max),
            );
        }
        assert!(peak > 0.4);
        let first_release = sim
            .tick(1.0 / 60.0, 0.0, false)
            .visemes
            .into_iter()
            .fold(0.0, f32::max);
        assert!(first_release > 0.0, "release should not snap");
        let mut settled = first_release;
        for _ in 0..120 {
            settled = sim
                .tick(1.0 / 60.0, 0.0, false)
                .visemes
                .into_iter()
                .fold(0.0, f32::max);
        }
        assert!(settled < 1e-4);
    }
}
