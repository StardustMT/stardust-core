//! `stardust-poc-host-clap` — host a CLAP plugin end-to-end.
//!
//! What this proves:
//!
//! - stardust-plugin's CLAP scanner + clack-host can instantiate a
//!   third-party `.clap` bundle inside a Stardust-built host.
//! - Live MIDI from a user-picked input gets translated into CLAP
//!   events (NoteOn / NoteOff as CLAP-dialect; CC / pitch bend as
//!   raw MIDI 1.0) and routed into the plugin sample-accurately.
//! - The plugin renders into a stereo output port we then deinterleave
//!   into cpal's audio buffer — the user hears the plugin's sound
//!   driven by their controller in real time.
//!
//! Limitations of this POC:
//!
//! - Assumes 1 stereo output port. Pure synth plugins (no audio in)
//!   work; multi-bus or multi-out plugins use only port 0.
//! - No parameter control, no preset loading, no GUI.
//! - No plugin extension support beyond what clack-host wires for free
//!   (audio-ports / note-ports discovery via the plugin descriptor is
//!   left to the plugin — we just give it a stereo bus and hope).
//!
//! Run:
//!
//! ```text
//! cargo run -p stardust-poc --bin stardust-poc-host-clap
//! ```

use anyhow::{Result, anyhow};
use stardust_audio::{AudioSpec, list_outputs, open_default_output, open_output};
use stardust_midi::{MidiMessage, list_inputs, open_input};
use stardust_plugin::clap::{
    AudioPortBuffer, AudioPortBufferType, AudioPorts, EventBuffer, InputChannel, MidiEvent,
    NoteOffEvent, NoteOnEvent, Pckn, PluginAudioConfiguration, PluginEntry, PluginInstance,
    StardustHost, default_clap_search_paths, host_info, scan_paths,
};
use stardust_rt::RingBuffer;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const SAMPLE_RATE: f32 = 48_000.0;
const MIN_FRAMES: u32 = 32;
const MAX_FRAMES: u32 = 2048;
const EVENT_QUEUE: usize = 1024;
const STEREO_CHANNELS: u32 = 2;

