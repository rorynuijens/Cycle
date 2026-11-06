//! The ambient mix: every bed's loop, faded towards the gain asked of it,
//! and the calls playing over them, summed under one master volume.
//!
//! Filled on the sound server's thread, so it does nothing that can wait:
//! no decoding, no allocation, no locks of its own.

use std::sync::Arc;

use crate::data::ambience::{Bed, Gains, BED_COUNT};

/// Frames a second the mix runs at — what the bundled loops are encoded at.
pub const RATE: u32 = 48_000;
/// Interleaved channels: left, right.
pub const CHANNELS: usize = 2;
/// Seconds a bed takes to fade between silence and full: slower than a
/// biome border is crossed at speed, so a border is a blend, not a cut.
pub const FADE_S: f32 = 4.0;
/// Seconds the whole mix takes to fade in or out, as the view opens and
/// closes or the volume is moved.
pub const MASTER_FADE_S: f32 = 1.5;

/// One bed's loop and how loud it is now.
#[derive(Default)]
struct Voice {
    /// Interleaved stereo frames, played round and round. `None` until
    /// decoded, and again once it has been silent long enough to let go of.
    pcm: Option<Arc<[i16]>>,
    /// The next sample to play, an index into `pcm`.
    pos: usize,
    gain: f32,
    target: f32,
}

/// Calls playing at once, at most: one more is let go rather than crowd in.
pub const MAX_CALLS: usize = 4;

/// One call playing through once: a mono take, panned.
struct Shot {
    pcm: Arc<[i16]>,
    pos: usize,
    left: f32,
    right: f32,
}

/// The beds and the calls, mixed. Gains glide: they move towards their targets at a fixed
/// rate and never jump, which would click.
#[derive(Default)]
pub struct Mixer {
    voices: [Voice; BED_COUNT],
    /// The calls playing now — a fixed few, so none is ever allocated on
    /// the sound server's thread.
    shots: [Option<Shot>; MAX_CALLS],
    master: f32,
    master_target: f32,
    /// Frames filled with something to hear in them, ever — how the player
    /// is seen to be playing.
    heard_frames: u64,
}

/// Moves `now` towards `target` by at most `step`.
fn glide(now: f32, target: f32, step: f32) -> f32 {
    if now < target {
        (now + step).min(target)
    } else {
        (now - step).max(target)
    }
}

impl Mixer {
    pub fn new() -> Self {
        Self::default()
    }

    /// How loud each bed should be.
    pub fn set_targets(&mut self, gains: &Gains) {
        for (v, g) in self.voices.iter_mut().zip(gains) {
            v.target = if g.is_finite() {
                g.clamp(0.0, 1.0)
            } else {
                0.0
            };
        }
    }

