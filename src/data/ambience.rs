//! The sound of the land a 3D ride passes through, in two layers, each
//! named per biome in `biomes.json`:
//!
//! - a quiet bed, looped: wind, distant birds, traffic;
//! - calls, now and then: a goat's bleat, a gull, an eagle overhead — each
//!   at a random gap, from a random side.
//!
//! Pure arithmetic — the audio module plays what this decides.

use serde::{Deserialize, Serialize};

use super::biome::{Biome, Weights};

/// One looped recording of a kind of land. Several biomes may share one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bed {
    /// Cicadas in the heat.
    Cicadas,
    /// Wind: the countryside, the woods, the fields, the polder, the marsh,
    /// the desert and the coast, each with its own calls over it.
    Wind,
    /// Wind in the pines.
    Alpine,
}

pub const BED_COUNT: usize = 3;

impl Bed {
    pub const ALL: [Bed; BED_COUNT] = [Bed::Cicadas, Bed::Wind, Bed::Alpine];
}

/// One kind of call, recorded as a few takes so it does not repeat itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Call {
    Goat,
    Gull,
    Eagle,
    Hawk,
    Elk,
    Raven,
    Songbird,
    Blackbird,
    Rail,
    Frog,
    Crane,
    Goose,
    Loon,
}

pub const CALL_COUNT: usize = 13;

impl Call {
    #[cfg(test)]
    pub const ALL: [Call; CALL_COUNT] = [
        Call::Goat,
        Call::Gull,
        Call::Eagle,
        Call::Hawk,
        Call::Elk,
        Call::Raven,
        Call::Songbird,
        Call::Blackbird,
        Call::Rail,
        Call::Frog,
        Call::Crane,
        Call::Goose,
        Call::Loon,
    ];
}

/// How a biome sounds: its bed, and the calls heard over it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ambience {
    pub bed: Bed,
    #[serde(default)]
    pub calls: Vec<CallRule>,
}

/// One call a biome makes, now and then.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallRule {
    pub call: Call,
    /// The gap between two of these calls is drawn from this range, in
    /// seconds.
    pub every_s: [f32; 2],
    /// How loud, from 0 to 1, against the call's own levelled take.
    pub gain: f32,
}

impl CallRule {
    /// Whether the numbers make sense: gaps of a few seconds to twenty
    /// minutes, the shorter first, and a gain that is heard.
    pub fn is_sane(&self) -> bool {
        let [lo, hi] = self.every_s;
        lo.is_finite()
            && hi.is_finite()
            && (MIN_GAP_S..=1_200.0).contains(&lo)
            && lo <= hi
            && hi <= 1_200.0
            && self.gain.is_finite()
            && self.gain > 0.0
            && self.gain <= 1.0
    }
}

/// Calls no closer together than this, in seconds — one call answering
/// another is the bed's business.
pub const MIN_GAP_S: f32 = 3.0;

/// How loud each bed plays, indexed by `Bed as usize`, from 0 to 1.
pub type Gains = [f32; BED_COUNT];

/// A biome holding less of the land than this is not heard at all: a bed
/// faintly under another sounds like noise, not like the land beside the road.
pub const HEARD_FROM: f32 = 0.08;

/// How loud each bed plays where the land holds `weights` of each biome.
///
/// Each bed takes the share of the biomes that sound like it. Shares too
/// small to hear are dropped and the rest scaled back to the whole; a bed's
/// gain is the square root of its share, so that two beds crossing over keep
/// the loudness of one (their powers add up to one).
pub fn gains(weights: &Weights) -> Gains {
    gains_by(weights, |b| b.def().ambience.bed)
}

/// The shares of the land that are heard: each biome's weight, those under
/// [`HEARD_FROM`] of the whole dropped and the rest scaled back to one.
fn heard_shares(weights: &Weights) -> Weights {
    let mut share = [0.0f32; super::biome::COUNT];
    for b in Biome::ALL {
        let w = weights[b as usize];
        if w.is_finite() && w > 0.0 {
            share[b as usize] = w;
        }
    }
    normalise_heard(&mut share);
    share
}