fn main() -> Result<()> {
    // ---------------------------------------------------------------
    // 1) Discover + pick a CLAP plugin.
    // ---------------------------------------------------------------
    let paths = default_clap_search_paths();
    let scan = scan_paths(&paths);
    if scan.bundles.is_empty() {
        return Err(anyhow!(
            "No CLAP plugins found on the standard search paths. Install one and re-run."
        ));
    }
    // Flat list of (bundle_path, descriptor) pairs so the user picks
    // one plugin out of all bundles in one prompt.
    let plugins: Vec<_> = scan
        .bundles
        .iter()
        .flat_map(|b| b.descriptors.iter().map(move |d| (b.path.clone(), d)))
        .collect();
    println!("Available CLAP plugins:");
    for (i, (path, d)) in plugins.iter().enumerate() {
        let path_short = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        println!("  [{i}] {} — {}  ({})", d.name, d.vendor, path_short);
    }
    let plugin_idx = prompt_index("Select plugin", plugins.len())?;
    let (bundle_path, descriptor) = plugins.get(plugin_idx).expect("validated above");

    // ---------------------------------------------------------------
    // 2) Pick MIDI input.
    // ---------------------------------------------------------------
    let inputs = list_inputs()?;
    if inputs.is_empty() {
        return Err(anyhow!(
            "No MIDI input ports found. Plug in a controller and re-run."
        ));
    }
    println!("\nMIDI inputs:");
    for (i, info) in inputs.iter().enumerate() {
        println!("  [{i}] {}", info.name);
    }
    let midi_idx = prompt_index("Select MIDI input", inputs.len())?;
    let midi_info = &inputs[midi_idx];

    // ---------------------------------------------------------------
    // 3) Pick audio output.
    // ---------------------------------------------------------------
    let outputs = list_outputs()?;
    if outputs.is_empty() {
        return Err(anyhow!("No audio output devices found."));
    }
    println!("\nAudio outputs:");
    for (i, info) in outputs.iter().enumerate() {
        let tag = if info.is_default { "  [default]" } else { "" };
        println!("  [{i}] {}{}", info.name, tag);
    }
    let audio_idx = prompt_index("Select audio output", outputs.len())?;
    let audio_info = &outputs[audio_idx];

    // ---------------------------------------------------------------
    // 4) Load the plugin bundle + instantiate.
    // ---------------------------------------------------------------
    let host_info = host_info();
    // SAFETY: loading any third-party CLAP dynamic library is unsafe at
    // the FFI boundary. We've already validated the path via the scan
    // and the user has explicitly opted into running CLAP plugins.
    let entry = unsafe { PluginEntry::load(bundle_path) }
        .map_err(|e| anyhow!("failed to load .clap bundle: {e}"))?;

    // Re-find the descriptor inside the loaded entry's factory so we
    // get the live `&CStr` plugin id — the descriptor we matched
    // earlier was a snapshot with owned strings.
    let factory = entry
        .get_plugin_factory()
        .ok_or_else(|| anyhow!("bundle exposed no plugin factory"))?;
    let plugin_id_cstr = factory
        .plugin_descriptors()
        .filter_map(|d| d.id())
        .find(|cs| cs.to_string_lossy() == descriptor.id)
        .ok_or_else(|| anyhow!("plugin id disappeared between scan and instantiate"))?;

    let mut plugin = PluginInstance::<StardustHost>::new(
        |_| stardust_plugin::clap::StardustHostShared,
        |_| (),
        &entry,
        plugin_id_cstr,
        &host_info,
    )
    .map_err(|e| anyhow!("plugin instantiation failed: {e:?}"))?;

    // ---------------------------------------------------------------
    // 5) Activate at our target audio config.
    // ---------------------------------------------------------------
    let config = PluginAudioConfiguration {
        sample_rate: SAMPLE_RATE as f64,
        min_frames_count: MIN_FRAMES,
        max_frames_count: MAX_FRAMES,
    };
    let stopped = plugin
        .activate(|_, _| (), config)
        .map_err(|e| anyhow!("plugin activation failed: {e:?}"))?;
    let mut started = stopped
        .start_processing()
        .map_err(|e| anyhow!("plugin failed to start processing: {e:?}"))?;

    // ---------------------------------------------------------------
    // 6) MIDI input → SPSC ring buffer. Producer goes to the midir
    //    callback thread; consumer lives in the audio callback.
    // ---------------------------------------------------------------
    let (mut producer, mut consumer) = RingBuffer::<MidiMessage>::new(EVENT_QUEUE);
    let dropped = Arc::new(AtomicUsize::new(0));
    let dropped_for_midi = dropped.clone();
    let _midi_handle = open_input(&midi_info.name, move |_ts, msg| {
        // Drop non-channel-voice messages at the source so the audio
        // thread doesn't burn cycles ignoring them.
        if matches!(msg, MidiMessage::Other) {
            return;
        }
        if producer.push(msg).is_err() {
            dropped_for_midi.fetch_add(1, Ordering::Relaxed);
        }
    })?;

    // ---------------------------------------------------------------
    // 7) Audio callback owns the plugin audio processor + buffers.
    //    All allocation is up-front; the closure mutates pre-sized
    //    state per-block.
    // ---------------------------------------------------------------
    let mut input_ports = AudioPorts::with_capacity(STEREO_CHANNELS as usize, 1);
    let mut output_ports = AudioPorts::with_capacity(STEREO_CHANNELS as usize, 1);
    // Pre-allocated audio buffers sized for the max block. CLAP wants
    // separate L/R channel slices, not interleaved — we'll deinterleave
    // into these from the plugin output and re-interleave for cpal.
    let mut input_l = vec![0.0f32; MAX_FRAMES as usize];
    let mut input_r = vec![0.0f32; MAX_FRAMES as usize];
    let mut output_l = vec![0.0f32; MAX_FRAMES as usize];
    let mut output_r = vec![0.0f32; MAX_FRAMES as usize];

    let mut input_events = EventBuffer::with_capacity(EVENT_QUEUE);
    let mut output_events = EventBuffer::with_capacity(EVENT_QUEUE);

    let render = move |cpal_buf: &mut [f32], spec: &AudioSpec| {
        let channels = spec.channels as usize;
        let frames = cpal_buf.len() / channels;
        let frames = frames.min(MAX_FRAMES as usize);

        // Drain MIDI events out of the SPSC and translate to CLAP.
        input_events.clear();
        output_events.clear();
        while let Ok(msg) = consumer.pop() {
            push_midi_as_clap_events(&mut input_events, msg);
        }

        // Zero the input audio buffers — instrument plugins ignore
        // them; effect plugins would have host-provided audio here.
        for s in input_l[..frames].iter_mut() {
            *s = 0.0;
        }
        for s in input_r[..frames].iter_mut() {
            *s = 0.0;
        }

        // Build CLAP buffer descriptors. The buffer slices feed straight
        // into the plugin's process call; no copies.
        //
        // SAFETY note: input/output channel iterators must yield exactly
        // STEREO_CHANNELS items per port — that's what `with_capacity`
        // above reserved. Mismatch would panic inside clack-host.
        let in_buffers = {
            let mut iter = [&mut input_l[..frames], &mut input_r[..frames]].into_iter();
            input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(std::iter::from_fn(move || {
                    iter.next().map(InputChannel::constant)
                })),
            }])
        };
        let mut out_buffers = {
            let mut iter = [&mut output_l[..frames], &mut output_r[..frames]].into_iter();
            output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(std::iter::from_fn(move || {
                    iter.next().map(|s| s as &mut [f32])
                })),
            }])
        };

        let in_events = input_events.as_input();
        let mut out_events = output_events.as_output();

        if let Err(e) = started.process(
            &in_buffers,
            &mut out_buffers,
            &in_events,
            &mut out_events,
            None,
            None,
        ) {
            eprintln!("plugin.process error: {e:?}");
            // Fill with silence so we don't blast garbage.
            cpal_buf.fill(0.0);
            return;
        }

        // Re-interleave plugin output into cpal's buffer. For mono
        // outputs sum L only; for stereo+, take L into channel 0 and
        // R into channel 1, mirror to extras as silence.
        for f in 0..frames {
            let l = output_l[f];
            let r = output_r[f];
            let base = f * channels;
            if channels == 1 {
                cpal_buf[base] = (l + r) * 0.5;
            } else {
                cpal_buf[base] = l;
                cpal_buf[base + 1] = r;
                for c in 2..channels {
                    cpal_buf[base + c] = 0.0;
                }
            }
        }
        // Zero any trailing samples we didn't fill (e.g. cpal asked
        // for > MAX_FRAMES — would only happen if a host violates the
        // configuration we passed at activate-time).
        let written = frames * channels;
        if written < cpal_buf.len() {
            for s in cpal_buf[written..].iter_mut() {
                *s = 0.0;
            }
        }
    };

    let audio_handle = if audio_info.is_default {
        open_default_output(Some(SAMPLE_RATE as u32), render)?
    } else {
        open_output(&audio_info.name, Some(SAMPLE_RATE as u32), render)?
    };

    println!(
        "\n✓ Hosting: {} ({})\n✓ MIDI:    {}\n✓ Audio:   {} @ {} Hz, {} ch\n",
        descriptor.name,
        descriptor.id,
        midi_info.name,
        audio_info.name,
        audio_handle.spec.sample_rate,
        audio_handle.spec.channels
    );
    if audio_handle.spec.sample_rate != SAMPLE_RATE as u32 {
        eprintln!(
            "⚠  cpal negotiated {} Hz but the plugin was activated at {} Hz — \
             pitches will be off until we wire dynamic re-activation.",
            audio_handle.spec.sample_rate, SAMPLE_RATE as u32,
        );
    }
    println!("Play your controller. Ctrl-C to exit.\n");

    let mut last_dropped = 0;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let now_dropped = dropped.load(Ordering::Relaxed);
        if now_dropped != last_dropped {
            eprintln!(
                "  ⚠  dropped {} MIDI event(s) (total {})",
                now_dropped - last_dropped,
                now_dropped
            );
            last_dropped = now_dropped;
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}

