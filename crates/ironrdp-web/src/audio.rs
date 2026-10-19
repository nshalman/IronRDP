//! Plays the server's audio in the browser, through Web Audio.
//!
//! An RDPSND processor's backend must be `Send`, and Web Audio objects are not, so the backend
//! only forwards each wave over a channel. [`play`] runs on the page's thread, owns the
//! `AudioContext`, and queues each wave to start where the one before it ends.
//!
//! A browser starts an `AudioContext` suspended until the page sees a click or a key press, so
//! playback starts at the first one; until then, waves are dropped.

use std::borrow::Cow;

use futures_channel::mpsc;
use futures_util::StreamExt as _;
use ironrdp::rdpsnd::client::RdpsndClientHandler;
use ironrdp::rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};

/// The one format offered: 16-bit stereo PCM at 48 kHz, which needs no decoding.
const RATE: u32 = 48_000;
#[cfg(target_arch = "wasm32")]
const RATE_F32: f32 = 48_000.0;
const CHANNELS: u16 = 2;

fn pcm_format() -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: CHANNELS,
        n_samples_per_sec: RATE,
        n_avg_bytes_per_sec: RATE * 4,
        n_block_align: 4,
        bits_per_sample: 16,
        data: None,
    }
}

/// One wave's 16-bit little-endian interleaved samples.
pub(crate) struct Wave {
    channels: u16,
    rate: u32,
    pcm: Vec<u8>,
}

/// The RDPSND backend: forwards each wave to [`play`].
#[derive(Debug)]
pub(crate) struct WebAudioBackend {
    formats: [AudioFormat; 1],
    waves: mpsc::UnboundedSender<Wave>,
}

/// A backend, and the receiving end [`play`] takes.
pub(crate) fn channel() -> (WebAudioBackend, mpsc::UnboundedReceiver<Wave>) {
    let (waves, rx) = mpsc::unbounded();
    (
        WebAudioBackend {
            formats: [pcm_format()],
            waves,
        },
        rx,
    )
}

impl RdpsndClientHandler for WebAudioBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn wave(&mut self, format: &AudioFormat, _ts: u32, data: Cow<'_, [u8]>) {
        if format.format != WaveFormat::PCM || format.bits_per_sample != 16 || format.n_channels == 0 {
            return;
        }
        // The receiver goes away with the session; a wave sent after that is dropped.
        let _ = self.waves.unbounded_send(Wave {
            channels: format.n_channels,
            rate: format.n_samples_per_sec,
            pcm: data.into_owned(),
        });
    }

    fn set_volume(&mut self, _volume: VolumePdu) {}

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {}
}

/// How far ahead a wave is queued when playback starts or has fallen behind, in seconds: room
/// for the next wave to arrive before this one ends.
#[cfg(target_arch = "wasm32")]
const LEAD: f64 = 0.08;

/// Waves are dropped while more than this many seconds are queued, so latency cannot grow
/// without bound (as it would while the context is still suspended).
#[cfg(target_arch = "wasm32")]
const MAX_QUEUED: f64 = 0.5;

/// Plays every wave the backend forwards, until the session's backend is dropped.
pub(crate) async fn play(mut waves: mpsc::UnboundedReceiver<Wave>) {
    #[cfg(target_arch = "wasm32")]
    {
        let ctx = match new_context() {
            Ok(ctx) => ctx,
            Err(e) => {
                tracing::warn!(?e, "Web Audio is unavailable; the session's audio is dropped");
                while waves.next().await.is_some() {}
                return;
            }
        };
        resume_on_gesture(&ctx);
        let mut next = 0.0;
        while let Some(wave) = waves.next().await {
            if let Err(e) = schedule(&ctx, &wave, &mut next) {
                tracing::debug!(?e, "A wave was not played");
            }
        }
        let _ = ctx.close();
    }
    #[cfg(not(target_arch = "wasm32"))]
    while waves.next().await.is_some() {}
}

#[cfg(target_arch = "wasm32")]
fn new_context() -> Result<web_sys::AudioContext, wasm_bindgen::JsValue> {
    let options = web_sys::AudioContextOptions::new();
    options.set_sample_rate(RATE_F32);
    web_sys::AudioContext::new_with_context_options(&options)
}

/// Resume `ctx` now, which works when the page already saw a click or a key press, and again at
/// the next one, which a login from a link has not seen yet.
#[cfg(target_arch = "wasm32")]
fn resume_on_gesture(ctx: &web_sys::AudioContext) {
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::closure::Closure;

    let _ = ctx.resume();
    let Some(window) = web_sys::window() else {
        return;
    };
    let ctx = ctx.clone();
    let resume = Closure::<dyn FnMut()>::new(move || {
        let _ = ctx.resume();
    });
    for event in ["pointerdown", "keydown"] {
        let _ = window.add_event_listener_with_callback_and_bool(event, resume.as_ref().unchecked_ref(), true);
    }
    // Lives as long as the page: resuming a closed context does nothing.
    resume.forget();
}

/// Queue `wave` to start at `next`, or a little ahead of now if `next` has passed, and move
/// `next` to where it ends. Dropped while too much is queued already.
#[cfg(target_arch = "wasm32")]
fn schedule(ctx: &web_sys::AudioContext, wave: &Wave, next: &mut f64) -> Result<(), wasm_bindgen::JsValue> {
    // Only the one rate is offered, so only it is played.
    if wave.rate != RATE {
        return Ok(());
    }
    let channels = usize::from(wave.channels);
    let frames = wave.pcm.len() / (2 * channels);
    let Ok(frame_count) = u32::try_from(frames) else {
        return Ok(());
    };
    if frame_count == 0 {
        return Ok(());
    }
    let now = ctx.current_time();
    if *next < now {
        *next = now + LEAD;
    }
    if *next - now > MAX_QUEUED {
        return Ok(());
    }

    let buffer = ctx.create_buffer(u32::from(wave.channels), frame_count, RATE_F32)?;
    let mut plane = vec![0f32; frames];
    for (channel, number) in (0..channels).zip(0i32..) {
        for (frame, sample) in plane.iter_mut().enumerate() {
            let at = (frame * channels + channel) * 2;
            let value = i16::from_le_bytes([wave.pcm[at], wave.pcm[at + 1]]);
            *sample = f32::from(value) / 32768.0;
        }
        buffer.copy_to_channel(&plane, number)?;
    }
    let source = ctx.create_buffer_source()?;
    source.set_buffer(Some(&buffer));
    source.connect_with_audio_node(&ctx.destination())?;
    source.start_with_when(*next)?;
    *next += f64::from(frame_count) / f64::from(RATE);
    Ok(())
}