/// `share` scaled to one, with those under [`HEARD_FROM`] of the whole
/// dropped first; all zero if nothing is there.
fn normalise_heard(share: &mut [f32]) {
    let total: f32 = share.iter().sum();
    if total <= 0.0 {
        share.fill(0.0);
        return;
    }
    for s in share.iter_mut() {
        if *s / total < HEARD_FROM {
            *s = 0.0;
        }
    }
    // The largest share of the whole is always at least 1 / len, above
    // HEARD_FROM, so something is always heard.
    let heard: f32 = share.iter().sum();
    for s in share.iter_mut() {
        *s /= heard;
    }
}

/// [`gains`], with each biome sounding like `bed_of` says.
fn gains_by(weights: &Weights, bed_of: impl Fn(Biome) -> Bed) -> Gains {
    let mut share = [0.0f32; BED_COUNT];
    for b in Biome::ALL {
        let w = weights[b as usize];
        if w.is_finite() && w > 0.0 {
            share[bed_of(b) as usize] += w;
        }
    }
    normalise_heard(&mut share);
    share.map(f32::sqrt)
}

/// One call to play now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cry {
    pub call: Call,
    /// Which take, any number: the player takes it modulo how many it has.
    pub take: u32,
    /// From 0 to 1.
    pub gain: f32,
    /// From -1 (left) to 1 (right).
    pub pan: f32,
}

/// Decides when each biome's calls are heard. One per 3D view; ticked a few
/// times a second while the ride sounds.
#[derive(Debug, Clone)]
pub struct Caller {
    /// Seconds to each call, per biome and rule; `None` until that biome is
    /// first heard, so a call does not wait out a gap begun elsewhere.
    waits: Vec<Vec<Option<f32>>>,
    rng: u64,
}

