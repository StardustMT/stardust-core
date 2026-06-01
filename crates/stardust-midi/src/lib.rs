//! # stardust-midi
//!
//! MIDI input for stardust-core. Wraps [`midir`] (CoreMIDI / WinMM / ALSA)
//! behind a small, allocation-free API suitable for live-performance use.
//!
//! # Quickstart
//!
//! ```no_run
//! use stardust_midi::{list_inputs, open_input, MidiMessage};
//!
//! let inputs = list_inputs().unwrap();
//! let first = inputs.first().expect("a MIDI input device");
//!
//! let _handle = open_input(&first.name, |_timestamp_ns, msg| {
//!     if let MidiMessage::NoteOn { channel, note, velocity } = msg {
//!         println!("note on: ch {channel} note {note} vel {velocity}");
//!     }
//! }).unwrap();
//!
//! std::thread::park(); // the midir thread does the work
//! ```
//!
//! The callback runs on midir's own input thread. It must be short, must not
//! allocate or lock if its output reaches the audio thread. For audio-thread
//! delivery, push events into a lock-free queue (see `stardust-rt`).

#![doc(html_root_url = "https://docs.rs/stardust-midi/0.0.1")]
#![warn(missing_docs)]

use midir::{MidiInput, MidiInputConnection};
use thiserror::Error;

/// Errors returned by this crate.
#[derive(Error, Debug)]
pub enum MidiError {
    /// Failed to initialize the platform MIDI backend.
    #[error("MIDI backend init failed: {0}")]
    Init(#[from] midir::InitError),

    /// Failed to connect to a port that exists.
    #[error("MIDI connect failed: {0}")]
    Connect(String),

    /// The requested port wasn't present at connect time.
    #[error("MIDI input port not found: {0}")]
    PortNotFound(String),
}

/// Metadata for a single MIDI input port.
///
/// The `name` is the human-readable label assigned by the OS / device driver
/// (e.g. `"Roland RD-2000 MIDI 1"` on macOS) and also serves as the lookup
/// key when reconnecting after a hot-plug event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiInputInfo {
    /// OS-reported device name. Use as the key when opening.
    pub name: String,
}

/// A timestamped MIDI message, sized for `Copy` so it's cheap to move
/// across thread boundaries via a lock-free queue (see `stardust-rt`).
///
/// `timestamp_ns` is whatever the platform MIDI backend reports — typically
/// a monotonic nanosecond counter relative to the connection opening. For
/// POC use it's enough to know "this came in before that one"; sample-
/// accurate scheduling against the audio clock is a v0.3+ concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiEvent {
    /// Platform-reported nanosecond timestamp.
    pub timestamp_ns: u64,
    /// Parsed message.
    pub message: MidiMessage,
}

/// A parsed MIDI channel-voice message.
///
/// SysEx and realtime messages (clock, MTC, etc.) collapse into
/// [`MidiMessage::Other`] for the POC — they aren't routed to plugins by
/// the v0.x engine, but are preserved as a class so callers can count them.
///
/// Note-on with velocity 0 normalises to [`MidiMessage::NoteOff`] —
/// the spec-blessed running-status convention many controllers use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiMessage {
    /// Key pressed.
    NoteOn {
        /// 0..=15
        channel: u8,
        /// MIDI note, 0..=127
        note: u8,
        /// Velocity, 1..=127 (zero-velocity collapses to NoteOff)
        velocity: u8,
    },
    /// Key released.
    NoteOff {
        /// 0..=15
        channel: u8,
        /// MIDI note, 0..=127
        note: u8,
        /// Release velocity, 0..=127
        velocity: u8,
    },
    /// Continuous controller event (mod wheel, sustain pedal, expression, …).
    ControlChange {
        /// 0..=15
        channel: u8,
        /// CC number, 0..=127
        cc: u8,
        /// CC value, 0..=127
        value: u8,
    },
    /// 14-bit pitch wheel. Range -8192..=8191, where 0 = center.
    PitchBend {
        /// 0..=15
        channel: u8,
        /// -8192..=8191
        value: i16,
    },
    /// Channel-wide aftertouch.
    ChannelPressure {
        /// 0..=15
        channel: u8,
        /// 0..=127
        value: u8,
    },
    /// Per-key aftertouch (polyphonic pressure).
    PolyAftertouch {
        /// 0..=15
        channel: u8,
        /// 0..=127
        note: u8,
        /// 0..=127
        value: u8,
    },
    /// Program change (patch select on the device).
    ProgramChange {
        /// 0..=15
        channel: u8,
        /// 0..=127
        program: u8,
    },
    /// SysEx, realtime (clock/MTC), or anything else not handled above.
    Other,
}

