//! Mtime-keyed CLAP scan cache (stardust-core#1, backing stardust-pit#4).
//!
//! `dlopen`ing every `.clap` bundle on every scan is the expensive part
//! of plugin discovery — each load runs the plugin's static initializers.
//! This module keeps a JSON cache of `{ bundle path, mtime, descriptors }`
//! records so a scan only loads bundles whose on-disk bits actually
//! changed:
//!
//! - **mtime keying follows symlinks and walks bundle directories.** On
//!   macOS a `.clap` is a bundle *directory* (and vendor layouts routinely
//!   symlink them), so the cache key is the newest modification time across
//!   every file reachable inside the bundle — the mtime of a symlink or of
//!   the bundle dir itself would never change when the binary inside does.
//! - **`schemaVersion` gates the file.** A Stardust upgrade that changes
//!   the record layout invalidates the whole cache (full rescan + rewrite)
//!   rather than parsing stale records.
//! - **Load failures are cached too.** A corrupt bundle stays corrupt
//!   until its mtime changes; re-`dlopen`ing it every scan would re-run
//!   whatever made it fail.
//!
//! Concurrency is the caller's concern: this module reads and writes the
//! cache file synchronously (the write is atomic via temp-file + rename).
//! The intended usage — one background rescan thread publishing immutable
//! snapshots — lives in the host app.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::clap::{ClapError, ScanResult, ScannedBundle, discover_paths, load_bundle};

/// Bump when [`CacheEntry`]'s layout changes. A mismatch on read discards
/// the whole cache file.
pub const CACHE_SCHEMA_VERSION: u32 = 1;

/// Serialized cache file shape.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheFile {
    schema_version: u32,
    entries: Vec<CacheEntry>,
}

/// One scanned bundle: its path, the mtime key it was scanned at, and
/// what the scan produced (descriptors, or the load error).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    /// Bundle path as discovered (the symlink path, if a symlink — the
    /// mtime key is what follows targets).
    pub path: PathBuf,
    /// Newest `(secs, nanos)` since the Unix epoch across the bundle's
    /// files, symlinks followed.
    pub mtime: (u64, u32),
    /// Descriptors the bundle's factory exposed. Empty when `error` is set.
    pub descriptors: Vec<crate::clap::PluginDescriptor>,
    /// Load failure message, if the bundle didn't load at `mtime`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

/// What a cached scan did, alongside the merged [`ScanResult`].
#[derive(Debug, Default)]
pub struct CachedScanOutcome {
    /// The full scan result (cache hits + fresh loads merged), same shape
    /// as [`crate::clap::scan_paths`].
    pub result: ScanResult,
    /// Bundles served from the cache without loading.
    pub cache_hits: usize,
    /// Bundles `dlopen`ed this pass (new, changed, or cache invalid).
    pub loaded: usize,
}

/// Scan `dirs` for `.clap` bundles using (and refreshing) the cache at
/// `cache_path`. Only bundles whose mtime key changed since the cached
/// record — plus bundles never seen — are actually loaded. Records for
/// bundles that vanished from disk are dropped.
pub fn scan_paths_cached<P: AsRef<Path>>(dirs: &[P], cache_path: &Path) -> CachedScanOutcome {
    scan_paths_cached_with(dirs, cache_path, load_bundle)
}

/// [`scan_paths_cached`] with an injectable bundle loader — the test seam
/// (loading real bundles means `dlopen`, which tests can't fabricate).
pub fn scan_paths_cached_with<P: AsRef<Path>>(
    dirs: &[P],
    cache_path: &Path,
    loader: impl Fn(&Path) -> Result<ScannedBundle, ClapError>,
) -> CachedScanOutcome {
    let cached = read_cache(cache_path);
    let by_path: std::collections::HashMap<&Path, &CacheEntry> =
        cached.iter().map(|e| (e.path.as_path(), e)).collect();

    let mut outcome = CachedScanOutcome::default();
    let mut fresh: Vec<CacheEntry> = Vec::new();

    for path in discover_paths(dirs) {
        let mtime = bundle_mtime(&path);
        if let Some(entry) = by_path.get(path.as_path()) {
            if entry.mtime == mtime {
                outcome.cache_hits += 1;
                fresh.push((*entry).clone());
                continue;
            }
        }
        outcome.loaded += 1;
        match loader(&path) {
            Ok(b) => fresh.push(CacheEntry {
                path,
                mtime,
                descriptors: b.descriptors,
                error: None,
            }),
            Err(e) => fresh.push(CacheEntry {
                path,
                mtime,
                descriptors: Vec::new(),
                error: Some(format!("{e}")),
            }),
        }
    }

    for entry in &fresh {
        match &entry.error {
            None => outcome.result.bundles.push(ScannedBundle {
                path: entry.path.clone(),
                descriptors: entry.descriptors.clone(),
            }),
            Some(msg) => outcome
                .result
                .errors
                .push((entry.path.clone(), msg.clone())),
        }
    }

    write_cache(cache_path, &fresh);
    outcome
}