    /// How loud the whole mix should be, from 0 to 1.
    pub fn set_master(&mut self, volume: f32) {
        self.master_target = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            0.0
        };
    }

    /// The beds asked to play whose loops are not yet here.
    pub fn wanted(&self) -> Vec<Bed> {
        Bed::ALL
            .into_iter()
            .filter(|&b| {
                let v = &self.voices[b as usize];
                v.target > 0.0 && v.pcm.is_none()
            })
            .collect()
    }

    /// `bed`'s loop is decoded: it starts from silence and fades up to its
    /// target. An odd sample is dropped, so the loop holds whole frames.
    pub fn install(&mut self, bed: Bed, pcm: Arc<[i16]>) {
        let v = &mut self.voices[bed as usize];
        if pcm.len() < CHANNELS || v.pcm.is_some() {
            return;
        }
        v.pcm = Some(pcm);
        v.pos = 0;
        v.gain = 0.0;
    }

    /// Plays `pcm` — mono frames — once, at `gain` (0 to 1) from `pan`
    /// (-1 left to 1 right), at equal power. Dropped when [`MAX_CALLS`] are
    /// playing already. The caller keeps its own hold on `pcm`, so a call
    /// ending frees nothing on the sound server's thread.
    pub fn shoot(&mut self, pcm: Arc<[i16]>, gain: f32, pan: f32) {
        if pcm.is_empty() || !gain.is_finite() || gain <= 0.0 {
            return;
        }
        let Some(free) = self.shots.iter_mut().find(|s| s.is_none()) else {
            return;
        };
        let gain = gain.min(1.0);
        let pan = if pan.is_finite() {
            pan.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        *free = Some(Shot {
            pcm,
            pos: 0,
            left: gain * angle.cos(),
            right: gain * angle.sin(),
        });
    }

    /// How many calls are playing.
    pub fn calls_playing(&self) -> usize {
        self.shots.iter().flatten().count()
    }

    /// Lets go of the loops that are silent and asked to stay so, handing
    /// them back so they are freed off the sound server's thread.
    pub fn evict_silent(&mut self) -> Vec<Arc<[i16]>> {
        self.voices
            .iter_mut()
            .filter(|v| v.gain == 0.0 && v.target == 0.0)
            .filter_map(|v| v.pcm.take())
            .collect()
    }

    /// Whether the mix is silent and asked to stay so: the stream can rest.
    pub fn is_resting(&self) -> bool {
        let quiet = |gain: f32, target: f32| gain == 0.0 && target == 0.0;
        quiet(self.master, self.master_target)
            || (self.calls_playing() == 0
                && self
                    .voices
                    .iter()
                    .all(|v| quiet(v.gain, v.target) || v.pcm.is_none()))
    }

    /// How many frames with sound in them have been filled, ever.
    pub fn heard_frames(&self) -> u64 {
        self.heard_frames
    }

    /// Fills `out` — interleaved stereo — with the next frames of the mix.
    pub fn fill(&mut self, out: &mut [f32]) {
        let step = 1.0 / (FADE_S * RATE as f32);
        let master_step = 1.0 / (MASTER_FADE_S * RATE as f32);
        out.fill(0.0);
        for v in &mut self.voices {
            let Some(pcm) = v.pcm.as_deref() else {
                continue;
            };
            if v.gain == 0.0 && v.target == 0.0 {
                continue;
            }
            let whole = pcm.len() - pcm.len() % CHANNELS;
            for frame in out.as_chunks_mut::<CHANNELS>().0 {
                v.gain = glide(v.gain, v.target, step);
                if v.pos + CHANNELS > whole {
                    v.pos = 0;
                }
                for (o, &s) in frame.iter_mut().zip(&pcm[v.pos..v.pos + CHANNELS]) {
                    *o += v.gain * s as f32 / 32_768.0;
                }
                v.pos += CHANNELS;
            }
        }
        for slot in &mut self.shots {
            let Some(shot) = slot else {
                continue;
            };
            for frame in out.as_chunks_mut::<CHANNELS>().0 {
                let Some(&s) = shot.pcm.get(shot.pos) else {
                    break;
                };
                let s = s as f32 / 32_768.0;
                frame[0] += shot.left * s;
                frame[1] += shot.right * s;
                shot.pos += 1;
            }
            if shot.pos >= shot.pcm.len() {
                *slot = None;
            }
        }
        for frame in out.as_chunks_mut::<CHANNELS>().0 {
            self.master = glide(self.master, self.master_target, master_step);
            if self.master > 0.0 && frame.iter().any(|&o| o != 0.0) {
                self.heard_frames += 1;
            }
            for o in frame {
                *o = (*o * self.master).clamp(-1.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loop of `frames` frames, every sample `value`.
    fn flat(frames: usize, value: i16) -> Arc<[i16]> {
        vec![value; frames * CHANNELS].into()
    }

    fn only(bed: Bed, gain: f32) -> Gains {
        let mut g = [0.0; BED_COUNT];
        g[bed as usize] = gain;
        g
    }

    fn run(m: &mut Mixer, seconds: f32) -> Vec<f32> {
        let mut out = vec![0.0; (seconds * RATE as f32) as usize * CHANNELS];
        m.fill(&mut out);
        out
    }

    #[test]
    fn should_be_silent_until_asked() {
        let mut m = Mixer::new();
        m.install(Bed::Cicadas, flat(100, 16_000));
        m.set_master(1.0);
        assert!(run(&mut m, 0.1).iter().all(|&s| s == 0.0));
        assert!(m.is_resting());
    }

    #[test]
    fn should_fade_a_bed_in_over_its_fade_and_never_jump() {
        let mut m = Mixer::new();
        m.install(Bed::Alpine, flat(1_000, 16_384));
        m.set_master(1.0);
        m.set_targets(&only(Bed::Alpine, 1.0));
        let out = run(&mut m, FADE_S + 1.0);
        let left: Vec<f32> = out.iter().step_by(CHANNELS).copied().collect();
        let biggest_step = left
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        // Full scale here is 0.5: a step of one part in 10,000 is no click.
        assert!(biggest_step < 1e-4, "a jump of {biggest_step}");
        // Halfway through the fade the bed is near half its gain (and the
        // master, quicker, is already full).
        let half = left[(FADE_S / 2.0 * RATE as f32) as usize];
        assert!((half - 0.25).abs() < 0.01, "{half}");
        assert!((left[left.len() - 1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn should_play_a_loop_round_without_a_gap() {
        // A ramp of 7 frames, played for 20: it comes round to the start
        // straight after the end, every time.
        let pcm: Arc<[i16]> = (0..7)
            .flat_map(|k| [k * 1_000, -k * 1_000])
            .collect::<Vec<i16>>()
            .into();
        let mut m = Mixer::new();
        m.install(Bed::Cicadas, pcm);
        // Gains already at full, so only the loop is under test.
        m.voices[Bed::Cicadas as usize].gain = 1.0;
        m.voices[Bed::Cicadas as usize].target = 1.0;
        m.master = 1.0;
        m.master_target = 1.0;
        let mut out = vec![0.0; 20 * CHANNELS];
        m.fill(&mut out);
        for (f, frame) in out.as_chunks::<CHANNELS>().0.iter().enumerate() {
            let want = (f % 7) as f32 * 1_000.0 / 32_768.0;
            assert!((frame[0] - want).abs() < 1e-6, "frame {f}");
            assert!((frame[1] + want).abs() < 1e-6, "frame {f}");
        }
    }

    #[test]
    fn should_carry_on_where_the_last_fill_stopped() {
        let pcm: Arc<[i16]> = (0..10).flat_map(|k| [k, k]).collect::<Vec<i16>>().into();
        let mut m = Mixer::new();
        m.install(Bed::Cicadas, pcm);
        m.voices[Bed::Cicadas as usize].gain = 1.0;
        m.voices[Bed::Cicadas as usize].target = 1.0;
        m.master = 1.0;
        m.master_target = 1.0;
        let mut a = vec![0.0; 3 * CHANNELS];
        let mut b = vec![0.0; 3 * CHANNELS];
        m.fill(&mut a);
        m.fill(&mut b);
        assert!((b[0] - 3.0 / 32_768.0).abs() < 1e-9);
    }

    #[test]
    fn should_glide_two_beds_past_each_other_at_one_rate() {
        // Alpine out, cicadas in, from full and from silence: they move at one
        // rate, so over one signal their sum holds steady the whole way.
        let mut m = Mixer::new();
        m.install(Bed::Alpine, flat(10, 10_000));
        m.install(Bed::Cicadas, flat(10, 10_000));
        m.voices[Bed::Alpine as usize].gain = 1.0;
        m.master = 1.0;
        m.set_master(1.0);
        m.set_targets(&only(Bed::Cicadas, 1.0));
        let out = run(&mut m, FADE_S * 2.0);
        let full = 10_000.0 / 32_768.0;
        // Within 1 %: the two glides round differently over 190,000 steps.
        let worst = out.iter().map(|&s| (s - full).abs()).fold(0.0, f32::max);
        assert!(worst < full * 0.01, "off by {worst}");
        assert_eq!(m.voices[Bed::Alpine as usize].gain, 0.0);
        assert_eq!(m.voices[Bed::Cicadas as usize].gain, 1.0);
    }

    #[test]
    fn should_wait_for_a_loop_before_fading_it_in() {
        let mut m = Mixer::new();
        m.set_master(1.0);
        m.set_targets(&only(Bed::Wind, 0.8));
        assert_eq!(m.wanted(), vec![Bed::Wind]);
        run(&mut m, 2.0);
        m.install(Bed::Wind, flat(10, 1));
        assert!(m.wanted().is_empty());
        // Arrived late, it still starts from silence.
        assert_eq!(m.voices[Bed::Wind as usize].gain, 0.0);
    }

    #[test]
    fn should_fade_the_whole_mix_out_and_rest() {
        let mut m = Mixer::new();
        m.install(Bed::Alpine, flat(100, 20_000));
        m.voices[Bed::Alpine as usize].gain = 1.0;
        m.set_targets(&only(Bed::Alpine, 1.0));
        m.master = 1.0;
        m.set_master(0.0);
        assert!(!m.is_resting());
        let out = run(&mut m, MASTER_FADE_S + 0.1);
        assert_eq!(out[out.len() - 1], 0.0);
        assert!(out[0] > 0.6);
        assert!(m.is_resting());
    }

    #[test]
    fn should_let_go_of_only_the_loops_that_are_silent() {
        let mut m = Mixer::new();
        m.install(Bed::Alpine, flat(10, 1));
        m.install(Bed::Cicadas, flat(10, 1));
        m.set_targets(&only(Bed::Cicadas, 1.0));
        m.voices[Bed::Cicadas as usize].gain = 0.0;
        assert_eq!(m.evict_silent().len(), 1);
        assert!(m.voices[Bed::Alpine as usize].pcm.is_none());
        assert!(m.voices[Bed::Cicadas as usize].pcm.is_some());
    }

    #[test]
    fn should_never_play_louder_than_full_scale() {
        let mut m = Mixer::new();
        for b in Bed::ALL {
            m.install(b, flat(10, i16::MAX));
            m.voices[b as usize].gain = 1.0;
        }
        m.set_targets(&[1.0; BED_COUNT]);
        m.master = 1.0;
        m.set_master(1.0);
        assert!(run(&mut m, 0.01).iter().all(|&s| s <= 1.0));
    }

    #[test]
    fn should_play_a_call_once_from_its_side_and_let_it_go() {
        let mut m = Mixer::new();
        m.master = 1.0;
        m.set_master(1.0);
        let take: Arc<[i16]> = vec![16_384; 100].into();
        m.shoot(take.clone(), 1.0, -1.0);
        assert_eq!(m.calls_playing(), 1);
        assert!(!m.is_resting());
        let out = run(&mut m, 300.0 / RATE as f32);
        // Hard left: all in the left channel, for its 100 frames, then gone.
        assert!((out[0] - 0.5).abs() < 1e-6, "{}", out[0]);
        assert!(out[1].abs() < 1e-6, "{}", out[1]);
        assert!((out[99 * CHANNELS] - 0.5).abs() < 1e-6);
        assert_eq!(out[100 * CHANNELS], 0.0);
        assert_eq!(m.calls_playing(), 0);
        assert!(m.is_resting());
        // The take is still the caller's to keep.
        assert_eq!(Arc::strong_count(&take), 1);
    }

    #[test]
    fn should_pan_a_call_at_equal_power() {
        let mut m = Mixer::new();
        m.master = 1.0;
        m.set_master(1.0);
        m.shoot(vec![16_384; 10].into(), 0.8, 0.0);
        let out = run(&mut m, 4.0 / RATE as f32);
        let half = 0.5 * 0.8 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[0] - half).abs() < 1e-6 && (out[1] - half).abs() < 1e-6);
        let mut m = Mixer::new();
        m.master = 1.0;
        m.set_master(1.0);
        m.shoot(vec![16_384; 10].into(), 0.8, 0.6);
        let out = run(&mut m, 4.0 / RATE as f32);
        let power = out[0] * out[0] + out[1] * out[1];
        assert!((power - 0.4 * 0.4).abs() < 1e-6, "{power}");
        assert!(out[1] > out[0]);
    }

    #[test]
    fn should_carry_a_call_on_across_fills_and_under_the_master() {
        let mut m = Mixer::new();
        m.master = 0.5;
        m.set_master(0.5);
        m.shoot((0..10).collect::<Vec<i16>>().into(), 1.0, 1.0);
        let mut a = vec![0.0; 4 * CHANNELS];
        let mut b = vec![0.0; 4 * CHANNELS];
        m.fill(&mut a);
        m.fill(&mut b);
        assert!((b[1] - 0.5 * 4.0 / 32_768.0).abs() < 1e-9, "{}", b[1]);
    }

    #[test]
    fn should_drop_a_call_rather_than_crowd_in_or_play_nonsense() {
        let mut m = Mixer::new();
        for _ in 0..MAX_CALLS + 2 {
            m.shoot(vec![1; 10].into(), 0.5, 0.0);
        }
        assert_eq!(m.calls_playing(), MAX_CALLS);
        let mut m = Mixer::new();
        m.shoot(Vec::new().into(), 0.5, 0.0);
        m.shoot(vec![1; 10].into(), 0.0, 0.0);
        m.shoot(vec![1; 10].into(), f32::NAN, 0.0);
        assert_eq!(m.calls_playing(), 0);
        // A wild pan is the middle; a gain past full is full.
        m.master = 1.0;
        m.set_master(1.0);
        m.shoot(vec![32_767; 10].into(), 9.0, f32::NAN);
        let out = run(&mut m, 2.0 / RATE as f32);
        assert!((out[0] - out[1]).abs() < 1e-6 && out[0] < 0.71, "{out:?}");
    }

    #[test]
    fn should_refuse_nonsense_gains() {
        let mut m = Mixer::new();
        m.set_targets(&[f32::NAN; BED_COUNT]);
        m.set_master(f32::INFINITY);
        assert!(m.wanted().is_empty());
        assert_eq!(m.master_target, 0.0);
        m.set_targets(&[7.0; BED_COUNT]);
        assert!(m.voices.iter().all(|v| v.target == 1.0));
    }
}
