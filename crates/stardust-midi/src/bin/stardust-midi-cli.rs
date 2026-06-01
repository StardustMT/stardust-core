//! `stardust-midi-cli` — list MIDI inputs and print incoming messages.
//!
//! Build + run from the `stardust-core` workspace:
//!
//! ```text
//! cargo run -p stardust-midi --bin stardust-midi-cli
//! ```
//!
//! Cross-platform: works on macOS (CoreMIDI), Windows (WinMM), and Linux
//! (ALSA). Plug in any MIDI device, pick its index, then play.

use stardust_midi::{MidiMessage, list_inputs, open_input};
use std::io::{self, BufRead, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let inputs = list_inputs()?;
    if inputs.is_empty() {
        println!("No MIDI input ports found. Plug in a device and re-run.");
        return Ok(());
    }

    println!("Available MIDI inputs:");
    for (i, info) in inputs.iter().enumerate() {
        println!("  [{i}] {}", info.name);
    }

    let idx = prompt_index(inputs.len())?;
    let info = &inputs[idx];
    println!("\nOpening: {}", info.name);
    println!("Listening — play your controller. Ctrl-C to exit.\n");

    let _handle = open_input(&info.name, |_timestamp_ns, msg| {
        print_event(msg);
    })?;

    // The midir thread drives the callback. Block forever.
    std::thread::park();
    Ok(())
}

fn prompt_index(max: usize) -> io::Result<usize> {
    print!("Select port [0]: ");
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

fn print_event(msg: MidiMessage) {
    match msg {
        MidiMessage::NoteOn {
            channel,
            note,
            velocity,
        } => println!(
            "  note on   ch {:>2}  note {:>3} ({})  vel {:>3}",
            channel + 1,
            note,
            note_name(note),
            velocity
        ),
        MidiMessage::NoteOff {
            channel,
            note,
            velocity,
        } => println!(
            "  note off  ch {:>2}  note {:>3} ({})  vel {:>3}",
            channel + 1,
            note,
            note_name(note),
            velocity
        ),
        MidiMessage::ControlChange { channel, cc, value } => println!(
            "  cc        ch {:>2}  cc   {:>3}        val {:>3}",
            channel + 1,
            cc,
            value
        ),
        MidiMessage::PitchBend { channel, value } => println!(
            "  pitch     ch {:>2}                   val {:>6}",
            channel + 1,
            value
        ),
        MidiMessage::ChannelPressure { channel, value } => println!(
            "  ch press  ch {:>2}                   val {:>3}",
            channel + 1,
            value
        ),
        MidiMessage::PolyAftertouch {
            channel,
            note,
            value,
        } => println!(
            "  poly at   ch {:>2}  note {:>3} ({})  val {:>3}",
            channel + 1,
            note,
            note_name(note),
            value
        ),
        MidiMessage::ProgramChange { channel, program } => println!(
            "  prog      ch {:>2}                   prog {:>3}",
            channel + 1,
            program
        ),
        MidiMessage::Other => {}
    }
}

fn note_name(midi: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let octave = midi as i32 / 12 - 1;
    format!("{}{}", NAMES[(midi % 12) as usize], octave)
}
