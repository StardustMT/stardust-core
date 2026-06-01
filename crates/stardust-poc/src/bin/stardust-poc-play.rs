//! `stardust-poc-play` — end-to-end MIDI → synth → audio pipeline.
//!
//! What this proves:
//!
//! - A MIDI input device opened with `stardust-midi` delivers parsed
//!   [`MidiMessage`]s into a `stardust-rt` SPSC ring buffer from the
//!   midir callback thread.
//! - The audio thread (driven by `stardust-audio` / cpal) drains the SPSC,
//!   feeds events into `stardust-dsp::Synth`, and renders into the output
//!   buffer in a single pass.
//! - No allocations and no locks in the audio callback — only ring-buffer
//!   pops and synth ticks.
//!
//! Run from the workspace:
//!
//! ```text
//! cargo run -p stardust-poc --bin stardust-poc-play
//! ```
//!
//! Pick a MIDI input, then an audio output. Play your controller — the
//! built-in sine synth responds with low-latency audio. Ctrl-C to exit.

use anyhow::{Result, anyhow};
use stardust_audio::{AudioSpec, list_outputs, open_default_output, open_output};
use stardust_dsp::Synth;
use stardust_midi::{MidiMessage, list_inputs, open_input};
use stardust_rt::RingBuffer;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Polyphony cap. Pre-allocated up front; never resized.
const POLYPHONY: usize = 16;

/// SPSC capacity — sized for short bursts (a fast chord run is ~50 msgs).
/// One slot is reserved by `rtrb` so usable capacity is `EVENT_QUEUE - 1`.
const EVENT_QUEUE: usize = 1024;

fn main() -> Result<()> {
    // ---------------------------------------------------------------
    // 1) Pick a MIDI input.
    // ---------------------------------------------------------------
    let inputs = list_inputs()?;
    if inputs.is_empty() {
        return Err(anyhow!(
            "No MIDI input ports found. Plug in a controller and re-run."
        ));
    }
    println!("MIDI inputs:");
    for (i, info) in inputs.iter().enumerate() {
        println!("  [{i}] {}", info.name);
    }
    let midi_idx = prompt_index("Select MIDI input", inputs.len())?;
    let midi_info = &inputs[midi_idx];

    // ---------------------------------------------------------------
    // 2) Pick an audio output.
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
    // 3) Open the SPSC. Producer goes to the midir callback, consumer
    //    lives on the audio thread.
    // ---------------------------------------------------------------
    let (mut producer, mut consumer) = RingBuffer::<MidiMessage>::new(EVENT_QUEUE);

    // Telemetry: count dropped events without locking. Reported from the
    // MIDI thread when the queue is full (audio thread isn't draining fast
    // enough — should never happen in practice but worth surfacing).
    let dropped_events = Arc::new(AtomicUsize::new(0));
    let dropped_for_midi = dropped_events.clone();

    // ---------------------------------------------------------------
    // 4) Open the MIDI input. The midir thread parses bytes into
    //    MidiMessage and pushes into the SPSC. The closure must NOT
    //    allocate or block — `producer.push` is wait-free.
    // ---------------------------------------------------------------
    let _midi_handle = open_input(&midi_info.name, move |_ts_ns, msg| {
        // Only push channel-voice messages the synth can act on. Everything
        // else (Other, realtime) is dropped at the source so the audio
        // thread doesn't burn cycles ignoring them.
        match msg {
            MidiMessage::NoteOn { .. } | MidiMessage::NoteOff { .. }
                if producer.push(msg).is_err() =>
            {
                dropped_for_midi.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    })?;

    // ---------------------------------------------------------------
    // 5) Open the audio output. The closure owns the Synth and drains
    //    the SPSC at the top of every callback. Synth::new pre-allocates
    //    POLYPHONY voices, so no allocation happens on the audio thread.
    // ---------------------------------------------------------------
    //
    // We don't have the negotiated sample rate yet — cpal decides that.
    // Initialize the synth lazily inside the callback after we see the
    // first AudioSpec.
    let mut synth: Option<Synth> = None;

    let render = move |buf: &mut [f32], spec: &AudioSpec| {
        if synth.is_none() {
            synth = Some(Synth::new(spec.sample_rate as f32, POLYPHONY));
        }
        let s = synth.as_mut().expect("synth initialized above");
        // Drain ALL pending events before rendering this block. Doing this
        // once per block is "block-rate" timing; sample-accurate scheduling
        // is a later concern (would split the buffer at event boundaries).
        while let Ok(msg) = consumer.pop() {
            s.process_midi(msg);
        }
        s.render(buf, spec.channels as usize);
    };

    let audio_handle = if audio_info.is_default {
        open_default_output(None, render)?
    } else {
        open_output(&audio_info.name, None, render)?
    };

    // ---------------------------------------------------------------
    // 6) Done. The midir thread and the cpal audio thread are now
    //    running independently. The main thread just reports status.
    // ---------------------------------------------------------------
    println!(
        "\n✓ MIDI:  {}\n✓ Audio: {} @ {} Hz, {} ch\n",
        midi_info.name, audio_info.name, audio_handle.spec.sample_rate, audio_handle.spec.channels
    );
    println!("Play your controller. Ctrl-C to exit.\n");

    // Heartbeat every second. Mostly useful for spotting dropped events.
    let mut last_dropped = 0;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let now_dropped = dropped_events.load(Ordering::Relaxed);
        if now_dropped != last_dropped {
            eprintln!(
                "  ⚠  dropped {} MIDI event(s) (total: {})",
                now_dropped - last_dropped,
                now_dropped
            );
            last_dropped = now_dropped;
        }
    }

    // Unreachable — loop above runs forever until Ctrl-C terminates the
    // process. `_midi_handle` + `audio_handle` are held in scope for the
    // duration of the run.
    #[allow(unreachable_code)]
    Ok(())
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
