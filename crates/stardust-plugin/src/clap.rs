//! CLAP plugin discovery and metadata.
//!
//! Thin wrapper over [`clack_host`] focused on the two things the host
//! needs before audio: **find** `.clap` bundles on the user's machine,
//! and **describe** the plugins inside each bundle. Instantiation for
//! audio processing comes later, behind the sandbox boundary.

use std::ffi::CStr;
use std::path::{Path, PathBuf};

use clack_host::entry::PluginEntryError;
use clack_host::prelude::{HostHandlers, HostInfo, SharedHandler};
use thiserror::Error;

pub use clack_host::entry::PluginEntry;

// Re-export the parts of clack-host any consumer of this crate would
// need to actually instantiate + drive a plugin. Keeps the
// stardust-plugin API surface coherent and lets callers stay off the
// clack-host crate root directly.
pub use clack_host::events::Pckn;
pub use clack_host::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent};
pub use clack_host::events::io::{EventBuffer, InputEvents, OutputEvents};
pub use clack_host::plugin::PluginInstance;
pub use clack_host::process::audio_buffers::{
    AudioPortBuffer, AudioPortBufferType, AudioPorts, InputChannel,
};
pub use clack_host::process::{
    PluginAudioConfiguration, ProcessStatus, StartedPluginAudioProcessor,
    StoppedPluginAudioProcessor,
};
pub use clack_host::utils::ClapId;

// =============================================================================
// Stardust host implementation
// =============================================================================

