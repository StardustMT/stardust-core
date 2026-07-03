//! Patch-graph data model.
//!
//! See ADR-0004 (patch graph data model) and ADR-0003 (schema versioning).
//! The Rust types here are a faithful mirror of the TypeScript shape in
//! `stardust-pit/src/src/components/patch-graph/_types.ts`; the wire
//! format is camelCase JSON so the Tauri bridge round-trips with no
//! adapter layer.

pub mod document;
pub mod types;
pub mod validate;

pub use document::{
    CURRENT_SCHEMA_VERSION, Header, PATCH_KIND, PatchDocument, migrate_patch_value,
};
pub use types::{
    CompositeBlock, CompositeId, GraphNode, NodeClass, NodeId, NodeKind, PatchGraph, Port,
    PortConfig, PortDirection, PortId, PromotedPort, SignalKind, StereoChannel, Wire, WireId,
};
pub use validate::ValidationError;
