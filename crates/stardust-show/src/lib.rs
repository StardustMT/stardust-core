//! Show data model.
//!
//! See ADR-0005 (show document data model) and ADR-0003 (schema versioning).
//! A `ShowDocument` is the single on-disk and over-the-wire shape that
//! holds a full Stardust show: songs, patches (each inlining its
//! `PatchGraph`), rig configuration, and user-saved composite blocks.
//!
//! The Rust types here are a faithful mirror of the TypeScript shapes in
//! `stardust-pit/src/src/screens/_seed-data.ts` (rig, song outline) and
//! `stardust-pit/src/src/components/patch-graph/_types.ts` (re-exported
//! `PatchGraph` from `stardust-patch`); the wire format is camelCase JSON
//! so the Tauri bridge round-trips with no adapter layer.

pub mod document;
pub mod types;
pub mod validate;

pub use document::{CURRENT_SCHEMA_VERSION, SHOW_KIND, ShowDocument};
pub use types::{BlockId, Patch, PatchId, Rig, RigSource, SavedBlock, Show, Song, SongId};
pub use validate::ShowValidationError;

// Re-exports from stardust-patch so consumers can depend on just stardust-show
// for the full surface area they need to load and inspect a show.
pub use stardust_patch::{Header, NodeKind, PatchGraph, ValidationError};
