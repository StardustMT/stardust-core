//! Persistence-layer wrapper around `PatchGraph`.
//!
//! Per ADR-0003, every persisted file carries a header with `kind`,
//! `schema_version`, `stardust_version`, and `saved_at`. The bare graph
//! lives at runtime; the document is the on-disk and over-the-wire shape.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::types::PatchGraph;

pub const PATCH_KIND: &str = "stardust.patch";
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    pub kind: String,
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stardust_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub saved_at: Option<String>,
}

impl Header {
    pub fn current() -> Self {
        Self {
            kind: PATCH_KIND.to_owned(),
            schema_version: CURRENT_SCHEMA_VERSION,
            stardust_version: None,
            saved_at: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchDocument {
    #[serde(flatten)]
    pub header: Header,
    pub graph: PatchGraph,
}

impl PatchDocument {
    pub fn new(graph: PatchGraph) -> Self {
        Self {
            header: Header::current(),
            graph,
        }
    }

    pub fn from_json(s: &str) -> Result<Self, LoadError> {
        let mut value: Value = serde_json::from_str(s)?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or(LoadError::MissingHeader)?
            .to_owned();
        if kind != PATCH_KIND {
            return Err(LoadError::WrongKind {
                expected: PATCH_KIND,
                found: kind,
            });
        }
        let version = value
            .get("schemaVersion")
            .and_then(Value::as_u64)
            .ok_or(LoadError::MissingHeader)? as u32;
        if version > CURRENT_SCHEMA_VERSION {
            return Err(LoadError::NewerSchema {
                document: version,
                current: CURRENT_SCHEMA_VERSION,
            });
        }
        migrate_patch_value(&mut value, version);
        let doc: PatchDocument = serde_json::from_value(value)?;
        Ok(doc)
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// Run the migration chain on a raw `stardust.patch` JSON value from
/// `from_version` up to [`CURRENT_SCHEMA_VERSION`]. Mutates `value` in
/// place and bumps its `schemaVersion` field to the current version.
///
/// Exposed at crate level so `stardust-show` can reuse the per-graph
/// rewrites when migrating embedded patch graphs.
pub fn migrate_patch_value(value: &mut Value, from_version: u32) {
    if let Some(graph) = value.get_mut("graph") {
        migrate_graph_value(graph, from_version, CURRENT_SCHEMA_VERSION);
    }
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "schemaVersion".to_owned(),
            Value::from(CURRENT_SCHEMA_VERSION),
        );
    }
}

/// Run the per-graph rewrites for schema steps `from_version` up to (but
/// not past) `to_version`. Public so `stardust-show` can apply exactly the
/// steps its own migration chain needs on embedded patch graphs — the show
/// v2→v3 migration harvests node-level `hardwareBinding` blobs itself, so
/// it must be able to run the graph 1→2 step *without* the 2→3 blob
/// removal firing first.
pub fn migrate_graph_value(graph: &mut Value, from_version: u32, to_version: u32) {
    let mut v = from_version;
    while v < to_version {
        match v {
            1 => migrate_graph_v1_to_v2(graph),
            2 => migrate_graph_v2_to_v3(graph),
            _ => break,
        }
        v += 1;
    }
}

/// v1 → v2: rename the built-in synth node kind from `instrument.sine` to
/// `instrument.testtone`. The node is no longer user-facing as of v0.6.0;
/// it survives as a diagnostic surface only.
fn migrate_graph_v1_to_v2(graph: &mut Value) {
    let Some(nodes) = graph.get_mut("nodes").and_then(Value::as_array_mut) else {
        return;
    };
    for node in nodes {
        let Some(kind) = node.get_mut("kind") else {
            continue;
        };
        if kind.as_str() == Some("instrument.sine") {
            *kind = Value::from("instrument.testtone");
        }
    }
}

/// v2 → v3: node-level `hardwareBinding` ceases to exist — hardware
/// identity lives on rig components (`stardust-show` schema v3), which
/// source nodes reference via `config.rigComponentId`. A standalone patch
/// document has no rig to attach bindings to, so the blob is dropped here;
/// the show migration converts blobs into rig components *before* this
/// step would discard them.
fn migrate_graph_v2_to_v3(graph: &mut Value) {
    let Some(nodes) = graph.get_mut("nodes").and_then(Value::as_array_mut) else {
        return;
    };
    for node in nodes {
        let Some(config) = node.get_mut("config").and_then(Value::as_object_mut) else {
            continue;
        };
        config.remove("hardwareBinding");
        if config.is_empty() {
            node.as_object_mut()
                .expect("node with config is an object")
                .remove("config");
        }
    }
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("not a stardust patch document: kind was {found:?}, expected {expected:?}")]
    WrongKind {
        expected: &'static str,
        found: String,
    },

    #[error("patch document is missing required header fields (kind, schemaVersion)")]
    MissingHeader,

    #[error(
        "patch document is schema v{document}, but this build only understands up to v{current}"
    )]
    NewerSchema { document: u32, current: u32 },

    #[error("malformed patch JSON: {0}")]
    Json(#[from] serde_json::Error),
}
