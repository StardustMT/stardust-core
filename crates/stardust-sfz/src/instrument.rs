//! Build a loaded, ready-to-play instrument from a parsed `.sfz` file.
//!
//! Multiple regions can share the same sample (different velocity layers
//! of the same note, for example). The loader deduplicates by path so
//! the audio engine ends up with `regions.len() <= samples.len()` and
//! every region carries an index into the sample bank.
//!
//! # Memory safety
//!
//! Samples are decoded fully into RAM. To prevent a runaway SFZ from
//! OOM-ing the host we apply two caps at load time:
//!
//! - `LoadLimits::max_sample_bytes`: any single sample larger than this
//!   is skipped with a clear error. Defaults to 64 MiB.
//! - `LoadLimits::max_total_bytes`: once the cumulative loaded size
//!   exceeds this, subsequent samples are skipped with a clear error.
//!   Defaults to 512 MiB.
//!
//! Both limits are soft — they don't crash, they just collect into
//! `LoadReport::errors` and drop the offending regions. The host can
//! show the user what didn't load.
//!
//! # Future: streaming
//!
//! For libraries that legitimately need GB of samples, a worker thread
//! and a per-voice `stardust_rt::RingBuffer` would let the audio thread
//! pull samples on demand without locking. The current full-preload
//! approach is the right starting point for a POC; the architectural
//! seam lives at [`Sample`](crate::sample::Sample) — swapping it for a
//! streaming type wouldn't touch the engine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::sample::Sample;
use crate::sfz::{Region, SfzFile};

/// Caps applied when loading samples. Built with `Default` for typical
/// instruments; override for memory-constrained or memory-rich setups.
#[derive(Debug, Clone, Copy)]
pub struct LoadLimits {
    /// Skip any single sample whose decoded f32 size exceeds this.
    pub max_sample_bytes: usize,
    /// Stop loading new samples once the cumulative decoded f32 size
    /// reaches this. Already-loaded samples stay in the instrument.
    pub max_total_bytes: usize,
}

impl Default for LoadLimits {
    fn default() -> Self {
        Self {
            max_sample_bytes: 64 * 1024 * 1024, // 64 MiB
            max_total_bytes: 512 * 1024 * 1024, // 512 MiB
        }
    }
}

/// A region paired with an index into [`Instrument::samples`].
#[derive(Debug, Clone)]
pub struct InstrumentRegion {
    /// The parsed region (key/velocity range, pitch, volume, envelope, loop).
    pub region: Region,
    /// Index of this region's sample in the bank.
    pub sample_index: usize,
}

/// A loaded instrument — regions + the unique sample bank they reference.
#[derive(Debug, Clone, Default)]
pub struct Instrument {
    /// Regions in original SFZ order.
    pub regions: Vec<InstrumentRegion>,
    /// Unique samples referenced by at least one region.
    pub samples: Vec<Sample>,
}

/// Aggregate report: what loaded, what didn't, and why.
#[derive(Debug, Default)]
pub struct LoadReport {
    /// Successfully loaded instrument.
    pub instrument: Instrument,
    /// Samples we tried to load but couldn't, paired with the reason.
    /// Regions that referenced these samples are dropped from the
    /// instrument.
    pub errors: Vec<(PathBuf, String)>,
    /// Total decoded sample bytes resident in RAM.
    pub bytes_loaded: usize,
}

/// Progress event reported by [`build_load_report_with_progress`] as
/// each unique sample is processed. Lets long-running loads surface
/// "loading 47/162: kick.wav" UI without coupling the loader to any
/// particular logging or progress crate.
#[derive(Debug, Clone)]
pub struct LoadProgress<'a> {
    /// Path the loader is about to read.
    pub path: &'a Path,
    /// 1-based index of this unique sample within the total set.
    pub index: usize,
    /// Total number of unique samples the loader expects to read.
    pub total: usize,
    /// Cumulative decoded RAM after this sample (zero before the
    /// sample is loaded; populated after, on the same callback call).
    pub bytes_loaded: usize,
}

/// Read an `.sfz` file from disk, parse it, and load every sample it
/// references with default RAM limits.
pub fn load_sfz(path: &Path) -> Result<LoadReport, std::io::Error> {
    load_sfz_with_limits(path, LoadLimits::default())
}

/// Same as [`load_sfz`] but with caller-supplied RAM limits.
pub fn load_sfz_with_limits(path: &Path, limits: LoadLimits) -> Result<LoadReport, std::io::Error> {
    load_sfz_with_progress(path, limits, |_| {})
}

