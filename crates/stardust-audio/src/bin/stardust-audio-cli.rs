//! `stardust-audio-cli` — list outputs and play a short test tone.
//!
//! Run from the `stardust-core` workspace:
//!
//! ```text
//! cargo run -p stardust-audio --bin stardust-audio-cli
//! ```
//!
//! Cross-platform: CoreAudio on macOS, WASAPI on Windows, ALSA on Linux.
//! Plays a 440 Hz sine for 2 seconds on the selected device.

use stardust_audio::{AudioSpec, list_outputs, open_default_output, open_output};
use std::f32::consts::TAU;
use std::io::{self, BufRead, Write};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let outputs = list_outputs()?;
    if outputs.is_empty() {
        println!("No audio output devices found.");
        return Ok(());
    }

    println!("Available audio outputs:");
    for (i, info) in outputs.iter().enumerate() {
        let tag = if info.is_default { "  [default]" } else { "" };
        println!("  [{i}] {}{}", info.name, tag);
    }

    let idx = prompt_index(outputs.len())?;
    let info = &outputs[idx];
    println!("\nPlaying 440 Hz sine for 2 seconds on: {}\n", info.name);

    let mut phase: f32 = 0.0;
    let render = move |buf: &mut [f32], spec: &AudioSpec| {
        let advance = 440.0 * TAU / spec.sample_rate as f32;
        let channels = spec.channels as usize;
        for frame in buf.chunks_exact_mut(channels) {
            let sample = phase.sin() * 0.2; // -6 dB amplitude
            for ch in frame.iter_mut() {
                *ch = sample;
            }
            phase += advance;
            if phase > TAU {
                phase -= TAU;
            }
        }
    };

    let handle = if info.is_default {
        open_default_output(None, render)?
    } else {
        open_output(&info.name, None, render)?
    };

    println!(
        "  device opened at {} Hz, {} channel(s)",
        handle.spec.sample_rate, handle.spec.channels
    );

    std::thread::sleep(Duration::from_secs(2));
    drop(handle);
    println!("done.");
    Ok(())
}

fn prompt_index(max: usize) -> io::Result<usize> {
    print!("Select output [0]: ");
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