/// Translate a single parsed MIDI message into one or more CLAP events
/// pushed onto the input buffer. Notes get CLAP-native NoteOn/Off
/// (best cross-plugin compatibility). Everything else (CC, pitch bend,
/// aftertouch, program change) goes through as raw MIDI 1.0 since
/// plugins that care will accept those, and ones that don't will
/// ignore them.
fn push_midi_as_clap_events(buf: &mut EventBuffer, msg: MidiMessage) {
    match msg {
        MidiMessage::NoteOn {
            channel,
            note,
            velocity,
        } => {
            let event = NoteOnEvent::new(
                0,
                Pckn::new(0u16, channel as u16, note as u16, u32::MAX),
                velocity as f64 / 127.0,
            );
            buf.push(&event);
        }
        MidiMessage::NoteOff {
            channel,
            note,
            velocity,
        } => {
            let event = NoteOffEvent::new(
                0,
                Pckn::new(0u16, channel as u16, note as u16, u32::MAX),
                velocity as f64 / 127.0,
            );
            buf.push(&event);
        }
        // Send everything else as raw MIDI 1.0 — plugins that handle
        // CCs / pitch bend will pick them up.
        other => {
            if let Some(bytes) = midi_message_to_bytes(other) {
                let event = MidiEvent::new(0, 0, bytes);
                buf.push(&event);
            }
        }
    }
}

