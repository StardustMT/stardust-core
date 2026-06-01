//! # stardust-sfz
//!
//! Built-in CLAP SFZ sampler plugin for Stardust. First-party plugin
//! living alongside the host so we can dogfood our CLAP toolchain
//! end-to-end without external dependencies.
//!
//! Compiles to a `.clap` cdylib that any CLAP host (Stardust included)
//! can load. Also exposes its parser + engine as a regular Rust library
//! so the offline render example and tests can drive it without the
//! plugin shell.
//!
//! ## Loading an SFZ file
//!
//! For the v0 POC we read the SFZ path from the `STARDUST_SFZ_PATH`
//! environment variable at plugin instantiation. Future iterations will
//! use the CLAP state extension so hosts can persist the choice into
//! patches, and a file-picker once we add a plugin GUI.
//!
//! ## Supported SFZ subset
//!
//! See [`sfz`] — currently `<region>` blocks with `sample`,
//! `pitch_keycenter`, `lokey`/`hikey`, `lovel`/`hivel`, and `volume`.
//! Enough to load real-world basic instruments (felt piano, organ
//! presets, drum kits with key-mapped samples). Groups, globals,
//! envelopes, filters, LFOs, and round-robin land in later phases.

#![doc(html_root_url = "https://docs.rs/stardust-sfz/0.0.1")]

pub mod engine;
pub mod instrument;
pub mod sample;
pub mod sfz;

use std::path::PathBuf;
use std::sync::Arc;

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPorts,
    PluginAudioPortsImpl,
};
use clack_extensions::note_ports::{
    NoteDialect, NoteDialects, NotePortInfo, NotePortInfoWriter, PluginNotePorts,
    PluginNotePortsImpl,
};
use clack_plugin::events::Match;
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;

use crate::engine::Engine;
use crate::instrument::Instrument;

/// Top-level plugin type. Stateless — all data lives in [`SharedState`]
/// or per-thread containers.
pub struct StardustSfzPlugin;

impl Plugin for StardustSfzPlugin {
    type AudioProcessor<'a> = Processor<'a>;
    type Shared<'a> = SharedState;
    type MainThread<'a> = MainThread<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&SharedState>) {
        builder
            .register::<PluginAudioPorts>()
            .register::<PluginNotePorts>();
    }
}

impl DefaultPluginFactory for StardustSfzPlugin {
    fn get_descriptor() -> PluginDescriptor {
        use clack_plugin::plugin::features::*;
        PluginDescriptor::new("com.stardust.sfz", "Stardust SFZ")
            .with_vendor("Stardust")
            .with_version(env!("CARGO_PKG_VERSION"))
            .with_description(
                "First-party SFZ sample-playback instrument. Loads an SFZ file from the \
                 STARDUST_SFZ_PATH environment variable at plugin instantiation.",
            )
            .with_features([INSTRUMENT, SAMPLER, STEREO])
    }

    fn new_shared(_host: HostSharedHandle) -> Result<SharedState, PluginError> {
        // Try to load the SFZ file at instantiation. If the env var is
        // unset or the file can't be read we still instantiate — just
        // with an empty instrument so the plugin loads and the host can
        // surface a "no SFZ loaded" warning rather than a hard failure.
        let instrument = load_from_env().unwrap_or_default();
        Ok(SharedState {
            instrument: Arc::new(instrument),
        })
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        shared: &'a SharedState,
    ) -> Result<MainThread<'a>, PluginError> {
        Ok(MainThread { shared })
    }
}

fn load_from_env() -> Option<Instrument> {
    let path: PathBuf = std::env::var_os("STARDUST_SFZ_PATH")?.into();
    match instrument::load_sfz(&path) {
        Ok(report) => {
            if !report.errors.is_empty() {
                for (p, msg) in &report.errors {
                    tracing::warn!(target: "stardust_sfz", "sample load failed: {} — {}", p.display(), msg);
                }
            }
            Some(report.instrument)
        }
        Err(e) => {
            tracing::warn!(
                target: "stardust_sfz",
                "could not read SFZ at {}: {e}",
                path.display()
            );
            None
        }
    }
}

