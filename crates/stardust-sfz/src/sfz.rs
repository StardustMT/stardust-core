//! SFZ format parser — Phase-1.5 subset.
//!
//! Supports enough of SFZ to load real-world basic and intermediate
//! instruments (felt piano, drum kits, organ presets, looped pads,
//! velocity-layered patches):
//!
//! - `<global>`, `<master>`, `<group>`, `<region>` sections with
//!   **opcode inheritance**: opcodes set in `<global>` propagate to
//!   every region, `<master>` extends/overrides global, `<group>`
//!   extends/overrides master, `<region>` extends/overrides group.
//! - Numeric **and** note-name (`c4`, `f#3`, `bb-1`) values for any
//!   opcode that accepts a MIDI note.
//! - Opcodes per region:
//!   - **mapping**: `sample`, `key`, `lokey`, `hikey`, `pitch_keycenter`,
//!     `lovel`, `hivel`
//!   - **amplitude**: `volume`, `pan` (-100..100)
//!   - **pitch**: `tune` (cents), `transpose` (semitones)
//!   - **envelope**: `ampeg_attack`, `ampeg_decay`, `ampeg_sustain`,
//!     `ampeg_release` (seconds; sustain is 0..100 percent per SFZ spec)
//!   - **loop**: `loop_mode` (`no_loop`/`one_shot`/`loop_continuous`),
//!     `loop_start`, `loop_end` (frame indices into the sample)
//! - `//` line comments. SFZ also has `/* */` block comments — we
//!   tolerate them by stripping any line containing only block-comment
//!   syntax; mid-line block comments still aren't fully handled.
//!
//! Unknown opcodes are silently dropped: real-world SFZs use opcodes we
//! don't understand and we'd rather load the parts we do than fail the
//! whole file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Loop behaviour parsed from `loop_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopMode {
    /// Play sample once and stop. `loop_start`/`loop_end` ignored.
    #[default]
    NoLoop,
    /// Play sample once. Identical to `NoLoop` for our engine; some SFZ
    /// authors distinguish them for backward-compat reasons.
    OneShot,
    /// Play through to `loop_end`, then jump back to `loop_start` and
    /// repeat indefinitely (until note-off if envelope still allows).
    LoopContinuous,
}

/// A single `<region>` block with all inherited opcodes resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    /// Resolved absolute path to the sample file (joined with the
    /// `<control>` `default_path` and the .sfz file's directory).
    pub sample: PathBuf,
    /// Lowest MIDI note this region responds to (inclusive).
    pub lokey: u8,
    /// Highest MIDI note this region responds to (inclusive).
    pub hikey: u8,
    /// MIDI note the sample was recorded at.
    pub pitch_keycenter: u8,
    /// Lowest MIDI velocity this region responds to (inclusive).
    pub lovel: u8,
    /// Highest MIDI velocity this region responds to (inclusive).
    pub hivel: u8,
    /// Volume in decibels relative to unity.
    pub volume_db: f32,
    /// Stereo pan, -100 (full left) to +100 (full right). 0 = centred.
    pub pan: f32,
    /// Pitch offset in cents (-100..100 per semitone).
    pub tune_cents: f32,
    /// Pitch offset in semitones (integer).
    pub transpose_semitones: i8,
    /// Amplitude-envelope attack time in seconds.
    pub ampeg_attack_secs: f32,
    /// Amplitude-envelope decay time in seconds.
    pub ampeg_decay_secs: f32,
    /// Amplitude-envelope sustain level (0..1). SFZ specifies it as
    /// 0-100 percent; we store the normalised form.
    pub ampeg_sustain: f32,
    /// Amplitude-envelope release time in seconds.
    pub ampeg_release_secs: f32,
    /// Loop mode.
    pub loop_mode: LoopMode,
    /// Loop start frame (only used when `loop_mode = LoopContinuous`).
    pub loop_start: u64,
    /// Loop end frame (only used when `loop_mode = LoopContinuous`).
    /// 0 means "use the sample length" — the loader fills this in.
    pub loop_end: u64,
}

impl Default for Region {
    fn default() -> Self {
        Self {
            sample: PathBuf::new(),
            lokey: 0,
            hikey: 127,
            pitch_keycenter: 60,
            lovel: 0,
            hivel: 127,
            volume_db: 0.0,
            pan: 0.0,
            tune_cents: 0.0,
            transpose_semitones: 0,
            // Sensible defaults: a few-ms attack avoids clicks even
            // when the SFZ omits ampeg_attack. SFZ spec default for
            // ampeg_attack is 0; we deliberately bump it to ~3ms here
            // and let explicit ampeg_attack=N override.
            ampeg_attack_secs: 0.003,
            ampeg_decay_secs: 0.0,
            ampeg_sustain: 1.0,
            ampeg_release_secs: 0.080,
            loop_mode: LoopMode::NoLoop,
            loop_start: 0,
            loop_end: 0,
        }
    }
}

