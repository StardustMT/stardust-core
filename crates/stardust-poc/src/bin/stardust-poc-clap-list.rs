//! `stardust-poc-clap-list` — discover and describe CLAP plugins.
//!
//! What this proves:
//!
//! - `stardust-plugin::clap::default_clap_search_paths` returns the
//!   correct standard CLAP directories for the platform (+ `CLAP_PATH`).
//! - `stardust-plugin::clap::scan_paths` walks those directories,
//!   loads each `.clap` bundle via `clack-host`, and surfaces every
//!   plugin descriptor with id / name / vendor / version / features.
//! - Broken or non-conformant bundles are reported but do NOT abort the
//!   scan — the host can still list everything else.
//!
//! Run from the workspace:
//!
//! ```text
//! cargo run -p stardust-poc --bin stardust-poc-clap-list
//! ```
//!
//! Useful env vars:
//!
//! - `CLAP_PATH=/some/extra/dir` — adds extra search dirs (colon- or
//!   semicolon-separated per platform conventions).

use anyhow::Result;
use stardust_plugin::clap::{default_clap_search_paths, scan_paths};

fn main() -> Result<()> {
    let paths = default_clap_search_paths();

    println!("CLAP search paths ({}):", paths.len());
    for p in &paths {
        if !p.exists() {
            println!("  {}  [not present]", p.display());
            continue;
        }
        // Quick non-recursive listing of the top level so users can see
        // what's actually under the search root. Helps when the scan
        // finds nothing (typically because the host expected plugins
        // a level deeper or in a different root entirely).
        match std::fs::read_dir(p) {
            Ok(read) => {
                let entries: Vec<_> = read.flatten().collect();
                if entries.is_empty() {
                    println!("  {}  [empty]", p.display());
                } else {
                    println!("  {}  [{} entries]", p.display(), entries.len());
                    for e in entries.iter().take(8) {
                        let kind = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                            "dir "
                        } else {
                            "file"
                        };
                        println!("      {} {}", kind, e.file_name().to_string_lossy());
                    }
                    if entries.len() > 8 {
                        println!("      … +{} more", entries.len() - 8);
                    }
                }
            }
            Err(e) => println!("  {}  [unreadable: {e}]", p.display()),
        }
    }
    println!();

    let scan = scan_paths(&paths);

    if scan.bundles.is_empty() && scan.errors.is_empty() {
        println!("No .clap bundles found.");
        println!(
            "  Drop a .clap into one of the paths above, or set CLAP_PATH \
             to another directory and re-run."
        );
        return Ok(());
    }

    let mut total_plugins = 0usize;
    for bundle in &scan.bundles {
        println!("📦  {}", bundle.path.display());
        if bundle.descriptors.is_empty() {
            println!("    (bundle loaded but advertised no descriptors)");
            continue;
        }
        for d in &bundle.descriptors {
            total_plugins += 1;
            let vendor = if d.vendor.is_empty() {
                "—"
            } else {
                &d.vendor
            };
            let version = if d.version.is_empty() {
                "—"
            } else {
                &d.version
            };
            println!("    • {}  ({})", d.name, d.id);
            println!("         vendor: {}   version: {}", vendor, version);
            if !d.description.is_empty() {
                println!("         {}", d.description);
            }
            if !d.features.is_empty() {
                println!("         features: {}", d.features.join(", "));
            }
        }
    }

    if !scan.errors.is_empty() {
        println!();
        println!("⚠  {} bundle(s) failed to load:", scan.errors.len());
        for (path, msg) in &scan.errors {
            println!("    {}", path.display());
            println!("        {msg}");
        }
    }

    println!();
    println!(
        "Summary: {} bundle(s), {} plugin(s){}",
        scan.bundles.len(),
        total_plugins,
        if scan.errors.is_empty() {
            String::new()
        } else {
            format!(", {} failed", scan.errors.len())
        }
    );

    Ok(())
}