/// Plugin data that lives on every thread.
pub struct SharedState {
    instrument: Arc<Instrument>,
}

impl PluginShared<'_> for SharedState {}

/// Main-thread state. Holds nothing yet — extension scaffolding will
/// land here when we wire in CLAP state and parameters.
pub struct MainThread<'a> {
    #[allow(dead_code)] // referenced by extension impls when those land
    shared: &'a SharedState,
}

impl<'a> PluginMainThread<'a, SharedState> for MainThread<'a> {}

/// Audio-thread state. Owns the polyphonic engine + a pre-allocated
/// scratch buffer the engine renders interleaved stereo into, which we
/// then deinterleave to the host's channel layout.
pub struct Processor<'a> {
    engine: Engine,
    /// Interleaved stereo scratch buffer — `[L0, R0, L1, R1, ...]`.
    /// Sized at activation for the max block, never reallocated.
    scratch: Vec<f32>,
    /// Held so the engine's `Arc<Instrument>` stays referenced for the
    /// processor's full lifetime, including across SharedState reloads.
    #[allow(dead_code)]
    shared: &'a SharedState,
}

impl<'a> PluginAudioProcessor<'a, SharedState, MainThread<'a>> for Processor<'a> {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &mut MainThread,
        shared: &'a SharedState,
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        let engine = Engine::new(shared.instrument.clone(), audio_config.sample_rate as f32);
        // Pre-allocate scratch for the max block size the host promised.
        // Interleaved stereo = 2 floats per frame.
        let scratch = vec![0.0f32; (audio_config.max_frames_count as usize).max(64) * 2];
        Ok(Self {
            engine,
            scratch,
            shared,
        })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        let mut output_port = audio
            .output_port(0)
            .ok_or(PluginError::Message("No output port found"))?;
        let mut channels = output_port
            .channels()?
            .into_f32()
            .ok_or(PluginError::Message("Expected f32 output"))?;
        if channels.channel_count() < 2 {
            return Err(PluginError::Message(
                "Stardust SFZ requires a stereo output",
            ));
        }

        // Determine the frame count from channel 0, then render the
        // full block into our interleaved scratch in one go and split
        // it back to L/R afterward. Doing the deinterleave at the end
        // avoids holding two mutable channel borrows simultaneously.
        let frames = {
            let ch0 = channels
                .channel(0)
                .ok_or(PluginError::Message("Expected at least one output channel"))?;
            ch0.len()
        };
        let needed = frames * 2;
        if needed > self.scratch.len() {
            // Host gave us a bigger block than promised — grow once.
            self.scratch.resize(needed, 0.0);
        }
        // Zero the scratch region we're about to render into. Done as
        // an inline scope so the borrow ends before the event loop
        // re-borrows self.scratch on each render call.
        self.scratch[..needed].fill(0.0);

        // Process events sample-accurately by splitting the render at
        // event boundaries. Each batch carries its first sample index;
        // the next batch's first sample is this batch's end (or the
        // buffer end for the final batch).
        //
        // We re-slice into self.scratch at each render site rather
        // than hoisting a `&mut self.scratch[..]` borrow over the loop,
        // so the borrow checker can split-borrow self.scratch (held
        // briefly) and self.engine (called via render_into_stereo)
        // disjointly — and so self.handle_event can take &mut self in
        // the same iteration.
        let mut cursor = 0usize;
        for batch in events.input.batch() {
            for event in batch.events() {
                self.handle_event(event);
            }
            let end = batch
                .next_batch_first_sample()
                .unwrap_or(frames)
                .min(frames);
            if end > cursor {
                self.engine
                    .render_into_stereo(&mut self.scratch[cursor * 2..end * 2]);
                cursor = end;
            }
        }
        // Render any remaining frames past the last event batch.
        if cursor < frames {
            self.engine
                .render_into_stereo(&mut self.scratch[cursor * 2..frames * 2]);
        }