/// The host identity Stardust presents to CLAP plugins.
///
/// `name` / `vendor` / `url` / `version` are reported back to plugins
/// via the CLAP host info struct so plugins can do telemetry, pick
/// host-specific workarounds, etc. This is the "I am Stardust" badge.
pub fn host_info() -> HostInfo {
    HostInfo::new(
        "Stardust",
        "Stardust",
        "https://github.com/StardustMT",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("static HostInfo strings must be valid")
}

/// Stub host shared-state handler. Plugins call `request_*` on this
/// when they want the host to do something off-thread; for the POC we
/// log + ignore. Real implementations will queue these requests into a
/// ring buffer for the engine to drain on its main-thread tick.
pub struct StardustHostShared;

impl<'a> SharedHandler<'a> for StardustHostShared {
    fn request_restart(&self) {
        tracing::debug!(target: "stardust_plugin", "plugin requested restart (ignored)");
    }
    fn request_process(&self) {
        tracing::debug!(target: "stardust_plugin", "plugin requested process (ignored)");
    }
    fn request_callback(&self) {
        tracing::debug!(target: "stardust_plugin", "plugin requested main-thread callback (ignored)");
    }
}

/// Top-level host type. All three thread-bound state types are unit
/// stubs for the POC — real engine code will replace them with types
/// that own request queues, parameter caches, etc.
pub struct StardustHost;

impl HostHandlers for StardustHost {
    type Shared<'a> = StardustHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

// =============================================================================
// Errors
// =============================================================================

/// Errors returned when scanning or loading CLAP bundles.
#[derive(Debug, Error)]
pub enum ClapError {
    /// The path doesn't exist or isn't readable.
    #[error("CLAP path not accessible: {0}")]
    PathNotAccessible(PathBuf),

    /// `clack-host` failed to load the bundle. The plugin file may be
    /// corrupt, built for the wrong architecture, or otherwise non-
    /// conformant. Causes are reported verbatim from clack-host.
    #[error("failed to load CLAP entry: {0}")]
    Load(String),

    /// The bundle loaded but exposed no plugin factory. Spec-conformant
    /// `.clap` files always expose one; this usually means a malformed
    /// or non-CLAP file with a `.clap` extension.
    #[error("CLAP bundle exposed no plugin factory")]
    NoFactory,
}

impl From<PluginEntryError> for ClapError {
    fn from(e: PluginEntryError) -> Self {
        ClapError::Load(format!("{e}"))
    }
}

// =============================================================================
// Descriptor (owned snapshot)
// =============================================================================

/// A snapshot of the metadata for one plugin advertised by a CLAP bundle.
///
/// All fields are owned `String`s rather than borrowed `&CStr`s so the
/// descriptor outlives the loaded bundle. Empty strings are normalised to
/// `String::new()` so callers can render them without special-casing
/// `Option`. The `id` and `name` fields are mandatory per the CLAP spec —
/// bundles missing either are skipped at scan time and won't appear in
/// the returned list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginDescriptor {
    /// Globally unique identifier (e.g. `com.u-he.diva`). Required.
    pub id: String,
    /// User-facing display name (e.g. `Diva`). Required.
    pub name: String,
    /// Vendor / author. Empty if unset.
    pub vendor: String,
    /// Plugin version string. Empty if unset.
    pub version: String,
    /// Free-text description. Empty if unset.
    pub description: String,
    /// CLAP feature tags (e.g. `"audio-effect"`, `"instrument"`,
    /// `"stereo"`). Order is preserved as advertised by the plugin.
    pub features: Vec<String>,
}

// =============================================================================
// Scan result types
// =============================================================================

/// One `.clap` bundle on disk and the plugins it advertises.
#[derive(Debug, Clone)]
pub struct ScannedBundle {
    /// Filesystem path of the `.clap` file or macOS bundle directory.
    pub path: PathBuf,
    /// Every plugin the bundle's factory exposed.
    pub descriptors: Vec<PluginDescriptor>,
}

/// Result of a directory scan.
///
/// Successfully-loaded bundles end up in `bundles`. Files that failed to
/// load (wrong arch, corrupt, etc.) are reported in `errors` with their
/// path + the underlying message so the host can surface them without
/// poisoning the whole scan.
#[derive(Debug, Default, Clone)]
pub struct ScanResult {
    /// Bundles that loaded and produced at least one descriptor.
    pub bundles: Vec<ScannedBundle>,
    /// Bundles that failed to load, paired with the error message.
    pub errors: Vec<(PathBuf, String)>,
}

// =============================================================================
// Discovery
// =============================================================================

/// Standard search paths for CLAP plugins on the current platform, plus
/// `CLAP_PATH` if set. Order is not significant — callers can dedupe
/// however they prefer (we do basic dedup of paths that compare equal).
///
/// Paths that don't exist are still included so callers can decide what
/// to do with them (e.g. show "no plugins installed in `~/.clap`" in a
/// settings panel).
///
/// Reference: <https://github.com/free-audio/clap/blob/main/include/clap/entry.h>
pub fn default_clap_search_paths() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs_home() {
            out.push(home.join("Library/Audio/Plug-Ins/CLAP"));
        }
        out.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(p) = std::env::var_os("COMMONPROGRAMFILES") {
            out.push(PathBuf::from(p).join("CLAP"));
        }
        if let Some(p) = std::env::var_os("LOCALAPPDATA") {
            out.push(PathBuf::from(p).join("Programs/Common/CLAP"));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(home) = dirs_home() {
            out.push(home.join(".clap"));
        }
        out.push(PathBuf::from("/usr/lib/clap"));
        out.push(PathBuf::from("/usr/local/lib/clap"));
    }

    // Honour CLAP_PATH (colon- or semicolon-separated per std::env::split_paths).
    if let Some(env) = std::env::var_os("CLAP_PATH") {
        out.extend(std::env::split_paths(&env));
    }

    dedupe_paths(out)
}

#[cfg(any(target_os = "macos", all(unix, not(target_os = "macos"))))]
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn dedupe_paths(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|p| seen.insert(p.clone()));
    v
}

// =============================================================================
// Scanning
// =============================================================================

/// Maximum directory depth the scanner walks. Most vendors install one
/// or two levels deep (`CLAP/Vendor/Plugin.clap`); a small cap prevents
/// pathological symlink loops or accidental scans of system-wide trees.
const MAX_SCAN_DEPTH: usize = 4;

/// Scan one directory **recursively** for `.clap` bundles and describe
/// each. Most vendors install at least one level deep (e.g.
/// `CLAP/u-he/Diva.clap`), so a top-level-only scan misses almost
/// everything in practice.
///
/// On macOS `.clap` is itself a bundle DIRECTORY — the scanner treats
/// any path ending in `.clap` as terminal and does not descend into it.
/// Symlinks are followed; depth is capped at [`MAX_SCAN_DEPTH`] to
/// guard against pathological loops.
pub fn scan_dir(dir: &Path) -> ScanResult {
    let mut result = ScanResult::default();
    for path in discover_dir(dir) {
        match load_bundle(&path) {
            Ok(b) => result.bundles.push(b),
            Err(e) => result.errors.push((path, format!("{e}"))),
        }
    }
    result
}