/// Read the cache file. Any problem — missing file, malformed JSON, wrong
/// [`CACHE_SCHEMA_VERSION`] — returns an empty cache, which degrades to a
/// full rescan.
fn read_cache(path: &Path) -> Vec<CacheEntry> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<CacheFile>(&raw) {
        Ok(f) if f.schema_version == CACHE_SCHEMA_VERSION => f.entries,
        Ok(f) => {
            tracing::info!(
                found = f.schema_version,
                current = CACHE_SCHEMA_VERSION,
                "plugin cache schema mismatch — full rescan"
            );
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(error = %e, "plugin cache unreadable — full rescan");
            Vec::new()
        }
    }
}

/// Atomically rewrite the cache file (temp file + rename in the same
/// directory). Failures are logged and swallowed — a cache that can't be
/// written just means the next scan is cold.
fn write_cache(path: &Path, entries: &[CacheEntry]) {
    let file = CacheFile {
        schema_version: CACHE_SCHEMA_VERSION,
        entries: entries.to_vec(),
    };
    let Ok(json) = serde_json::to_string(&file) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, path)) {
        tracing::warn!(error = %e, path = %path.display(), "plugin cache write failed");
    }
}

/// The cache key for one bundle: the newest `(secs, nanos)` modification
/// time across every file reachable inside it, following symlinks.
///
/// - A plain `.clap` file (Linux/Windows): its own mtime, symlink target's
///   if it's a symlink (`fs::metadata` follows).
/// - A `.clap` bundle directory (macOS): the max over the directory tree —
///   the binary lives at `Contents/MacOS/<name>`, and the bundle dir's own
///   mtime doesn't change when a nested file is rewritten in place.
fn bundle_mtime(path: &Path) -> (u64, u32) {
    let mut newest = (0u64, 0u32);
    bundle_mtime_recursive(path, 0, &mut newest);
    newest
}