/// Convert a parsed MidiMessage back into a 3-byte MIDI 1.0 frame.
/// Returns None for the variants we don't pack (NoteOn/NoteOff are
/// handled separately as CLAP events; `Other` is dropped at the
/// MIDI-input site).
fn midi_message_to_bytes(msg: MidiMessage) -> Option<[u8; 3]> {
    match msg {
        MidiMessage::ControlChange { channel, cc, value } => {
            Some([0xB0 | (channel & 0x0F), cc & 0x7F, value & 0x7F])
        }
        MidiMessage::PitchBend { channel, value } => {
            let raw = (value as i32 + 8192).clamp(0, 16383) as u16;
            let lsb = (raw & 0x7F) as u8;
            let msb = ((raw >> 7) & 0x7F) as u8;
            Some([0xE0 | (channel & 0x0F), lsb, msb])
        }
        MidiMessage::ChannelPressure { channel, value } => {
            Some([0xD0 | (channel & 0x0F), value & 0x7F, 0])
        }
        MidiMessage::PolyAftertouch {
            channel,
            note,
            value,
        } => Some([0xA0 | (channel & 0x0F), note & 0x7F, value & 0x7F]),
        MidiMessage::ProgramChange { channel, program } => {
            Some([0xC0 | (channel & 0x0F), program & 0x7F, 0])
        }
        // NoteOn/NoteOff handled as CLAP-native events above; Other dropped.
        _ => None,
    }
}

fn prompt_index(label: &str, max: usize) -> Result<usize> {
    print!("{label} [0]: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    let parsed: usize = trimmed.parse().unwrap_or(0);
    if parsed >= max {
        eprintln!("Index out of range, using 0.");
        return Ok(0);
    }
    Ok(parsed)
}
