//! The bundled sounds: one Ogg Vorbis loop per bed and a few takes per call,
//! compiled in, and decoded only when a ride first needs them.
//!
//! Where each recording came from is in `data/sounds/CREDITS.md`.

use std::io::Cursor;
use std::sync::Arc;

use lewton::inside_ogg::OggStreamReader;

use super::mix::{CHANNELS, RATE};
use crate::data::ambience::{Bed, Call};

/// The loop for `bed`, as the file bundled with the app.
pub fn ogg(bed: Bed) -> &'static [u8] {
    match bed {
        Bed::Cicadas => include_bytes!("../../data/sounds/cicadas.ogg"),
        Bed::Wind => include_bytes!("../../data/sounds/wind.ogg"),
        Bed::Alpine => include_bytes!("../../data/sounds/alpine.ogg"),
    }
}

/// Takes of a call, as the files bundled with the app.
macro_rules! takes {
    ($call:literal, $($n:literal),+) => {
        &[$(include_bytes!(concat!("../../data/sounds/calls/", $call, "_", $n, ".ogg"))),+]
    };
}

/// The takes of `call`, as the files bundled with the app.
pub fn takes(call: Call) -> &'static [&'static [u8]] {
    match call {
        Call::Goat => takes!("goat", 1, 2, 3, 4, 5, 6),
        Call::Gull => takes!("gull", 1, 2, 3, 4, 5, 6, 7),
        Call::Eagle => takes!("eagle", 1, 2, 3, 4),
        Call::Hawk => takes!("hawk", 1, 2, 3, 4, 5),
        Call::Elk => takes!("elk", 1, 2, 3),
        Call::Raven => takes!("raven", 1, 2, 3),
        Call::Songbird => takes!("songbird", 1, 2, 3, 4, 5, 6),
        Call::Blackbird => takes!("blackbird", 1, 2, 3),
        Call::Rail => takes!("rail", 1, 2, 3, 4),
        Call::Frog => takes!("frog", 1, 2, 3, 4),
        Call::Crane => takes!("crane", 1, 2, 3),
        Call::Goose => takes!("goose", 1, 2, 3),
        Call::Loon => takes!("loon", 1, 2, 3, 4),
    }
}

/// Where one sound's recording came from — `data/sounds/sources.json`.
#[derive(Debug, serde::Deserialize)]
struct Source {
    title: String,
    author: String,
    page: String,
}

/// `data/sounds/sources.json`: what every bed and call is cut from.
#[derive(Debug, serde::Deserialize)]
struct Sources {
    beds: std::collections::BTreeMap<String, Source>,
    calls: std::collections::BTreeMap<String, Source>,
}

fn sources() -> Result<Sources, serde_json::Error> {
    serde_json::from_str(include_str!("../../data/sounds/sources.json"))
}

/// The recordists of the sounds, each once, as the About dialog lists
/// people: "name url".
pub fn credits() -> Vec<String> {
    let sources = match sources() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("The sound credits would not read: {e}");
            return Vec::new();
        }
    };
    let mut out: Vec<String> = Vec::new();
    for s in sources.beds.values().chain(sources.calls.values()) {
        let line = format!("{} {}", s.author, s.page);
        if !s.title.is_empty() && !out.contains(&line) {
            out.push(line);
        }
    }
    out.sort();
    out
}

/// Why a loop could not be decoded.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("not a Vorbis stream: {0}")]
    Vorbis(#[from] lewton::VorbisError),
    #[error("{rate} Hz × {channels} channels, not {RATE} Hz × {want}")]
    Format {
        rate: u32,
        channels: u8,
        want: usize,
    },
    #[error("no sound in it")]
    Empty,
}

/// Decodes an Ogg Vorbis loop to interleaved stereo frames at [`RATE`]. The
/// bundled files are made that way; anything else is refused, not resampled.
pub fn decode(bytes: &[u8]) -> Result<Arc<[i16]>, DecodeError> {
    decode_as(bytes, CHANNELS)
}

/// Decodes an Ogg Vorbis take of a call to mono frames at [`RATE`]; anything
/// else is refused.
pub fn decode_take(bytes: &[u8]) -> Result<Arc<[i16]>, DecodeError> {
    decode_as(bytes, 1)
}

