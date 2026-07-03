//! Persistence-layer wrapper around `Show`.
//!
//! Per ADR-0003 / ADR-0005, every persisted file carries a header with
//! `kind`, `schema_version`, `stardust_version`, and `saved_at`. The bare
//! `Show` lives at runtime; the document is the on-disk and over-the-wire
//! shape.
//!
//! The `Header` struct is re-used from `stardust-patch` — both document
//! types share the exact same header layout, so a third crate just to hold
//! one struct would be ceremony. See ADR-0005.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use stardust_patch::{Header, migrate_patch_value};

use crate::types::Show;

pub const SHOW_KIND: &str = "stardust.show";
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShowDocument {
    #[serde(flatten)]
    pub header: Header,
    pub show: Show,
}

impl ShowDocument {
    pub fn new(show: Show) -> Self {
        Self {
            header: Header {
                kind: SHOW_KIND.to_owned(),
                schema_version: CURRENT_SCHEMA_VERSION,
                stardust_version: None,
                saved_at: None,
            },
            show,
        }
    }

    pub fn from_json(s: &str) -> Result<Self, LoadError> {
        let mut value: Value = serde_json::from_str(s)?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or(LoadError::MissingHeader)?
            .to_owned();
        if kind != SHOW_KIND {
            return Err(LoadError::WrongKind {
                expected: SHOW_KIND,
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
        migrate_show_value(&mut value, version);
        let doc: ShowDocument = serde_json::from_value(value)?;
        Ok(doc)
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// Run the migration chain on a raw `stardust.show` JSON value from
/// `from_version` up to [`CURRENT_SCHEMA_VERSION`].
pub fn migrate_show_value(value: &mut Value, from_version: u32) {
    let mut v = from_version;
    while v < CURRENT_SCHEMA_VERSION {
        match v {
            1 => migrate_show_v1_to_v2(value),
            _ => break,
        }
        v += 1;
    }
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "schemaVersion".to_owned(),
            Value::from(CURRENT_SCHEMA_VERSION),
        );
    }
}

/// v1 → v2: every embedded patch graph runs through the v1→v2 patch
/// migration (renames `instrument.sine` → `instrument.testtone`).
fn migrate_show_v1_to_v2(value: &mut Value) {
    let Some(songs) = value
        .get_mut("show")
        .and_then(|s| s.get_mut("songs"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for song in songs {
        let Some(patches) = song.get_mut("patches").and_then(Value::as_array_mut) else {
            continue;
        };
        for patch in patches {
            if let Some(graph) = patch.get_mut("graph") {
                // Wrap the bare graph in a synthetic patch-doc-shaped
                // value so we can call the shared migration entry-point.
                let mut wrapper = serde_json::json!({ "graph": graph.take() });
                migrate_patch_value(&mut wrapper, 1);
                if let Some(migrated_graph) = wrapper.get_mut("graph") {
                    *graph = migrated_graph.take();
                }
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("not a stardust show document: kind was {found:?}, expected {expected:?}")]
    WrongKind {
        expected: &'static str,
        found: String,
    },

    #[error("show document is missing required header fields (kind, schemaVersion)")]
    MissingHeader,

    #[error(
        "show document is schema v{document}, but this build only understands up to v{current}"
    )]
    NewerSchema { document: u32, current: u32 },

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