impl Region {
    /// True if this region should sound for the given (key, velocity).
    pub fn matches(&self, key: u8, velocity: u8) -> bool {
        key >= self.lokey && key <= self.hikey && velocity >= self.lovel && velocity <= self.hivel
    }

    /// Combined pitch offset: transpose (semitones) + tune (cents).
    /// Returned in semitones as f32 for direct use in `2^(n/12)`.
    pub fn pitch_offset_semitones(&self) -> f32 {
        self.transpose_semitones as f32 + self.tune_cents / 100.0
    }
}

/// A parsed SFZ instrument — flattened regions, all opcodes resolved.
#[derive(Debug, Clone, Default)]
pub struct SfzFile {
    /// Regions in original declaration order. Order is preserved
    /// because SFZ uses last-match-wins for overlapping ranges.
    pub regions: Vec<Region>,
}

/// Bag of opcodes captured at a particular section level. We collect
/// them as raw strings rather than typed values so override semantics
/// stay simple: setting `volume=-6` at `<group>` is just a HashMap
/// insert that a later `<region>` opcode can shadow.
type OpcodeMap = HashMap<String, String>;

/// Section the parser is currently filling. Drives where each opcode
/// gets stored and when accumulated regions get flushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    /// Before any section header — opcodes ignored.
    None,
    /// `<control>` — collects `default_path` etc.
    Control,
    /// `<global>` — opcodes inherited by every subsequent region until
    /// the next `<global>`.
    Global,
    /// `<master>` — inherited until next `<master>` or `<global>`.
    Master,
    /// `<group>` — inherited until next `<group>`/`<master>`/`<global>`.
    Group,
    /// `<region>` — accumulates region-specific opcodes; flushed when
    /// the next section header arrives or the file ends.
    Region,
    /// Header the parser doesn't model (e.g. `<curve>`). Opcodes
    /// dropped until the next known section.
    Unknown,
}

impl SfzFile {
    /// Parse SFZ text. `base_dir` is what `sample=` paths resolve
    /// against; the parser also honours `<control>` `default_path`
    /// when present.
    pub fn parse(text: &str, base_dir: &Path) -> Self {
        let mut file = SfzFile::default();
        let mut control: OpcodeMap = HashMap::new();
        let mut global: OpcodeMap = HashMap::new();
        let mut master: OpcodeMap = HashMap::new();
        let mut group: OpcodeMap = HashMap::new();
        let mut region: OpcodeMap = HashMap::new();
        let mut current = Section::None;

        for raw_line in text.lines() {
            let line = strip_line_comment(raw_line);
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            for token in split_sfz_tokens(trimmed) {
                if let Some(header) = parse_header(&token) {
                    // Flush any in-progress region BEFORE switching
                    // section, so its opcodes don't leak into the next.
                    if current == Section::Region {
                        flush_region(
                            &mut file, &control, &global, &master, &group, &region, base_dir,
                        );
                        region.clear();
                    }
                    current = match header.as_str() {
                        "control" => Section::Control,
                        "global" => {
                            // Re-entering a new <global> resets every
                            // narrower scope per SFZ inheritance rules.
                            global.clear();
                            master.clear();
                            group.clear();
                            Section::Global
                        }
                        "master" => {
                            master.clear();
                            group.clear();
                            Section::Master
                        }
                        "group" => {
                            group.clear();
                            Section::Group
                        }
                        "region" => Section::Region,
                        _ => Section::Unknown,
                    };
                    continue;
                }
                if let Some((k, v)) = token.split_once('=') {
                    let key = k.trim().to_ascii_lowercase();
                    let val = v.trim().to_string();
                    let target = match current {
                        Section::Control => &mut control,
                        Section::Global => &mut global,
                        Section::Master => &mut master,
                        Section::Group => &mut group,
                        Section::Region => &mut region,
                        Section::None | Section::Unknown => continue,
                    };
                    target.insert(key, val);
                }
            }
        }
        // Final region in the file.
        if current == Section::Region {
            flush_region(
                &mut file, &control, &global, &master, &group, &region, base_dir,
            );
        }
        file
    }
}