fn decode_as(bytes: &[u8], want_channels: usize) -> Result<Arc<[i16]>, DecodeError> {
    let mut reader = OggStreamReader::new(Cursor::new(bytes))?;
    let (rate, channels) = (
        reader.ident_hdr.audio_sample_rate,
        reader.ident_hdr.audio_channels,
    );
    if rate != RATE || channels as usize != want_channels {
        return Err(DecodeError::Format {
            rate,
            channels,
            want: want_channels,
        });
    }
    let mut pcm = Vec::new();
    while let Some(packet) = reader.read_dec_packet_itl()? {
        pcm.extend_from_slice(&packet);
    }
    if pcm.len() < want_channels {
        return Err(DecodeError::Empty);
    }
    Ok(pcm.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_decode_every_bundled_loop_to_a_long_stereo_loop() {
        for bed in Bed::ALL {
            let pcm = decode(ogg(bed)).unwrap_or_else(|e| panic!("{bed:?}: {e}"));
            let seconds = pcm.len() as f32 / (RATE as f32 * CHANNELS as f32);
            // Long enough that its turn round is not heard as a pattern.
            assert!(seconds >= 30.0, "{bed:?} is {seconds} s long");
            assert!(seconds <= 120.0, "{bed:?} is {seconds} s long");
            assert_eq!(pcm.len() % CHANNELS, 0, "{bed:?}");
        }
    }

    #[test]
    fn should_join_every_loop_end_to_start_without_a_click() {
        // The last frame runs into the first: the step between them is no
        // bigger than the loop's own steps from one frame to the next.
        for bed in Bed::ALL {
            let pcm = decode(ogg(bed)).unwrap_or_else(|e| panic!("{bed:?}: {e}"));
            let n = pcm.len();
            let step = |a: usize, b: usize| (pcm[a] as i32 - pcm[b] as i32).abs();
            let join = step(n - CHANNELS, 0).max(step(n - 1, 1));
            let mut steps: Vec<i32> = (CHANNELS..n)
                .step_by(CHANNELS)
                .map(|i| step(i, i - CHANNELS))
                .collect();
            steps.sort_unstable();
            let typical = steps[steps.len() * 999 / 1000];
            assert!(
                join <= typical.max(64),
                "{bed:?}: joins with a step of {join}, typical {typical}"
            );
        }
    }

    #[test]
    fn should_not_open_the_bundled_loops_too_loud_or_too_quiet() {
        // The loops are levelled when they are made; one far off the others
        // would blare or vanish crossing a border.
        for bed in Bed::ALL {
            let pcm = decode(ogg(bed)).unwrap_or_else(|e| panic!("{bed:?}: {e}"));
            let rms = (pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / pcm.len() as f64)
                .sqrt()
                / 32_768.0;
            let db = 20.0 * rms.log10();
            assert!((-34.0..=-14.0).contains(&db), "{bed:?} at {db:.1} dBFS");
        }
    }

    #[test]
    fn should_credit_the_recording_of_every_bundled_loop() {
        let sources = sources().unwrap();
        let credits_md = include_str!("../../data/sounds/CREDITS.md");
        for bed in Bed::ALL {
            let key = serde_json::to_value(bed).unwrap();
            let key = key.as_str().unwrap();
            let s = sources
                .beds
                .get(key)
                .unwrap_or_else(|| panic!("no source for {key}"));
            assert!(
                credits_md.contains(&format!("`{key}.ogg`")),
                "{key} not in CREDITS.md"
            );
            assert!(
                credits_md.contains(&s.author),
                "{} not in CREDITS.md",
                s.author
            );
            assert!(credits().contains(&format!("{} {}", s.author, s.page)));
        }
        assert_eq!(
            sources.beds.len(),
            Bed::ALL.len(),
            "a source no loop is made from"
        );
    }

    #[test]
    fn should_credit_every_call_and_bundle_every_take_it_lists() {
        let sources = sources().unwrap();
        let credits_md = include_str!("../../data/sounds/CREDITS.md");
        for call in Call::ALL {
            let key = serde_json::to_value(call).unwrap();
            let key = key.as_str().unwrap();
            let s = sources
                .calls
                .get(key)
                .unwrap_or_else(|| panic!("no source for {key}"));
            // The takes made are the takes bundled: one added to the list but
            // not to `takes`, or the other way, would ship unheard or fail.
            let listed: serde_json::Value =
                serde_json::from_str(include_str!("../../data/sounds/sources.json")).unwrap();
            let listed = listed["calls"][key]["takes"].as_array().map_or(0, Vec::len);
            assert_eq!(listed, takes(call).len(), "{key}");
            assert!(
                credits_md.contains(&format!("`calls/{key}_*.ogg`")),
                "{key} not in CREDITS.md"
            );
            assert!(credits().contains(&format!("{} {}", s.author, s.page)));
        }
        assert_eq!(sources.calls.len(), Call::ALL.len(), "a call no one makes");
    }

    #[test]
    fn should_decode_every_take_of_every_call_to_a_short_mono_call() {
        for call in Call::ALL {
            for (n, bytes) in takes(call).iter().enumerate() {
                let pcm = decode_take(bytes).unwrap_or_else(|e| panic!("{call:?} {n}: {e}"));
                let seconds = pcm.len() as f32 / RATE as f32;
                assert!((0.3..=6.0).contains(&seconds), "{call:?} {n}: {seconds} s");
                // It starts and ends in silence, so it never clicks in or out.
                let edge = |i: usize| (pcm[i] as i32).abs();
                assert!(edge(0) < 64 && edge(pcm.len() - 1) < 64, "{call:?} {n}");
                // And a loop is no take: the stereo beds are refused here.
                assert!(decode(bytes).is_err());
            }
        }
        assert!(decode_take(ogg(Bed::Wind)).is_err());
    }

    #[test]
    fn should_refuse_what_is_not_vorbis() {
        assert!(decode(b"").is_err());
        assert!(decode(b"OggS but not really a stream at all").is_err());
        assert!(decode(&[0u8; 4096]).is_err());
    }

    #[test]
    fn should_refuse_a_truncated_loop_without_panicking() {
        for bed in Bed::ALL {
            let whole = ogg(bed);
            for cut in [
                0,
                10,
                100,
                1_000,
                whole.len() / 3,
                whole.len().saturating_sub(1),
            ] {
                // Either refused or decoded short — never a panic.
                let _ = decode(&whole[..cut.min(whole.len())]);
            }
        }
    }
}