impl MidiMessage {
    /// Parse a raw status+data byte slice into a [`MidiMessage`].
    ///
    /// Returns `None` only on a fully empty slice. Truncated or otherwise
    /// invalid frames return [`MidiMessage::Other`] so callers can still
    /// count them.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() {
            return None;
        }
        let status = bytes[0];
        let channel = status & 0x0F;
        let kind = status & 0xF0;

        Some(match kind {
            0x80 if bytes.len() >= 3 => Self::NoteOff {
                channel,
                note: bytes[1] & 0x7F,
                velocity: bytes[2] & 0x7F,
            },
            0x90 if bytes.len() >= 3 => {
                let velocity = bytes[2] & 0x7F;
                if velocity == 0 {
                    Self::NoteOff {
                        channel,
                        note: bytes[1] & 0x7F,
                        velocity: 0,
                    }
                } else {
                    Self::NoteOn {
                        channel,
                        note: bytes[1] & 0x7F,
                        velocity,
                    }
                }
            }
            0xA0 if bytes.len() >= 3 => Self::PolyAftertouch {
                channel,
                note: bytes[1] & 0x7F,
                value: bytes[2] & 0x7F,
            },
            0xB0 if bytes.len() >= 3 => Self::ControlChange {
                channel,
                cc: bytes[1] & 0x7F,
                value: bytes[2] & 0x7F,
            },
            0xC0 if bytes.len() >= 2 => Self::ProgramChange {
                channel,
                program: bytes[1] & 0x7F,
            },
            0xD0 if bytes.len() >= 2 => Self::ChannelPressure {
                channel,
                value: bytes[1] & 0x7F,
            },
            0xE0 if bytes.len() >= 3 => {
                let lsb = (bytes[1] & 0x7F) as u16;
                let msb = (bytes[2] & 0x7F) as u16;
                let raw = (msb << 7) | lsb;
                Self::PitchBend {
                    channel,
                    value: raw as i16 - 8192,
                }
            }
            _ => Self::Other,
        })
    }
}

/// Enumerate currently-available MIDI input ports.
///
/// Order is OS-dependent and may change between calls (e.g. on hot-plug).
pub fn list_inputs() -> Result<Vec<MidiInputInfo>, MidiError> {
    let mi = MidiInput::new("stardust-midi-list")?;
    let mut out = Vec::with_capacity(mi.port_count());
    for port in mi.ports() {
        let name = mi
            .port_name(&port)
            .unwrap_or_else(|_| "<unknown>".to_string());
        out.push(MidiInputInfo { name });
    }
    Ok(out)
}

/// Live handle to an opened MIDI input. Drop to disconnect.
///
/// While held, the closure passed to [`open_input`] runs on midir's input
/// thread for every incoming MIDI event.
pub struct MidiInputHandle {
    _connection: MidiInputConnection<()>,
}

/// Open an input port by name and start delivering parsed messages.
///
/// The `callback` runs on midir's input thread. Its first argument is the
/// MIDI timestamp in nanoseconds since the connection opened (precise across
/// platforms via midir).
///
/// Returns an error if the named port doesn't exist or the OS refuses to
/// open it. SysEx is *not* filtered at this layer — the callback may see
/// [`MidiMessage::Other`] for non-channel-voice traffic.
pub fn open_input<F>(port_name: &str, mut callback: F) -> Result<MidiInputHandle, MidiError>
where
    F: FnMut(u64, MidiMessage) + Send + 'static,
{
    let mut mi = MidiInput::new("stardust-midi-input")?;
    mi.ignore(midir::Ignore::None);

    let target = mi
        .ports()
        .into_iter()
        .find(|p| mi.port_name(p).ok().as_deref() == Some(port_name))
        .ok_or_else(|| MidiError::PortNotFound(port_name.to_string()))?;

    let connection = mi
        .connect(
            &target,
            "stardust-midi",
            move |timestamp_ns, bytes, _| {
                if let Some(msg) = MidiMessage::parse(bytes) {
                    callback(timestamp_ns, msg);
                }
            },
            (),
        )
        .map_err(|e| MidiError::Connect(format!("{e:?}")))?;

    Ok(MidiInputHandle {
        _connection: connection,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_note_on() {
        let bytes = [0x90, 60, 100];
        assert_eq!(
            MidiMessage::parse(&bytes),
            Some(MidiMessage::NoteOn {
                channel: 0,
                note: 60,
                velocity: 100
            })
        );
    }

    #[test]
    fn note_on_velocity_zero_is_note_off() {
        let bytes = [0x91, 60, 0];
        assert_eq!(
            MidiMessage::parse(&bytes),
            Some(MidiMessage::NoteOff {
                channel: 1,
                note: 60,
                velocity: 0
            })
        );
    }

    #[test]
    fn parses_cc() {
        let bytes = [0xB3, 64, 127];
        assert_eq!(
            MidiMessage::parse(&bytes),
            Some(MidiMessage::ControlChange {
                channel: 3,
                cc: 64,
                value: 127
            })
        );
    }

    #[test]
    fn parses_pitch_bend_center() {
        let bytes = [0xE0, 0, 64]; // raw 8192 = center
        assert_eq!(
            MidiMessage::parse(&bytes),
            Some(MidiMessage::PitchBend {
                channel: 0,
                value: 0
            })
        );
    }

    #[test]
    fn parses_pitch_bend_extremes() {
        assert_eq!(
            MidiMessage::parse(&[0xE0, 127, 127]),
            Some(MidiMessage::PitchBend {
                channel: 0,
                value: 8191
            })
        );
        assert_eq!(
            MidiMessage::parse(&[0xE0, 0, 0]),
            Some(MidiMessage::PitchBend {
                channel: 0,
                value: -8192
            })
        );
    }

    #[test]
    fn empty_slice_is_none() {
        assert_eq!(MidiMessage::parse(&[]), None);
    }

    #[test]
    fn truncated_channel_voice_is_other() {
        assert_eq!(MidiMessage::parse(&[0x90, 60]), Some(MidiMessage::Other));
    }
}