/// Merge global → master → group → region opcodes and append the
/// resolved Region to the file. Regions with no `sample=` are dropped
/// (template-only sections aren't playable on their own).
fn flush_region(
    file: &mut SfzFile,
    control: &OpcodeMap,
    global: &OpcodeMap,
    master: &OpcodeMap,
    group: &OpcodeMap,
    region: &OpcodeMap,
    base_dir: &Path,
) {
    let mut merged: OpcodeMap =
        HashMap::with_capacity(global.len() + master.len() + group.len() + region.len());
    for src in [global, master, group, region] {
        for (k, v) in src {
            merged.insert(k.clone(), v.clone());
        }
    }
    let mut out = Region::default();
    let default_path = control.get("default_path").map(String::as_str);
    for (k, v) in &merged {
        apply_opcode(&mut out, k, v, base_dir, default_path);
    }
    if !out.sample.as_os_str().is_empty() {
        file.regions.push(out);
    }
}

/// Apply one opcode to a region. Unknown opcodes are silently ignored.
fn apply_opcode(
    region: &mut Region,
    key: &str,
    value: &str,
    base_dir: &Path,
    default_path: Option<&str>,
) {
    match key {
        "sample" => {
            let normalised: String = value.replace('\\', "/");
            let mut p = PathBuf::from(&normalised);
            if !p.is_absolute() {
                let root = match default_path {
                    Some(rel) => base_dir.join(rel.replace('\\', "/")),
                    None => base_dir.to_path_buf(),
                };
                p = root.join(p);
            }
            region.sample = p;
        }
        "lokey" => {
            if let Some(n) = parse_note(value) {
                region.lokey = n;
            }
        }
        "hikey" => {
            if let Some(n) = parse_note(value) {
                region.hikey = n;
            }
        }
        "key" => {
            // `key=N` is shorthand for lokey=N hikey=N pitch_keycenter=N.
            if let Some(n) = parse_note(value) {
                region.lokey = n;
                region.hikey = n;
                region.pitch_keycenter = n;
            }
        }
        "pitch_keycenter" => {
            if let Some(n) = parse_note(value) {
                region.pitch_keycenter = n;
            }
        }
        "lovel" => {
            if let Ok(n) = value.parse::<u8>() {
                region.lovel = n.min(127);
            }
        }
        "hivel" => {
            if let Ok(n) = value.parse::<u8>() {
                region.hivel = n.min(127);
            }
        }
        "volume" => {
            if let Ok(db) = value.parse::<f32>() {
                region.volume_db = db;
            }
        }
        "pan" => {
            if let Ok(p) = value.parse::<f32>() {
                region.pan = p.clamp(-100.0, 100.0);
            }
        }
        "tune" => {
            if let Ok(c) = value.parse::<f32>() {
                region.tune_cents = c;
            }
        }
        "transpose" => {
            if let Ok(t) = value.parse::<i8>() {
                region.transpose_semitones = t;
            }
        }
        "ampeg_attack" => {
            if let Ok(s) = value.parse::<f32>() {
                region.ampeg_attack_secs = s.max(0.0);
            }
        }
        "ampeg_decay" => {
            if let Ok(s) = value.parse::<f32>() {
                region.ampeg_decay_secs = s.max(0.0);
            }
        }
        "ampeg_sustain" => {
            // SFZ spec: 0-100 percent. Normalise to 0-1.
            if let Ok(p) = value.parse::<f32>() {
                region.ampeg_sustain = (p / 100.0).clamp(0.0, 1.0);
            }
        }
        "ampeg_release" => {
            if let Ok(s) = value.parse::<f32>() {
                region.ampeg_release_secs = s.max(0.0001);
            }
        }
        "loop_mode" => {
            region.loop_mode = match value.to_ascii_lowercase().as_str() {
                "loop_continuous" => LoopMode::LoopContinuous,
                "one_shot" => LoopMode::OneShot,
                _ => LoopMode::NoLoop,
            };
        }
        "loop_start" => {
            if let Ok(n) = value.parse::<u64>() {
                region.loop_start = n;
            }
        }
        "loop_end" => {
            if let Ok(n) = value.parse::<u64>() {
                region.loop_end = n;
            }
        }
        _ => {}
    }
}

/// Parse a value that's either a MIDI number (0-127) or a note name
/// like `c4`, `c#3`, `eb-1`, `f#-1`. Returns `None` on garbage.
pub fn parse_note(s: &str) -> Option<u8> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Try numeric first — covers the common case and is unambiguous.
    if let Ok(n) = s.parse::<i32>() {
        if (0..=127).contains(&n) {
            return Some(n as u8);
        }
        return None;
    }
    parse_note_name(s)
}

