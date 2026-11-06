//! The app's one ambient player: a stream to the sound server, opened the
//! first time a ride is heard and left resting — corked — while it is
//! silent, so an idle app keeps no sound card awake.
//!
//! Called from the GTK main thread; the connection, the decoding and the
//! mixing all happen on the tokio runtime and the sound server's thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use pulseaudio::{protocol, AsPlaybackSource, Client};

use super::beds;
use super::mix::{Mixer, CHANNELS, RATE};
use crate::data::ambience::{Cry, Gains, BED_COUNT, CALL_COUNT};

/// How much sound the server keeps queued: short, so a fade asked for is
/// heard at once, long enough that a busy moment does not break it up.
const QUEUED_S: f32 = 0.25;
/// How often the player looks whether the mix has gone quiet or woken.
const LOOK_EVERY: Duration = Duration::from_millis(500);
/// Silent this long, the stream is corked; this long, the loops are let go.
const CORK_AFTER: Duration = Duration::from_secs(10);
const FORGET_AFTER: Duration = Duration::from_secs(60);
/// Silent this long with no one to play for, the connection is closed.
const CLOSE_AFTER: Duration = Duration::from_secs(300);
/// After a failed connection, the next try waits this long.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Who is asking for sound — each 3D view has its own, so one closing does
/// not silence another that took over.
pub type Owner = u64;

/// A new owner, never handed out before.
pub fn next_owner() -> Owner {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub struct Ambience {
    mixer: Arc<Mutex<Mixer>>,
    control: Mutex<Control>,
    rt: tokio::runtime::Handle,
}

/// A call's takes, decoded: mono frames each.
type Takes = Arc<[Arc<[i16]>]>;

#[derive(Default)]
struct Control {
    owner: Option<Owner>,
    volume: f32,
    /// Whether the connection task is up.
    running: bool,
    /// The beds being decoded right now.
    loading: [bool; BED_COUNT],
    /// The beds that would not decode: not tried again.
    broken: [bool; BED_COUNT],
    /// Every call's takes, once decoded — kept for the app's life: a few
    /// seconds each, and the mix never frees one on the server's thread.
    takes: [Option<Takes>; CALL_COUNT],
    /// The calls being decoded right now.
    takes_loading: [bool; CALL_COUNT],
}

static AMBIENCE: OnceLock<Ambience> = OnceLock::new();

/// Sets up the player at `volume` (0 to 1, 0 for off). Nothing connects
/// until something is played.
pub fn init(rt: tokio::runtime::Handle, volume: f32) {
    let player = AMBIENCE.get_or_init(|| Ambience {
        mixer: Arc::new(Mutex::new(Mixer::new())),
        control: Mutex::new(Control::default()),
        rt,
    });
    player.set_volume(volume);
}

/// The player, once [`init`] has run.
pub fn get() -> Option<&'static Ambience> {
    AMBIENCE.get()
}

