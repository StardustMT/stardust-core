//! # stardust-audio
//!
//! Real-time audio output for stardust-core. Wraps [`cpal`] (CoreAudio /
//! WASAPI / ALSA + ASIO opt-in) behind a small API focused on the live-
//! performance use case: open a device, fill an interleaved `f32` buffer
//! on the audio thread.
//!
//! # Quickstart
//!
//! ```no_run
//! use stardust_audio::{open_default_output, AudioSpec};
//!
//! // Play a 1-second 440 Hz sine on the default output.
//! let mut phase: f32 = 0.0;
//! let _handle = open_default_output(None, move |buf: &mut [f32], spec: &AudioSpec| {
//!     let advance = 440.0 * std::f32::consts::TAU / spec.sample_rate as f32;
//!     let channels = spec.channels as usize;
//!     for frame in buf.chunks_exact_mut(channels) {
//!         let s = (phase).sin() * 0.2;
//!         for ch in frame { *ch = s; }
//!         phase += advance;
//!         if phase > std::f32::consts::TAU { phase -= std::f32::consts::TAU; }
//!     }
//! }).unwrap();
//!
//! std::thread::sleep(std::time::Duration::from_secs(1));
//! ```
//!
//! The callback runs on a dedicated audio thread at platform-managed
//! priority. **Do not allocate, lock, or perform I/O inside it.** Use a
//! lock-free queue (see `stardust-rt`) to communicate with non-audio
//! threads.

#![doc(html_root_url = "https://docs.rs/stardust-audio/0.0.1")]
#![warn(missing_docs)]

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Stream, StreamConfig};
use thiserror::Error;

/// Errors returned by this crate.
#[derive(Error, Debug)]
pub enum AudioError {
    /// No default output device on this host.
    #[error("no default output device available")]
    NoDefaultOutput,

    /// Named output device not found.
    #[error("output device not found: {0}")]
    OutputNotFound(String),

    /// Couldn't query supported configurations for a device.
    #[error("failed to query device capabilities: {0}")]
    DeviceQuery(String),

    /// Couldn't build an output stream with the requested config.
    #[error("failed to build output stream: {0}")]
    BuildStream(#[from] cpal::BuildStreamError),

    /// Couldn't start an output stream.
    #[error("failed to start stream: {0}")]
    PlayStream(#[from] cpal::PlayStreamError),

    /// The device doesn't support an `f32` output sample format.
    #[error("device does not support f32 output (sample format: {0:?})")]
    UnsupportedSampleFormat(cpal::SampleFormat),
}

/// Metadata for a single audio output device.
#[derive(Debug, Clone)]
pub struct AudioOutputInfo {
    /// OS-reported device name.
    pub name: String,
    /// True if this is the host's default output.
    pub is_default: bool,
}

/// The audio stream's runtime configuration. Passed to the callback so it
/// can render correctly regardless of what the device negotiated.
#[derive(Debug, Clone, Copy)]
pub struct AudioSpec {
    /// Sample rate in Hz (e.g. 48000).
    pub sample_rate: u32,
    /// Number of interleaved channels per frame (typically 2 for stereo).
    pub channels: u16,
}

/// Live handle to an opened audio output stream. Drop to stop + release.
pub struct AudioOutputHandle {
    _stream: Stream,
    /// The spec the device actually opened with (after negotiation).
    pub spec: AudioSpec,
}

/// Enumerate available audio output devices on the default host.
///
/// The default device (if any) is listed first with `is_default = true`.
//
// TODO: migrate from `DeviceTrait::name()` to the new `description()` /
// `id()` API once we settle on which one we want surfaced to the UI
// (description gives manufacturer + type; id is stable across reboots).
// Tracked separately so this is a behavior change, not a CI cleanup.
#[allow(deprecated)]
pub fn list_outputs() -> Result<Vec<AudioOutputInfo>, AudioError> {
    let host = cpal::default_host();
    let default_name = host
        .default_output_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();

    let devices = host
        .output_devices()
        .map_err(|e| AudioError::DeviceQuery(e.to_string()))?;

    let mut out = Vec::new();
    for d in devices {
        if let Ok(name) = d.name() {
            let is_default = !default_name.is_empty() && name == default_name;
            out.push(AudioOutputInfo { name, is_default });
        }
    }
    // Move the default to the front so callers can pick `[0]` for "best guess".
    out.sort_by_key(|info| std::cmp::Reverse(info.is_default));
    Ok(out)
}

/// Open the host's default output device.
///
/// `preferred_sample_rate` is honoured if the device supports it, otherwise
/// the device's own default is used.
pub fn open_default_output<F>(
    preferred_sample_rate: Option<u32>,
    callback: F,
) -> Result<AudioOutputHandle, AudioError>
where
    F: FnMut(&mut [f32], &AudioSpec) + Send + 'static,
{
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(AudioError::NoDefaultOutput)?;
    open_on_device(device, preferred_sample_rate, callback)
}

/// Open a named output device.
#[allow(deprecated)]
pub fn open_output<F>(
    device_name: &str,
    preferred_sample_rate: Option<u32>,
    callback: F,
) -> Result<AudioOutputHandle, AudioError>
where
    F: FnMut(&mut [f32], &AudioSpec) + Send + 'static,
{
    let host = cpal::default_host();
    let device = host
        .output_devices()
        .map_err(|e| AudioError::DeviceQuery(e.to_string()))?
        .find(|d| d.name().ok().as_deref() == Some(device_name))
        .ok_or_else(|| AudioError::OutputNotFound(device_name.to_string()))?;
    open_on_device(device, preferred_sample_rate, callback)
}

fn open_on_device<F>(
    device: Device,
    preferred_sample_rate: Option<u32>,
    mut callback: F,
) -> Result<AudioOutputHandle, AudioError>
where
    F: FnMut(&mut [f32], &AudioSpec) + Send + 'static,
{
    // Pick an f32 output config matching the preferred sample rate when possible.
    let supported = device
        .supported_output_configs()
        .map_err(|e| AudioError::DeviceQuery(e.to_string()))?
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32)
        .collect::<Vec<_>>();

    let default_config = device
        .default_output_config()
        .map_err(|e| AudioError::DeviceQuery(e.to_string()))?;

    if default_config.sample_format() != cpal::SampleFormat::F32 && supported.is_empty() {
        return Err(AudioError::UnsupportedSampleFormat(
            default_config.sample_format(),
        ));
    }

    // Choose the config: try to honour preferred_sample_rate, fall back to default.
    // cpal 0.17 made SampleRate a plain `u32` (was a tuple newtype in 0.16).
    let chosen_config: StreamConfig = match preferred_sample_rate {
        Some(want) => supported
            .iter()
            .find(|c| c.min_sample_rate() <= want && c.max_sample_rate() >= want)
            .map(|c| (*c).with_sample_rate(want).config())
            .unwrap_or_else(|| default_config.config()),
        None => default_config.config(),
    };

    let spec = AudioSpec {
        sample_rate: chosen_config.sample_rate,
        channels: chosen_config.channels,
    };
    let spec_for_cb = spec;

    let err_fn = |err| tracing::error!("audio output stream error: {err}");

    let stream = device.build_output_stream(
        &chosen_config,
        move |data: &mut [f32], _info| {
            callback(data, &spec_for_cb);
        },
        err_fn,
        None,
    )?;
    stream.play()?;

    Ok(AudioOutputHandle {
        _stream: stream,
        spec,
    })
}
