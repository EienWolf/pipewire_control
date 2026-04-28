//! In-process clip playback for the soundboard.
//!
//! Decodes audio files in the calling (Tokio) thread, then hands an
//! `Arc<Vec<f32>>` of interleaved stereo samples to the PipeWire engine
//! thread, which creates a short-lived `StreamRc` per playback.
//!
//! Volume and loop are mutated live through atomics shared with the RT
//! process callback — no command round-trip needed for those.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Decoded audio: stereo, interleaved f32, at the source sample rate.
/// PipeWire will resample to the sink's rate as part of normal graph routing.
pub struct DecodedClip {
    pub samples: Arc<Vec<f32>>,
    pub sample_rate: u32,
}

impl DecodedClip {
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 {
            return 0;
        }
        let frames = (self.samples.len() / 2) as u64;
        frames * 1000 / self.sample_rate as u64
    }
}

/// Decode any symphonia-supported audio file into stereo interleaved f32.
/// Mono inputs are duplicated to L/R; sources with >2 channels are downmixed
/// to L/R (drops surround channels).
pub fn decode_to_stereo_f32(path: &Path) -> Result<DecodedClip> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {} for decode", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .with_context(|| format!("probing {}", path.display()))?;
    let mut format = probed.format;

    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("no default track"))?
        .clone();
    let track_id = track.id;

    let codec_params = track.codec_params;
    let sample_rate = codec_params
        .sample_rate
        .ok_or_else(|| anyhow!("missing sample rate"))?;
    let channels = codec_params
        .channels
        .ok_or_else(|| anyhow!("missing channel layout"))?
        .count();

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .context("making decoder")?;

    let mut out: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(e) => return Err(anyhow!("packet read: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(anyhow!("decode: {e}")),
        };

        if sample_buf.is_none() {
            let spec = *decoded.spec();
            let duration = decoded.capacity() as u64;
            sample_buf = Some(SampleBuffer::<f32>::new(duration, spec));
        }
        if let Some(buf) = sample_buf.as_mut() {
            buf.copy_interleaved_ref(decoded);
            let interleaved = buf.samples();
            mix_to_stereo(interleaved, channels, &mut out);
        }
    }

    if out.is_empty() {
        return Err(anyhow!("decoded zero samples"));
    }

    Ok(DecodedClip {
        samples: Arc::new(out),
        sample_rate,
    })
}

fn mix_to_stereo(interleaved: &[f32], src_channels: usize, out: &mut Vec<f32>) {
    if src_channels == 0 {
        return;
    }
    let frames = interleaved.len() / src_channels;
    out.reserve(frames * 2);
    match src_channels {
        1 => {
            for &s in interleaved {
                out.push(s);
                out.push(s);
            }
        }
        2 => out.extend_from_slice(interleaved),
        n => {
            // Take first two channels (front L/R) — typical for surround sources.
            for i in 0..frames {
                let base = i * n;
                out.push(interleaved[base]);
                out.push(interleaved[base + 1]);
            }
        }
    }
}

/// Live-mutable knobs for an active playback. Held by both the web layer
/// (which mutates) and the RT process callback (which reads).
pub struct PlaybackKnobs {
    pub volume_bits: AtomicU32, // f32 stored as u32 bits
    pub loop_mode: AtomicBool,
    pub stop_requested: AtomicBool,
}

impl PlaybackKnobs {
    pub fn new(volume: f32, loop_mode: bool) -> Arc<Self> {
        Arc::new(Self {
            volume_bits: AtomicU32::new(volume.to_bits()),
            loop_mode: AtomicBool::new(loop_mode),
            stop_requested: AtomicBool::new(false),
        })
    }
    pub fn set_volume(&self, v: f32) {
        self.volume_bits
            .store(v.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(std::sync::atomic::Ordering::Relaxed))
    }
    pub fn set_loop(&self, l: bool) {
        self.loop_mode
            .store(l, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn looped(&self) -> bool {
        self.loop_mode.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn request_stop(&self) {
        self.stop_requested
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn stop_requested(&self) -> bool {
        self.stop_requested
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Public DTO mirrored to the web layer for active playback listings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackInfo {
    pub playback_id: String,
    pub clip_id: String,
    pub target_node_name: Option<String>,
    pub volume: f32,
    pub loop_mode: bool,
    pub duration_ms: u64,
    pub started_at: u64,
}