        // Deinterleave into host channels.
        for ch_idx in 0..channels.channel_count().min(2) {
            if let Some(ch) = channels.channel_mut(ch_idx) {
                for (i, sample) in ch[..frames].iter_mut().enumerate() {
                    *sample = self.scratch[i * 2 + ch_idx as usize];
                }
            }
        }
        // Silence any extra channels the host might have provided.
        for ch_idx in 2..channels.channel_count() {
            if let Some(ch) = channels.channel_mut(ch_idx) {
                ch[..frames].fill(0.0);
            }
        }

        if self.engine.is_idle() {
            Ok(ProcessStatus::Sleep)
        } else {
            Ok(ProcessStatus::Continue)
        }
    }

    fn stop_processing(&mut self) {
        self.engine.all_notes_off();
    }
}

impl Processor<'_> {
    fn handle_event(&mut self, event: &UnknownEvent) {
        match event.as_core_event() {
            Some(CoreEventSpace::NoteOn(e)) => {
                if !e.port_index().matches(0u16) {
                    return;
                }
                if let (Match::Specific(channel), Match::Specific(key)) = (e.channel(), e.key()) {
                    let velocity = (e.velocity() * 127.0).clamp(0.0, 127.0) as u8;
                    self.engine
                        .note_on(channel as u8, key as u8, velocity.max(1));
                }
            }
            Some(CoreEventSpace::NoteOff(e)) => {
                if !e.port_index().matches(0u16) {
                    return;
                }
                if let (Match::Specific(channel), Match::Specific(key)) = (e.channel(), e.key()) {
                    self.engine.note_off(channel as u8, key as u8);
                }
            }
            // Raw MIDI (1.0) from the host — fixed 3-byte channel
            // message. We declared CLAP | MIDI dialect on the note
            // port so CCs (sustain pedal, etc.) and pitch bend flow.
            Some(CoreEventSpace::Midi(e)) => {
                let [status, d1, d2] = e.data();
                let kind = status & 0xF0;
                let channel = status & 0x0F;
                match kind {
                    0xB0 => {
                        // Control change
                        self.engine.control_change(channel, d1 & 0x7F, d2 & 0x7F);
                    }
                    0xE0 => {
                        // Pitch bend — 14-bit value with centre 8192.
                        let lsb = (d1 & 0x7F) as u16;
                        let msb = (d2 & 0x7F) as u16;
                        let raw = ((msb << 7) | lsb) as i32;
                        let centred = (raw - 8192) as i16; // -8192..=8191
                        self.engine.pitch_bend(channel, centred);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

// =============================================================================
// Port declarations
// =============================================================================

impl PluginAudioPortsImpl for MainThread<'_> {
    fn count(&mut self, is_input: bool) -> u32 {
        if is_input { 0 } else { 1 }
    }
    fn get(&mut self, index: u32, is_input: bool, writer: &mut AudioPortInfoWriter) {
        if !is_input && index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: b"main",
                channel_count: 2,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::STEREO),
                in_place_pair: None,
            });
        }
    }
}

impl PluginNotePortsImpl for MainThread<'_> {
    fn count(&mut self, is_input: bool) -> u32 {
        if is_input { 1 } else { 0 }
    }
    fn get(&mut self, index: u32, is_input: bool, writer: &mut NotePortInfoWriter) {
        if is_input && index == 0 {
            writer.set(&NotePortInfo {
                id: ClapId::new(0),
                name: b"main",
                preferred_dialect: Some(NoteDialect::Clap),
                supported_dialects: NoteDialects::CLAP | NoteDialects::MIDI,
            });
        }
    }
}

// =============================================================================
// CLAP entry export
// =============================================================================

clack_export_entry!(SinglePluginEntry<StardustSfzPlugin>);