/// A lock that a panic elsewhere does not poison: the mix is plain numbers,
/// fit to carry on with whatever it held.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Ambience {
    /// How loud the ambience plays, from 0 (off) to 1.
    pub fn set_volume(&'static self, volume: f32) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let owned = {
            let mut c = lock(&self.control);
            c.volume = volume;
            c.owner.is_some()
        };
        if owned {
            lock(&self.mixer).set_master(volume);
            self.wake();
        }
    }

    /// `owner` is showing a ride whose land sounds as `gains` says. Called
    /// a few times a second while it is on screen.
    pub fn play(&'static self, owner: Owner, gains: &Gains) {
        let volume = {
            let mut c = lock(&self.control);
            c.owner = Some(owner);
            c.volume
        };
        {
            let mut m = lock(&self.mixer);
            m.set_targets(gains);
            m.set_master(volume);
        }
        if volume > 0.0 {
            self.wake();
        }
    }

    /// `owner` hears `cry` now — or, the first time its call is asked for,
    /// as soon as its takes are decoded.
    pub fn cry(&'static self, owner: Owner, cry: &Cry) {
        let i = cry.call as usize;
        let takes = {
            let mut c = lock(&self.control);
            if c.owner != Some(owner) || c.volume == 0.0 {
                return;
            }
            match &c.takes[i] {
                Some(t) => Arc::clone(t),
                None => {
                    if !c.takes_loading[i] {
                        c.takes_loading[i] = true;
                        self.load_takes(owner, *cry);
                    }
                    return;
                }
            }
        };
        self.shoot(&takes, cry);
    }

    /// Plays one of `takes`, as `cry` picks.
    fn shoot(&'static self, takes: &Takes, cry: &Cry) {
        if takes.is_empty() {
            return;
        }
        let take = Arc::clone(&takes[cry.take as usize % takes.len()]);
        lock(&self.mixer).shoot(take, cry.gain, cry.pan);
        self.wake();
    }

    /// Decodes every take of `first`'s call, off the main thread, then
    /// plays `first` if `owner` is still the one heard. A take that will not
    /// decode is left out, said once.
    fn load_takes(&'static self, owner: Owner, first: Cry) {
        let call = first.call;
        self.rt.spawn(async move {
            let decoded = tokio::task::spawn_blocking(move || {
                beds::takes(call)
                    .iter()
                    .filter_map(|bytes| match beds::decode_take(bytes) {
                        Ok(pcm) => Some(pcm),
                        Err(e) => {
                            tracing::warn!("A {call:?} call would not decode: {e}");
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            let takes: Takes = decoded.into();
            let still_heard = {
                let mut c = lock(&self.control);
                c.takes_loading[call as usize] = false;
                c.takes[call as usize] = Some(Arc::clone(&takes));
                c.owner == Some(owner) && c.volume > 0.0
            };
            if still_heard {
                self.shoot(&takes, &first);
            }
        });
    }

    /// `owner` is gone from the screen: its sound fades out, unless another
    /// has taken over since.
    pub fn release(&self, owner: Owner) {
        let mut c = lock(&self.control);
        if c.owner == Some(owner) {
            c.owner = None;
            lock(&self.mixer).set_master(0.0);
        }
    }

    /// Decodes the loops now wanted and makes sure the stream is up.
    fn wake(&'static self) {
        let wanted = lock(&self.mixer).wanted();
        let mut c = lock(&self.control);
        for bed in wanted {
            let i = bed as usize;
            if c.loading[i] || c.broken[i] {
                continue;
            }
            c.loading[i] = true;
            self.rt.spawn(async move {
                let decoded = tokio::task::spawn_blocking(move || beds::decode(beds::ogg(bed)))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()));
                let mut c = lock(&self.control);
                c.loading[i] = false;
                match decoded {
                    Ok(pcm) => lock(&self.mixer).install(bed, pcm),
                    Err(e) => {
                        c.broken[i] = true;
                        tracing::warn!("The {bed:?} ambience would not decode: {e}");
                    }
                }
            });
        }
        if !c.running {
            c.running = true;
            self.rt.spawn(self.run());
        }
    }

    /// Keeps a stream to the sound server while there is sound to play.
    async fn run(&'static self) {
        loop {
            match self.stream_while_wanted().await {
                Ok(()) => {
                    lock(&self.control).running = false;
                    return;
                }
                Err(e) => {
                    // Once per failure, not per frame: no sound server in a
                    // sandbox without the socket, or one that went away.
                    tracing::warn!("No ambient sound: {e}");
                    tokio::time::sleep(RETRY_AFTER).await;
                    let c = lock(&self.control);
                    if c.owner.is_none() || c.volume == 0.0 {
                        drop(c);
                        lock(&self.control).running = false;
                        return;
                    }
                }
            }
        }
    }

    /// Streams the mix until it has been silent with no one to play for
    /// long enough to close; errs when the server cannot be reached or the
    /// connection breaks.
    async fn stream_while_wanted(&'static self) -> Result<(), pulseaudio::ClientError> {
        let client = tokio::task::spawn_blocking(|| Client::from_env(c"Tour du Salon"))
            .await
            .map_err(|e| pulseaudio::ClientError::Io(std::io::Error::other(e)))??;
        let mut props = protocol::Props::new();
        props.set(protocol::Prop::MediaName, c"Ambient sound");
        props.set(protocol::Prop::MediaRole, c"game");
        props.set(
            protocol::Prop::ApplicationIconName,
            c"io.github.rorynuijens.Cycle",
        );
        let frame_bytes = (CHANNELS * std::mem::size_of::<f32>()) as u32;
        let params = protocol::PlaybackStreamParams {
            sample_spec: protocol::SampleSpec {
                format: protocol::SampleFormat::Float32Le,
                channels: CHANNELS as u8,
                sample_rate: RATE,
            },
            channel_map: protocol::ChannelMap::stereo(),
            cvolume: Some(protocol::ChannelVolume::norm(CHANNELS as u8)),
            sink_name: Some(protocol::DEFAULT_SINK.to_owned()),
            buffer_attr: protocol::stream::BufferAttr {
                target_length: (QUEUED_S * RATE as f32) as u32 * frame_bytes,
                ..Default::default()
            },
            flags: protocol::stream::StreamFlags {
                adjust_latency: true,
                ..Default::default()
            },
            props,
            ..Default::default()
        };
        let mixer = Arc::clone(&self.mixer);
        let mut scratch: Vec<f32> = Vec::new();
        let fill = move |buf: &mut [u8]| {
            let frames = buf.len() / frame_bytes as usize;
            if frames == 0 {
                buf.fill(0);
                return buf.len();
            }
            scratch.resize(frames * CHANNELS, 0.0);
            lock(&mixer).fill(&mut scratch);
            for (bytes, s) in buf.as_chunks_mut::<4>().0.iter_mut().zip(&scratch) {
                bytes.copy_from_slice(&s.to_le_bytes());
            }
            frames * frame_bytes as usize
        };
        let stream = client
            .create_playback_stream(params, fill.as_playback_source())
            .await?;
        tracing::info!("Ambient sound stream open");

        let mut corked = false;
        let mut quiet_for = Duration::ZERO;
        loop {
            tokio::time::sleep(LOOK_EVERY).await;
            let (resting, freed) = {
                let mut m = lock(&self.mixer);
                let resting = m.is_resting();
                let freed = if resting && quiet_for >= FORGET_AFTER {
                    m.evict_silent()
                } else {
                    Vec::new()
                };
                (resting, freed)
            };
            // Dropped here, off the sound server's thread.
            drop(freed);
            quiet_for = if resting {
                quiet_for + LOOK_EVERY
            } else {
                Duration::ZERO
            };
            if resting && !corked && quiet_for >= CORK_AFTER {
                stream.cork().await?;
                let heard = lock(&self.mixer).heard_frames();
                tracing::debug!("Ambient sound stream resting, {heard} frames heard so far");
                corked = true;
            } else if !resting && corked {
                stream.uncork().await?;
                tracing::debug!("Ambient sound stream woken");
                corked = false;
            } else if !corked {
                // Asks the server something now and then, so a connection
                // that broke is noticed and opened again.
                stream.timing_info().await?;
            }
            if corked && quiet_for >= CLOSE_AFTER && lock(&self.control).owner.is_none() {
                stream.delete().await?;
                tracing::info!("Ambient sound stream closed");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::ambience::{Bed, Call};

    /// Plays, falls quiet long enough for the stream to cork, and plays
    /// again — against the real sound server, at a volume nobody hears.
    /// Needs one: `cargo test -- --ignored should_play_again_after_resting`.
    #[test]
    #[ignore]
    fn should_play_again_after_resting() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let player: &'static Ambience = Box::leak(Box::new(Ambience {
            mixer: Arc::new(Mutex::new(Mixer::new())),
            control: Mutex::new(Control::default()),
            rt: rt.handle().clone(),
        }));
        player.set_volume(0.001);
        let mut gains = [0.0; BED_COUNT];
        gains[Bed::Wind as usize] = 1.0;
        let heard = || lock(&player.mixer).heard_frames();
        let ride = |label: &str| {
            let before = heard();
            for _ in 0..12 {
                player.play(7, &gains);
                std::thread::sleep(Duration::from_millis(250));
            }
            let after = heard();
            eprintln!("{label}: {} frames heard", after - before);
            assert!(after - before > RATE as u64, "{label}: nothing heard");
        };
        ride("first ride");
        // A call is decoded on its first asking, and heard once it is.
        let gull = Cry {
            call: Call::Gull,
            take: 3,
            gain: 1.0,
            pan: 0.5,
        };
        player.cry(7, &gull);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(lock(&player.mixer).calls_playing(), 1);
        player.release(7);
        std::thread::sleep(CORK_AFTER + Duration::from_secs(3));
        ride("second ride");
    }
}