impl Caller {
    /// A caller whose choices follow from `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            waits: Biome::ALL
                .iter()
                .map(|b| vec![None; b.def().ambience.calls.len()])
                .collect(),
            rng: seed | 1,
        }
    }

    /// A number from 0 to 1.
    fn unit(&mut self) -> f32 {
        self.rng = super::world::splitmix(self.rng);
        (self.rng >> 40) as f32 / (1u64 << 24) as f32
    }

    /// `dt` seconds have gone by with the land holding `weights` of each
    /// biome: the calls due now.
    pub fn tick(&mut self, dt: f32, weights: &Weights) -> Vec<Cry> {
        self.tick_with(dt, weights, |b| &b.def().ambience.calls)
    }

    /// [`Caller::tick`], with each biome's calls as `calls_of` says.
    fn tick_with<'a>(
        &mut self,
        dt: f32,
        weights: &Weights,
        calls_of: impl Fn(Biome) -> &'a [CallRule],
    ) -> Vec<Cry> {
        let mut out = Vec::new();
        if !dt.is_finite() || dt <= 0.0 {
            return out;
        }
        // A long stall — the view hidden, the machine asleep — is not a
        // reason to call everything at once.
        let dt = dt.min(1.0);
        let share = heard_shares(weights);
        for b in Biome::ALL {
            let rules = calls_of(b);
            let i = b as usize;
            if self.waits[i].len() != rules.len() {
                self.waits[i] = vec![None; rules.len()];
            }
            if share[i] <= 0.0 {
                // Out of earshot: the next time it is heard starts afresh.
                self.waits[i].fill(None);
                continue;
            }
            for (k, rule) in rules.iter().enumerate() {
                let [lo, hi] = rule.every_s;
                let wait = match self.waits[i][k] {
                    // Newly heard: the first call comes some way into a gap.
                    None => lo * 0.3 + (hi - lo * 0.3) * self.unit(),
                    Some(w) => w,
                } - dt;
                if wait > 0.0 {
                    self.waits[i][k] = Some(wait);
                    continue;
                }
                self.waits[i][k] = Some(lo + (hi - lo) * self.unit());
                let take = (self.unit() * 1_000.0) as u32;
                let loud = 0.7 + 0.3 * self.unit();
                let pan = self.unit() * 1.6 - 0.8;
                out.push(Cry {
                    call: rule.call,
                    take,
                    gain: rule.gain * share[i].sqrt() * loud,
                    pan,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::biome::COUNT;

    fn only(b: Biome) -> Weights {
        let mut w = [0.0; COUNT];
        w[b as usize] = 1.0;
        w
    }

    fn power(g: &Gains) -> f32 {
        g.iter().map(|x| x * x).sum()
    }

    #[test]
    fn should_play_only_its_own_bed_in_a_biome_alone() {
        for b in Biome::ALL {
            let g = gains(&only(b));
            let bed = b.def().ambience.bed as usize;
            assert_eq!(g[bed], 1.0, "{b:?}");
            assert_eq!(g.iter().filter(|&&x| x > 0.0).count(), 1, "{b:?}");
        }
    }

    #[test]
    fn should_keep_the_loudness_of_one_bed_across_a_border() {
        let (a, b) = (Biome::Mediterranean, Biome::Mountain);
        assert_ne!(a.def().ambience.bed, b.def().ambience.bed);
        for k in 0..=20 {
            let t = k as f32 / 20.0;
            let mut w = [0.0; COUNT];
            w[a as usize] = 1.0 - t;
            w[b as usize] = t;
            let p = power(&gains(&w));
            assert!((p - 1.0).abs() < 1e-5, "at {t}: power {p}");
        }
    }

    #[test]
    fn should_cross_at_equal_power_halfway() {
        let mut w = [0.0; COUNT];
        w[Biome::Mediterranean as usize] = 0.5;
        w[Biome::Mountain as usize] = 0.5;
        let g = gains(&w);
        let half = 0.5f32.sqrt();
        assert!((g[Bed::Cicadas as usize] - half).abs() < 1e-6);
        assert!((g[Bed::Alpine as usize] - half).abs() < 1e-6);
    }

    #[test]
    fn should_drop_a_biome_too_faint_to_hear_and_give_its_share_back() {
        let mut w = [0.0; COUNT];
        w[Biome::Mediterranean as usize] = 0.93;
        w[Biome::Mountain as usize] = 0.07;
        let g = gains(&w);
        assert_eq!(g[Bed::Alpine as usize], 0.0);
        assert_eq!(g[Bed::Cicadas as usize], 1.0);
    }

    #[test]
    fn should_hear_a_biome_right_at_the_threshold() {
        let mut w = [0.0; COUNT];
        w[Biome::Mediterranean as usize] = 1.0 - HEARD_FROM;
        w[Biome::Mountain as usize] = HEARD_FROM;
        assert!(gains(&w)[Bed::Alpine as usize] > 0.0);
    }

    #[test]
    fn should_add_up_biomes_that_share_a_bed() {
        // Temperate and farmland both sound like the wind here: 0.05 each
        // is under the threshold alone, 0.1 together is heard.
        let bed_of = |b: Biome| match b {
            Biome::Temperate | Biome::Farmland => Bed::Wind,
            _ => Bed::Alpine,
        };
        let mut w = [0.0; COUNT];
        w[Biome::Temperate as usize] = 0.05;
        w[Biome::Farmland as usize] = 0.05;
        w[Biome::Coast as usize] = 0.9;
        let g = gains_by(&w, bed_of);
        assert!((g[Bed::Wind as usize] - 0.1f32.sqrt()).abs() < 1e-6);
        assert!((g[Bed::Alpine as usize] - 0.9f32.sqrt()).abs() < 1e-6);
    }

    #[test]
    fn should_be_silent_where_there_is_no_land() {
        assert_eq!(gains(&[0.0; COUNT]), [0.0; BED_COUNT]);
        let mut nan = [0.0; COUNT];
        nan[0] = f32::NAN;
        assert_eq!(gains(&nan), [0.0; BED_COUNT]);
    }

    #[test]
    fn should_ride_the_countryside_and_the_coast_on_wind_and_birds() {
        // The rider heard a train in the countryside's beds and thunder in
        // the surf. A countryside ride blends temperate with forest wherever
        // the land is wooded, so both have to change for it to go away.
        let calls =
            |b: Biome| -> Vec<Call> { b.def().ambience.calls.iter().map(|r| r.call).collect() };
        for b in [Biome::Temperate, Biome::Forest, Biome::Coast] {
            assert_eq!(b.def().ambience.bed, Bed::Wind, "{b:?}");
        }
        for b in [Biome::Temperate, Biome::Forest] {
            assert!(calls(b).contains(&Call::Songbird), "{b:?}");
            assert!(calls(b).contains(&Call::Blackbird), "{b:?}");
        }
        assert_eq!(calls(Biome::Coast), vec![Call::Gull]);
    }

    #[test]
    fn should_give_every_bed_a_biome() {
        // A bed no biome plays would ship a recording nobody hears.
        for bed in Bed::ALL {
            assert!(
                Biome::ALL.iter().any(|b| b.def().ambience.bed == bed),
                "{bed:?} is played by no biome"
            );
        }
    }

    #[test]
    fn should_give_every_call_a_biome() {
        for call in Call::ALL {
            assert!(
                Biome::ALL
                    .iter()
                    .any(|b| b.def().ambience.calls.iter().any(|r| r.call == call)),
                "{call:?} is made by no biome"
            );
        }
    }

    fn goat_every(lo: f32, hi: f32) -> Vec<CallRule> {
        vec![CallRule {
            call: Call::Goat,
            every_s: [lo, hi],
            gain: 0.8,
        }]
    }

    /// The times, in ticks of `dt`, at which `caller` calls over `secs`.
    fn calls_over(
        caller: &mut Caller,
        secs: f32,
        dt: f32,
        w: &Weights,
        rules: &[CallRule],
    ) -> Vec<(f32, Cry)> {
        let mut out = Vec::new();
        let mut t = 0.0;
        while t < secs {
            t += dt;
            for c in caller.tick_with(dt, w, |b| {
                if b == Biome::Mediterranean {
                    rules
                } else {
                    &[]
                }
            }) {
                out.push((t, c));
            }
        }
        out
    }

    #[test]
    fn should_call_now_and_then_with_gaps_inside_the_range() {
        let rules = goat_every(20.0, 50.0);
        let mut caller = Caller::new(7);
        let calls = calls_over(
            &mut caller,
            3_600.0,
            0.25,
            &only(Biome::Mediterranean),
            &rules,
        );
        // About an hour over 35 s on average: 103; well clear either side.
        assert!((80..=130).contains(&calls.len()), "{}", calls.len());
        // The first comes within one gap — not a wait of minutes on entering.
        assert!(calls[0].0 <= 50.25, "first at {}", calls[0].0);
        for pair in calls.windows(2) {
            let gap = pair[1].0 - pair[0].0;
            assert!((19.75..=50.25).contains(&gap), "gap of {gap} s");
        }
        for (_, c) in &calls {
            assert_eq!(c.call, Call::Goat);
            assert!((-0.8..=0.8).contains(&c.pan), "pan {}", c.pan);
            assert!(c.gain > 0.0 && c.gain <= 0.8, "gain {}", c.gain);
        }
        // From both sides, and in more than one take.
        assert!(calls.iter().any(|(_, c)| c.pan < -0.3));
        assert!(calls.iter().any(|(_, c)| c.pan > 0.3));
        let takes: std::collections::HashSet<u32> = calls.iter().map(|(_, c)| c.take % 6).collect();
        assert!(takes.len() >= 4, "{takes:?}");
    }

    #[test]
    fn should_not_call_where_its_biome_is_not_heard() {
        let rules = goat_every(5.0, 6.0);
        let mut caller = Caller::new(3);
        let mut w = [0.0; COUNT];
        w[Biome::Coast as usize] = 1.0 - HEARD_FROM / 2.0;
        w[Biome::Mediterranean as usize] = HEARD_FROM / 2.0;
        assert!(calls_over(&mut caller, 600.0, 0.25, &w, &rules).is_empty());
        assert!(calls_over(&mut caller, 600.0, 0.25, &only(Biome::Coast), &rules).is_empty());
    }

    #[test]
    fn should_call_more_quietly_where_its_biome_holds_less_of_the_land() {
        let rules = goat_every(5.0, 6.0);
        let loudest = |w: &Weights| {
            let mut caller = Caller::new(11);
            calls_over(&mut caller, 600.0, 0.25, w, &rules)
                .iter()
                .map(|(_, c)| c.gain)
                .fold(0.0, f32::max)
        };
        let mut half = [0.0; COUNT];
        half[Biome::Mediterranean as usize] = 0.25;
        half[Biome::Coast as usize] = 0.75;
        let (full, quarter) = (loudest(&only(Biome::Mediterranean)), loudest(&half));
        assert!(quarter > 0.0);
        // A quarter of the land: half the gain, as the beds' power share.
        assert!((quarter / full - 0.5).abs() < 0.1, "{quarter} / {full}");
    }

    #[test]
    fn should_not_call_everything_at_once_after_a_stall() {
        let rules = goat_every(5.0, 6.0);
        let mut caller = Caller::new(5);
        let w = only(Biome::Mediterranean);
        calls_over(&mut caller, 30.0, 0.25, &w, &rules);
        let after = caller.tick_with(600.0, &w, |b| {
            if b == Biome::Mediterranean {
                &rules
            } else {
                &[]
            }
        });
        assert!(after.len() <= 1, "{after:?}");
        assert!(caller.tick_with(f32::NAN, &w, |_| &rules).is_empty());
        assert!(caller.tick_with(-1.0, &w, |_| &rules).is_empty());
    }

    #[test]
    fn should_start_afresh_when_a_biome_comes_back_into_earshot() {
        // Leaving and coming back does not carry an old wait over: the
        // first call back comes within one gap of the return.
        let rules = goat_every(30.0, 40.0);
        let mut caller = Caller::new(9);
        let med = only(Biome::Mediterranean);
        calls_over(&mut caller, 100.0, 0.25, &med, &rules);
        calls_over(&mut caller, 100.0, 0.25, &only(Biome::Coast), &rules);
        let back = calls_over(&mut caller, 100.0, 0.25, &med, &rules);
        assert!(back[0].0 <= 40.25, "{}", back[0].0);
        assert!(back[0].0 >= 30.0 * 0.3 - 0.25, "{}", back[0].0);
    }

    #[test]
    fn should_read_only_sane_call_rules() {
        let rule = |lo: f32, hi: f32, gain: f32| CallRule {
            call: Call::Gull,
            every_s: [lo, hi],
            gain,
        };
        assert!(rule(MIN_GAP_S, MIN_GAP_S, 1.0).is_sane());
        assert!(rule(10.0, 1_200.0, 0.01).is_sane());
        assert!(!rule(MIN_GAP_S - 0.1, 10.0, 0.5).is_sane());
        assert!(!rule(20.0, 10.0, 0.5).is_sane());
        assert!(!rule(10.0, 1_201.0, 0.5).is_sane());
        assert!(!rule(10.0, 20.0, 0.0).is_sane());
        assert!(!rule(10.0, 20.0, 1.1).is_sane());
        assert!(!rule(f32::NAN, 20.0, 0.5).is_sane());
        for b in Biome::ALL {
            assert!(
                b.def().ambience.calls.iter().all(CallRule::is_sane),
                "{b:?}"
            );
        }
    }
}
