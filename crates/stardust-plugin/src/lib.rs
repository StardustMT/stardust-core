//! # stardust-plugin
//!
//! Plugin discovery and loading for stardust-core.
//!
//! - **CLAP** support (this crate, behind the `clap` feature) wraps
//!   [`clack_host`] to scan the platform's standard plugin directories,
//!   load `.clap` bundles, and surface their plugin descriptors.
//! - **VST3** support is planned for a later phase via a small C++ shim
//!   around the Steinberg VST3 SDK.
//!
//! Out-of-process (sandboxed) hosting lives in `stardust-ipc`. This crate
//! covers the in-process API surface.
//!
//! # Quickstart
//!
//! ```no_run
//! use stardust_plugin::clap::{default_clap_search_paths, scan_paths};
//!
//! let mut paths = default_clap_search_paths();
//! paths.extend(std::env::var_os("CLAP_PATH").into_iter().flat_map(|p| {
//!     std::env::split_paths(&p).collect::<Vec<_>>()
//! }));
//!
//! let scan = scan_paths(&paths);
//! for bundle in &scan.bundles {
//!     println!("{} ({} plugins)", bundle.path.display(), bundle.descriptors.len());
//!     for d in &bundle.descriptors {
//!         println!("  - {} [{}] by {}", d.name, d.id, d.vendor);
//!     }
//! }
//! ```
//!
//! Loading a `.clap` bundle is **unsafe** at the boundary — `clack-host`
//! has to dynamically load native code which can do anything. Stardust
//! exposes a safe wrapper that returns descriptor information only;
//! instantiating plugins for audio processing comes in a later phase
//! with the sandbox + process-isolation work in `stardust-ipc`.

#![doc(html_root_url = "https://docs.rs/stardust-plugin/0.0.1")]
#![warn(missing_docs)]

#[cfg(feature = "clap")]
pub mod cache;
#[cfg(feature = "clap")]
pub mod clap;