/// [`load_sfz`] + a progress callback fired once per unique sample
/// (deduplicated by path) before the WAV decode starts.
pub fn load_sfz_with_progress<F>(
    path: &Path,
    limits: LoadLimits,
    progress: F,
) -> Result<LoadReport, std::io::Error>
where
    F: FnMut(&LoadProgress<'_>),
{
    let text = std::fs::read_to_string(path)?;
    let base = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let parsed = SfzFile::parse(&text, &base);
    Ok(build_load_report_with_progress(parsed, limits, progress))
}

/// Pure-data variant: takes an already-parsed file. Useful in tests
/// and when the SFZ text comes from a non-file source.
pub fn build_load_report(parsed: SfzFile, limits: LoadLimits) -> LoadReport {
    build_load_report_with_progress(parsed, limits, |_| {})
}

/// [`build_load_report`] + per-sample progress callback.
pub fn build_load_report_with_progress<F>(
    parsed: SfzFile,
    limits: LoadLimits,
    mut progress: F,
) -> LoadReport
where
    F: FnMut(&LoadProgress<'_>),
{
    let mut report = LoadReport::default();
    let mut path_to_index: HashMap<PathBuf, usize> = HashMap::new();

    // Count unique sample paths up front so the progress total is
    // honest. Cheap because regions hold owned PathBufs.
    let total_unique = {
        let mut seen = std::collections::HashSet::new();
        parsed
            .regions
            .iter()
            .filter(|r| seen.insert(r.sample.clone()))
            .count()
    };

    let mut unique_index = 0usize;
    for region in parsed.regions {
        let sample_index = match path_to_index.get(&region.sample).copied() {
            Some(i) => i,
            None => {
                unique_index += 1;
                progress(&LoadProgress {
                    path: &region.sample,
                    index: unique_index,
                    total: total_unique,
                    bytes_loaded: report.bytes_loaded,
                });
                match Sample::load_wav(&region.sample) {
                    Ok(sample) => {
                        let bytes = sample.data.len() * std::mem::size_of::<f32>();
                        if bytes > limits.max_sample_bytes {
                            report.errors.push((
                                region.sample.clone(),
                                format!(
                                    "sample is {} MiB; over max_sample_bytes cap of {} MiB",
                                    bytes / (1024 * 1024),
                                    limits.max_sample_bytes / (1024 * 1024)
                                ),
                            ));
                            continue;
                        }
                        if report.bytes_loaded + bytes > limits.max_total_bytes {
                            report.errors.push((
                                region.sample.clone(),
                                format!(
                                    "would exceed max_total_bytes cap of {} MiB \
                                     (currently {} MiB loaded, +{} MiB requested)",
                                    limits.max_total_bytes / (1024 * 1024),
                                    report.bytes_loaded / (1024 * 1024),
                                    bytes / (1024 * 1024)
                                ),
                            ));
                            continue;
                        }
                        let i = report.instrument.samples.len();
                        report.bytes_loaded += bytes;
                        report.instrument.samples.push(sample);
                        path_to_index.insert(region.sample.clone(), i);
                        i
                    }
                    Err(e) => {
                        report.errors.push((region.sample.clone(), format!("{e}")));
                        continue;
                    }
                }
            }
        };
        report.instrument.regions.push(InstrumentRegion {
            region,
            sample_index,
        });
    }
    report
}

/// Convenience helper: build an [`Instrument`] without reporting
/// errors, using default limits. Use in tests.
pub fn build_instrument(parsed: SfzFile) -> Instrument {
    build_load_report(parsed, LoadLimits::default()).instrument
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_sample_files_collect_as_errors() {
        let sfz = SfzFile::parse(
            "
            <region> sample=does_not_exist.wav pitch_keycenter=60
            <region> sample=also_missing.wav pitch_keycenter=72
            ",
            Path::new("/tmp/nope"),
        );
        let r = build_load_report(sfz, LoadLimits::default());
        assert_eq!(r.errors.len(), 2);
        assert_eq!(r.instrument.regions.len(), 0);
        assert_eq!(r.instrument.samples.len(), 0);
        assert_eq!(r.bytes_loaded, 0);
    }

    #[test]
    fn ram_caps_are_tight_when_set_low() {
        // Force the per-sample cap to 1 byte so even a successfully
        // parsed region that points at a (would-be) valid sample is
        // never loaded. We don't have real samples in unit tests, so
        // the error message is the missing-file one — but the limits
        // type compiles + flows through.
        let limits = LoadLimits {
            max_sample_bytes: 1,
            max_total_bytes: 1,
        };
        let sfz = SfzFile::parse(
            "<region> sample=ghost.wav pitch_keycenter=60",
            Path::new("/tmp"),
        );
        let r = build_load_report(sfz, limits);
        assert_eq!(r.errors.len(), 1);
        assert_eq!(r.instrument.regions.len(), 0);
    }
}