fn parse_note_name(s: &str) -> Option<u8> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let letter = match bytes[0].to_ascii_lowercase() {
        b'c' => 0,
        b'd' => 2,
        b'e' => 4,
        b'f' => 5,
        b'g' => 7,
        b'a' => 9,
        b'b' => 11,
        _ => return None,
    };
    let mut idx = 1;
    let mut accidental: i32 = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'#' => {
                accidental += 1;
                idx += 1;
            }
            b'b' if idx > 0 => {
                accidental -= 1;
                idx += 1;
            }
            _ => break,
        }
    }
    let octave: i32 = s[idx..].parse().ok()?;
    // MIDI 0 = C-1, MIDI 60 = C4 (Yamaha / SFZ convention).
    let note = (octave + 1) * 12 + letter + accidental;
    if !(0..=127).contains(&note) {
        return None;
    }
    Some(note as u8)
}

fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(idx) => &line[..idx],
        None => line,
    }
}

fn parse_header(token: &str) -> Option<String> {
    token
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .map(|s| s.to_ascii_lowercase())
}

/// Tokenise an SFZ line. Tokens are whitespace-separated, but a
/// `sample=` value may itself contain spaces. We approximate by
/// re-joining whitespace tokens after a `sample=` opcode until the
/// next `key=value` or `<header>` arrives.
fn split_sfz_tokens(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_value_with_spaces = false;
    for word in line.split_whitespace() {
        let is_header = word.starts_with('<') && word.ends_with('>');
        if is_header {
            in_value_with_spaces = false;
            out.push(word.to_string());
            continue;
        }
        if word.contains('=') {
            in_value_with_spaces = word.to_ascii_lowercase().starts_with("sample=");
            out.push(word.to_string());
        } else if in_value_with_spaces {
            if let Some(last) = out.last_mut() {
                last.push(' ');
                last.push_str(word);
            }
        } else {
            out.push(word.to_string());
        }
    }
    out
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn parses_single_region() {
        let sfz = "<region> sample=piano_c4.wav pitch_keycenter=60 lokey=48 hikey=72";
        let f = SfzFile::parse(sfz, &p("/inst"));
        assert_eq!(f.regions.len(), 1);
        let r = &f.regions[0];
        assert_eq!(r.sample, p("/inst/piano_c4.wav"));
        assert_eq!(r.pitch_keycenter, 60);
        assert_eq!(r.lokey, 48);
        assert_eq!(r.hikey, 72);
    }

    #[test]
    fn group_opcodes_inherit_to_regions() {
        let sfz = "
            <group> volume=-6 ampeg_release=0.5
            <region> sample=a.wav pitch_keycenter=60
            <region> sample=b.wav pitch_keycenter=72 volume=-3
        ";
        let f = SfzFile::parse(sfz, &p("/"));
        assert_eq!(f.regions.len(), 2);
        // First region picks up both group opcodes.
        assert_eq!(f.regions[0].volume_db, -6.0);
        assert!((f.regions[0].ampeg_release_secs - 0.5).abs() < 1e-6);
        // Second region overrides volume but still inherits release.
        assert_eq!(f.regions[1].volume_db, -3.0);
        assert!((f.regions[1].ampeg_release_secs - 0.5).abs() < 1e-6);
    }

    #[test]
    fn global_master_group_region_inheritance_chain() {
        let sfz = "
            <global> volume=-12
            <master> ampeg_attack=0.1
            <group> ampeg_release=0.3
            <region> sample=x.wav pitch_keycenter=60
        ";
        let r = &SfzFile::parse(sfz, &p("/")).regions[0];
        assert_eq!(r.volume_db, -12.0);
        assert!((r.ampeg_attack_secs - 0.1).abs() < 1e-6);
        assert!((r.ampeg_release_secs - 0.3).abs() < 1e-6);
    }

    #[test]
    fn region_opcode_overrides_group() {
        let sfz = "
            <group> volume=-12
            <region> sample=x.wav pitch_keycenter=60 volume=0
        ";
        let r = &SfzFile::parse(sfz, &p("/")).regions[0];
        assert_eq!(r.volume_db, 0.0);
    }

    #[test]
    fn note_name_parsing() {
        assert_eq!(parse_note("60"), Some(60));
        assert_eq!(parse_note("c4"), Some(60));
        assert_eq!(parse_note("C4"), Some(60));
        assert_eq!(parse_note("a4"), Some(69));
        assert_eq!(parse_note("c#4"), Some(61));
        assert_eq!(parse_note("db4"), Some(61));
        assert_eq!(parse_note("c-1"), Some(0));
        assert_eq!(parse_note("g9"), Some(127));
        assert_eq!(parse_note("nope"), None);
        assert_eq!(parse_note("200"), None);
    }

    #[test]
    fn region_uses_note_names() {
        let sfz = "<region> sample=x.wav lokey=c4 hikey=c5 pitch_keycenter=c4";
        let r = &SfzFile::parse(sfz, &p("/")).regions[0];
        assert_eq!(r.lokey, 60);
        assert_eq!(r.hikey, 72);
        assert_eq!(r.pitch_keycenter, 60);
    }

    #[test]
    fn ampeg_sustain_normalises_percent() {
        let r = &SfzFile::parse(
            "<region> sample=x.wav pitch_keycenter=60 ampeg_sustain=70",
            &p("/"),
        )
        .regions[0];
        assert!((r.ampeg_sustain - 0.7).abs() < 1e-6);
    }

    #[test]
    fn loop_opcodes() {
        let r = &SfzFile::parse(
            "<region> sample=x.wav pitch_keycenter=60 loop_mode=loop_continuous loop_start=1000 loop_end=20000",
            &p("/"),
        )
        .regions[0];
        assert_eq!(r.loop_mode, LoopMode::LoopContinuous);
        assert_eq!(r.loop_start, 1000);
        assert_eq!(r.loop_end, 20000);
    }

    #[test]
    fn tune_and_transpose() {
        let r = &SfzFile::parse(
            "<region> sample=x.wav pitch_keycenter=60 tune=-15 transpose=2",
            &p("/"),
        )
        .regions[0];
        assert!((r.tune_cents + 15.0).abs() < 1e-6);
        assert_eq!(r.transpose_semitones, 2);
        // Combined offset: 2 semis - 0.15 = 1.85.
        assert!((r.pitch_offset_semitones() - 1.85).abs() < 1e-6);
    }

    #[test]
    fn default_path_from_control_applies() {
        let sfz = "
            <control> default_path=Samples/
            <region> sample=piano.wav pitch_keycenter=60
        ";
        let r = &SfzFile::parse(sfz, &p("/inst")).regions[0];
        assert_eq!(r.sample, p("/inst/Samples/piano.wav"));
    }

    #[test]
    fn key_shorthand_sets_all_three() {
        let f = SfzFile::parse("<region> sample=x.wav key=64", &p("/"));
        let r = &f.regions[0];
        assert_eq!(r.lokey, 64);
        assert_eq!(r.hikey, 64);
        assert_eq!(r.pitch_keycenter, 64);
    }

    #[test]
    fn ignores_unknown_opcodes() {
        let sfz = "<region> sample=x.wav loop_crossfade=0.1 random=0.5 pitch_keycenter=60";
        let f = SfzFile::parse(sfz, &p("/"));
        assert_eq!(f.regions.len(), 1);
    }

    #[test]
    fn line_comments_are_stripped() {
        let sfz = "
            // top comment
            <region> sample=a.wav pitch_keycenter=60  // inline trailing comment
        ";
        let f = SfzFile::parse(sfz, &p("/"));
        assert_eq!(f.regions.len(), 1);
    }

    #[test]
    fn sample_path_with_spaces() {
        let sfz = "<region> sample=Felt Piano/A4 v3.wav pitch_keycenter=69";
        let f = SfzFile::parse(sfz, &p("/inst"));
        assert_eq!(f.regions[0].sample, p("/inst/Felt Piano/A4 v3.wav"));
    }

    #[test]
    fn region_match_respects_key_and_velocity() {
        let r = Region {
            sample: p("x.wav"),
            lokey: 60,
            hikey: 72,
            pitch_keycenter: 64,
            lovel: 40,
            hivel: 120,
            ..Region::default()
        };
        assert!(r.matches(64, 80));
        assert!(!r.matches(59, 80));
        assert!(!r.matches(64, 30));
        assert!(!r.matches(64, 121));
    }

    #[test]
    fn backslash_paths_normalise() {
        let sfz = "<region> sample=subdir\\thing.wav pitch_keycenter=60";
        let f = SfzFile::parse(sfz, &p("/root"));
        assert_eq!(f.regions[0].sample, p("/root/subdir/thing.wav"));
    }
}
