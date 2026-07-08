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

use stardust_patch::{Header, migrate_graph_value};

use crate::types::Show;

pub const SHOW_KIND: &str = "stardust.show";
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

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
            2 => migrate_show_v2_to_v3(value),
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

/// Visit every embedded patch graph in a raw show value.
fn for_each_graph(value: &mut Value, mut f: impl FnMut(&mut Value)) {
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
                f(graph);
            }
        }
    }
}

/// v1 → v2: every embedded patch graph runs through the graph v1→v2
/// rewrite (renames `instrument.sine` → `instrument.testtone`). Only the
/// 1→2 step — later graph steps belong to later *show* migrations, which
/// need to see the pre-migration shape (v2→v3 harvests binding blobs
/// before the graph step would discard them).
fn migrate_show_v1_to_v2(value: &mut Value) {
    for_each_graph(value, |graph| migrate_graph_value(graph, 1, 2));
}

/// v2 → v3: hardware identity moves from patch source nodes to rig
/// components (stardust-pit#122).
///
/// - `show.rig.sources` (`{ kind, label }`) become `show.rig.components`
///   (`{ id, kind, name, config? }`), unbound.
/// - Every source node carrying a v2 `config.hardwareBinding` blob with a
///   device identity gets an equivalent rig component — one per distinct
///   (kind, deviceId, deviceName, channel) — and references it via
///   `config.rigComponentId`. Keyboard `noteRange` filters union into the
///   component's learned key range; `ccRange` narrowing is carried over.
/// - `hardwareBinding` blobs with no device identity (`deviceId: null`,
///   no name — the "any device" path) are dropped: the concept is removed,
///   and an unassigned node is silent by design.
fn migrate_show_v2_to_v3(value: &mut Value) {
    // -- 1. rig.sources -> rig.components ------------------------------
    let mut components: Vec<Value> = Vec::new();
    let mut next_id = 1u32;
    if let Some(rig) = value
        .get_mut("show")
        .and_then(|s| s.get_mut("rig"))
        .and_then(Value::as_object_mut)
    {
        if let Some(sources) = rig.remove("sources") {
            for src in sources.as_array().into_iter().flatten() {
                let kind = src.get("kind").cloned().unwrap_or(Value::Null);
                let name = src
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("Component")
                    .to_owned();
                components.push(serde_json::json!({
                    "id": format!("rc-{next_id}"),
                    "kind": kind,
                    "name": name,
                }));
                next_id += 1;
            }
        }
    }

    // -- 2. harvest node-level hardwareBinding blobs -------------------
    // Key: (kind, deviceId, deviceName, channel) -> component array index.
    let mut by_binding: std::collections::HashMap<(String, String, String, String), usize> =
        std::collections::HashMap::new();

    for_each_graph(value, |graph| {
        let Some(nodes) = graph.get_mut("nodes").and_then(Value::as_array_mut) else {
            return;
        };
        for node in nodes {
            let kind = node
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let Some(config) = node.get_mut("config").and_then(Value::as_object_mut) else {
                continue;
            };
            let Some(binding) = config.remove("hardwareBinding") else {
                if config.is_empty() {
                    node.as_object_mut()
                        .expect("node with config is an object")
                        .remove("config");
                }
                continue;
            };
            let device_id = binding.get("deviceId").and_then(Value::as_str);
            let device_name = binding.get("deviceName").and_then(Value::as_str);
            if device_id.is_none() && device_name.is_none() {
                // Any-device binding: no identity to attach to a
                // component. The node becomes unassigned (silent).
                if config.is_empty() {
                    node.as_object_mut()
                        .expect("node with config is an object")
                        .remove("config");
                }
                continue;
            }
            let channel = binding.get("channel").and_then(Value::as_u64);
            let key = (
                kind.clone(),
                device_id.unwrap_or_default().to_owned(),
                device_name.unwrap_or_default().to_owned(),
                channel.map(|c| c.to_string()).unwrap_or_default(),
            );
            let idx = *by_binding.entry(key).or_insert_with(|| {
                let mut cfg = serde_json::Map::new();
                cfg.insert(
                    "device".to_owned(),
                    serde_json::json!({
                        "id": device_id,
                        "name": device_name.unwrap_or_default(),
                    }),
                );
                if let Some(ch) = channel {
                    cfg.insert("channel".to_owned(), Value::from(ch));
                }
                if let Some(cc) = binding.get("ccRange").filter(|v| v.is_array()) {
                    cfg.insert("ccRange".to_owned(), cc.clone());
                }
                let short_kind = kind
                    .strip_prefix("source.")
                    .unwrap_or(&kind)
                    .replace('-', " ");
                let name = match device_name {
                    Some(n) => format!("{n} {short_kind}"),
                    None => format!("Migrated {short_kind}"),
                };
                components.push(serde_json::json!({
                    "id": format!("rc-{next_id}"),
                    "kind": kind.clone(),
                    "name": name,
                    "config": Value::Object(cfg),
                }));
                next_id += 1;
                components.len() - 1
            });

            // Keyboard note-range filters union into the component's
            // learned key range.
            if let Some(range) = binding.get("noteRange").and_then(Value::as_array) {
                if let (Some(lo), Some(hi)) = (
                    range.first().and_then(Value::as_u64),
                    range.get(1).and_then(Value::as_u64),
                ) {
                    let cfg = components[idx]
                        .get_mut("config")
                        .and_then(Value::as_object_mut)
                        .expect("harvested component has config");
                    let cur_lo = cfg
                        .get("lowNote")
                        .and_then(Value::as_u64)
                        .unwrap_or(u64::MAX);
                    let cur_hi = cfg.get("highNote").and_then(Value::as_u64).unwrap_or(0);
                    cfg.insert("lowNote".to_owned(), Value::from(lo.min(cur_lo)));
                    cfg.insert("highNote".to_owned(), Value::from(hi.max(cur_hi)));
                }
            }

            let component_id = components[idx]
                .get("id")
                .cloned()
                .expect("component has id");
            config.insert("rigComponentId".to_owned(), component_id);
        }
    });

    if let Some(rig) = value
        .get_mut("show")
        .and_then(|s| s.get_mut("rig"))
        .and_then(Value::as_object_mut)
    {
        rig.insert("components".to_owned(), Value::Array(components));
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
