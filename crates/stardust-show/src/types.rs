//! Core show types: rig, songs, patches, saved blocks, the show.
//!
//! Mirrors the TS shapes in `stardust-pit/src/src/screens/_seed-data.ts`
//! and adjacent component files. Field names are camelCase on the wire so
//! the TS UI can produce and consume `ShowDocument` JSON without adapters.

use serde::{Deserialize, Serialize};

use stardust_patch::{NodeKind, PatchGraph};

// -----------------------------------------------------------------------------
// ID newtypes (same pattern as stardust-patch)
// -----------------------------------------------------------------------------

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

string_id!(SongId);
string_id!(PatchId);
string_id!(BlockId);
string_id!(RigComponentId);

// -----------------------------------------------------------------------------
// Rig
// -----------------------------------------------------------------------------

/// One physical component of the user's rig — a keyboard, a pedal, a bank
/// of pads. The `kind` maps to a `source.*` `NodeKind`; `name` is the
/// user's friendly label ("Nord Stage 3 keys"). Two components of the same
/// kind is valid — someone with two keyboards has two `source.keyboard`
/// entries.
///
/// Since schema v3 the component owns hardware identity: a patch source
/// node references a component via `config.rigComponentId` and inherits
/// its device binding. Node-level `hardwareBinding` no longer exists.
///
/// Per ADR-0004 the kind-specific configuration (device binding, learned
/// key range, pad note assignments, captured CC source, …) lives in the
/// free-form `config` bag; the engine owns the strong typing. The config
/// vocabulary is documented in `docs/schemas/CHANGELOG.md`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RigComponent {
    pub id: RigComponentId,
    pub kind: NodeKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub config: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rig {
    pub components: Vec<RigComponent>,
}

impl Rig {
    pub fn find_component(&self, id: &RigComponentId) -> Option<&RigComponent> {
        self.components.iter().find(|c| &c.id == id)
    }
}

// -----------------------------------------------------------------------------
// Saved blocks (user-created composite presets)
// -----------------------------------------------------------------------------

/// A user-saved composite block, shown in the right-panel Blocks tab. v1
/// stores only metadata — the actual subgraph for re-instantiation is a
/// future revisit (see ADR-0005).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedBlock {
    pub id: BlockId,
    pub name: String,
    pub node_count: u32,
}

// -----------------------------------------------------------------------------
// Songs + patches
// -----------------------------------------------------------------------------

/// One patch within a song. Inlines its `PatchGraph` directly — see
/// ADR-0005 for the inline-vs-side-table decision. `compound` is a v1
/// placeholder for multi-part patches (verse/chorus/bridge); structural
/// support for parts is deferred.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Patch {
    pub id: PatchId,
    pub number: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub compound: bool,
    pub graph: PatchGraph,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Song {
    pub id: SongId,
    pub number: u32,
    pub name: String,
    pub patches: Vec<Patch>,
}

// -----------------------------------------------------------------------------
// The show
// -----------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Show {
    pub name: String,
    pub songs: Vec<Song>,
    pub rig: Rig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub saved_blocks: Vec<SavedBlock>,
}

impl Show {
    pub fn find_song(&self, id: &SongId) -> Option<&Song> {
        self.songs.iter().find(|s| &s.id == id)
    }

    pub fn find_patch(&self, id: &PatchId) -> Option<(&Song, &Patch)> {
        for s in &self.songs {
            if let Some(p) = s.patches.iter().find(|p| &p.id == id) {
                return Some((s, p));
            }
        }
        None
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}