/// Walk one directory for `.clap` bundle paths **without loading them** —
/// the discovery half of [`scan_dir`]. The mtime-keyed cache
/// ([`crate::cache`]) uses this to decide which bundles actually need a
/// `dlopen`.
pub fn discover_dir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    discover_dir_recursive(dir, 0, &mut out);
    out
}

/// Walk multiple directories for `.clap` bundle paths, deduped by path
/// equality (first hit wins), matching [`scan_paths`]' dedup rule.
pub fn discover_paths<P: AsRef<Path>>(dirs: &[P]) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::<PathBuf>::new();
    let mut out = Vec::new();
    for dir in dirs {
        for p in discover_dir(dir.as_ref()) {
            if seen.insert(p.clone()) {
                out.push(p);
            }
        }
    }
    out
}

fn discover_dir_recursive(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return, // missing/unreadable dir is not an error
    };
    for entry in read.flatten() {
        let path = entry.path();
        let is_clap = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("clap"))
            .unwrap_or(false);
        if is_clap {
            // Terminal: a .clap file on Linux/Windows or a .clap bundle
            // dir on macOS — either way, don't descend into it.
            out.push(path);
            continue;
        }
        // Recurse into regular subdirectories (vendor folders etc.).
        let is_dir = entry
            .file_type()
            .map(|t| t.is_dir() || t.is_symlink())
            .unwrap_or(false);
        if is_dir {
            discover_dir_recursive(&path, depth + 1, out);
        }
    }
}

/// Scan multiple directories and merge the results. Duplicate `.clap`
/// paths (the same file picked up from two search roots) are deduped by
/// path equality; the first one wins.
pub fn scan_paths<P: AsRef<Path>>(dirs: &[P]) -> ScanResult {
    let mut merged = ScanResult::default();
    let mut seen = std::collections::HashSet::<PathBuf>::new();
    for dir in dirs {
        let r = scan_dir(dir.as_ref());
        for b in r.bundles {
            if seen.insert(b.path.clone()) {
                merged.bundles.push(b);
            }
        }
        for (p, msg) in r.errors {
            if seen.insert(p.clone()) {
                merged.errors.push((p, msg));
            }
        }
    }
    merged
}

/// Load a single `.clap` file and read its plugin descriptors.
///
/// Loading is unsafe at the FFI boundary (a malicious or buggy plugin
/// can execute arbitrary code at load time), so this function is the
/// "I really mean it" gate around `clack-host`'s `PluginEntry::load`.
/// Stardust accepts the risk in v0.x; once the IPC sandbox lands in
/// Phase 2, scanning will run in a child process and this becomes a
/// trust-boundary internal detail.
pub fn load_bundle(path: &Path) -> Result<ScannedBundle, ClapError> {
    if !path.exists() {
        return Err(ClapError::PathNotAccessible(path.to_path_buf()));
    }
    // SAFETY: loading any third-party dynamic library is inherently
    // unsafe. The caller has opted into running CLAP plugins, which the
    // CLAP spec assumes are trusted code. See module docs.
    let entry = unsafe { PluginEntry::load(path) }?;
    let factory = entry.get_plugin_factory().ok_or(ClapError::NoFactory)?;
    let mut descriptors = Vec::with_capacity(factory.plugin_count() as usize);
    for d in factory.plugin_descriptors() {
        // The id + name fields are mandatory per CLAP. Skip any entry
        // missing them rather than poisoning the bundle.
        let Some(id) = cstr_to_owned(d.id()) else {
            continue;
        };
        let Some(name) = cstr_to_owned(d.name()) else {
            continue;
        };
        descriptors.push(PluginDescriptor {
            id,
            name,
            vendor: cstr_to_owned(d.vendor()).unwrap_or_default(),
            version: cstr_to_owned(d.version()).unwrap_or_default(),
            description: cstr_to_owned(d.description()).unwrap_or_default(),
            features: d
                .features()
                .filter_map(|f| cstr_to_owned(Some(f)))
                .collect(),
        });
    }
    Ok(ScannedBundle {
        path: path.to_path_buf(),
        descriptors,
    })
}

/// Convert an `Option<&CStr>` to an owned, UTF-8 `String`. Returns `None`
/// if the input is `None` or contains non-UTF-8 bytes (CLAP doesn't
/// require UTF-8 — we lossily skip the field in that case rather than
/// fabricating replacement characters).
fn cstr_to_owned(s: Option<&CStr>) -> Option<String> {
    s.and_then(|c| c.to_str().ok().map(|s| s.to_owned()))
        .filter(|s| !s.is_empty())
}