fn bundle_mtime_recursive(path: &Path, depth: usize, newest: &mut (u64, u32)) {
    // Bundle internals are shallow (Contents/MacOS/binary); the cap only
    // guards symlink loops.
    if depth > 6 {
        return;
    }
    // fs::metadata follows symlinks — deliberate (see module docs).
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if let Ok(modified) = meta.modified() {
        if let Ok(d) = modified.duration_since(UNIX_EPOCH) {
            let key = (d.as_secs(), d.subsec_nanos());
            if key > *newest {
                *newest = key;
            }
        }
    }
    if meta.is_dir() {
        let Ok(read) = std::fs::read_dir(path) else {
            return;
        };
        for entry in read.flatten() {
            bundle_mtime_recursive(&entry.path(), depth + 1, newest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fake_loader(
        counter: &AtomicUsize,
    ) -> impl Fn(&Path) -> Result<ScannedBundle, ClapError> + '_ {
        move |path: &Path| {
            counter.fetch_add(1, Ordering::SeqCst);
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin")
                .to_owned();
            Ok(ScannedBundle {
                path: path.to_path_buf(),
                descriptors: vec![crate::clap::PluginDescriptor {
                    id: format!("test.{name}"),
                    name,
                    vendor: "Test".into(),
                    version: "1.0".into(),
                    description: String::new(),
                    features: vec!["instrument".into()],
                }],
            })
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stardust-cache-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_scan_is_all_cache_hits() {
        let dir = temp_dir("hits");
        std::fs::write(dir.join("A.clap"), b"aaaa").unwrap();
        std::fs::write(dir.join("B.clap"), b"bbbb").unwrap();
        let cache = dir.join("cache.json");
        let loads = AtomicUsize::new(0);

        let first = scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));
        assert_eq!(first.loaded, 2);
        assert_eq!(first.cache_hits, 0);
        assert_eq!(first.result.bundles.len(), 2);

        let second = scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));
        assert_eq!(second.loaded, 0);
        assert_eq!(second.cache_hits, 2);
        assert_eq!(second.result.bundles.len(), 2);
        assert_eq!(loads.load(Ordering::SeqCst), 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mtime_change_reloads_only_that_bundle() {
        let dir = temp_dir("mtime");
        std::fs::write(dir.join("A.clap"), b"aaaa").unwrap();
        std::fs::write(dir.join("B.clap"), b"bbbb").unwrap();
        let cache = dir.join("cache.json");
        let loads = AtomicUsize::new(0);

        scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));
        assert_eq!(loads.load(Ordering::SeqCst), 2);

        // Bump A's mtime well past the recorded one.
        let f = std::fs::File::options()
            .write(true)
            .open(dir.join("A.clap"))
            .unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
            .unwrap();

        let rescan = scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));
        assert_eq!(rescan.loaded, 1);
        assert_eq!(rescan.cache_hits, 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removed_bundle_drops_out_and_new_bundle_is_picked_up() {
        let dir = temp_dir("churn");
        std::fs::write(dir.join("A.clap"), b"aaaa").unwrap();
        let cache = dir.join("cache.json");
        let loads = AtomicUsize::new(0);

        scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));

        std::fs::remove_file(dir.join("A.clap")).unwrap();
        std::fs::write(dir.join("C.clap"), b"cccc").unwrap();

        let rescan = scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));
        assert_eq!(rescan.result.bundles.len(), 1);
        assert!(rescan.result.bundles[0].path.ends_with("C.clap"));
        assert_eq!(rescan.loaded, 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn schema_mismatch_forces_full_rescan() {
        let dir = temp_dir("schema");
        std::fs::write(dir.join("A.clap"), b"aaaa").unwrap();
        let cache = dir.join("cache.json");
        let loads = AtomicUsize::new(0);

        scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));
        assert_eq!(loads.load(Ordering::SeqCst), 1);

        // Rewrite the cache with a bumped schema version.
        let raw = std::fs::read_to_string(&cache).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        v["schemaVersion"] = serde_json::json!(CACHE_SCHEMA_VERSION + 1);
        std::fs::write(&cache, v.to_string()).unwrap();

        let rescan = scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));
        assert_eq!(rescan.loaded, 1, "schema mismatch must rescan");
        assert_eq!(rescan.cache_hits, 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_failures_are_cached_until_mtime_changes() {
        let dir = temp_dir("errors");
        std::fs::write(dir.join("BAD.clap"), b"corrupt").unwrap();
        let cache = dir.join("cache.json");
        let loads = AtomicUsize::new(0);
        let failing = |path: &Path| {
            loads.fetch_add(1, Ordering::SeqCst);
            Err(ClapError::PathNotAccessible(path.to_path_buf()))
        };

        let first = scan_paths_cached_with(&[&dir], &cache, failing);
        assert_eq!(first.result.errors.len(), 1);

        let second = scan_paths_cached_with(&[&dir], &cache, failing);
        assert_eq!(second.result.errors.len(), 1);
        assert_eq!(second.loaded, 0, "failure record must be served from cache");
        assert_eq!(loads.load(Ordering::SeqCst), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Cold-vs-warm benchmark against the real system CLAP directories.
    /// `#[ignore]`d: needs plugins installed and real `dlopen`s. Run
    /// locally for the #4 cold-launch numbers:
    ///
    /// ```sh
    /// cargo test -p stardust-plugin --release -- --ignored --nocapture real_system
    /// ```
    #[test]
    #[ignore = "dlopens real system plugins; run locally for benchmark numbers"]
    fn real_system_scan_benchmark() {
        let dirs = crate::clap::default_clap_search_paths();
        let cache =
            std::env::temp_dir().join(format!("stardust-scan-bench-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&cache);

        let t0 = std::time::Instant::now();
        let cold = scan_paths_cached(&dirs, &cache);
        let cold_ms = t0.elapsed().as_millis();

        let t1 = std::time::Instant::now();
        let warm = scan_paths_cached(&dirs, &cache);
        let warm_ms = t1.elapsed().as_millis();

        println!(
            "cold scan: {} bundles loaded in {cold_ms} ms; warm scan: {} cache hits in {warm_ms} ms",
            cold.loaded, warm.cache_hits
        );
        assert_eq!(warm.loaded, 0, "warm scan must be all cache hits");
        let _ = std::fs::remove_file(&cache);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_bundle_keys_on_target_mtime() {
        let dir = temp_dir("symlink");
        let target_dir = temp_dir("symlink-target");
        let target = target_dir.join("Real.clap");
        std::fs::write(&target, b"real").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("Linked.clap")).unwrap();
        let cache = dir.join("cache.json");
        let loads = AtomicUsize::new(0);

        scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));
        assert_eq!(loads.load(Ordering::SeqCst), 1);

        // Touch the TARGET; the symlink itself is untouched.
        let f = std::fs::File::options().write(true).open(&target).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
            .unwrap();

        let rescan = scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));
        assert_eq!(rescan.loaded, 1, "target mtime change must invalidate");

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&target_dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bundle_directory_keys_on_nested_file_mtime() {
        let dir = temp_dir("bundle-dir");
        let bundle = dir.join("Mac.clap");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        let binary = bundle.join("Contents/MacOS/Mac");
        std::fs::write(&binary, b"binary").unwrap();
        let cache = dir.join("cache.json");
        let loads = AtomicUsize::new(0);

        scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));
        assert_eq!(loads.load(Ordering::SeqCst), 1);

        let f = std::fs::File::options().write(true).open(&binary).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
            .unwrap();

        let rescan = scan_paths_cached_with(&[&dir], &cache, fake_loader(&loads));
        assert_eq!(
            rescan.loaded, 1,
            "nested binary mtime change must invalidate the bundle"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
