//! Structural validation for a `Show`.
//!
//! Per ADR-0004's collect-all philosophy (carried into ADR-0005), validation
//! gathers every error rather than fail-fast so the UI can show every
//! problem with a show at once. Embedded `PatchGraph` validation is
//! delegated to `stardust_patch`; its errors are wrapped with patch context
//! so the UI can render "Patch X has 3 problems" rather than an unattributed
//! list.

use std::collections::HashSet;

use serde::Serialize;
use thiserror::Error;

use stardust_patch::ValidationError as GraphError;

use crate::types::{BlockId, PatchId, RigComponentId, Show, SongId};

#[derive(Clone, Debug, PartialEq, Error, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ShowValidationError {
    #[error("duplicate song id {0:?}")]
    DuplicateSongId(SongId),

    #[error("duplicate patch id {0:?}")]
    DuplicatePatchId(PatchId),

    #[error("duplicate saved-block id {0:?}")]
    DuplicateBlockId(BlockId),

    #[error("duplicate rig-component id {0:?}")]
    DuplicateRigComponentId(RigComponentId),

    #[error("patch {patch:?} (in song {song:?}) has structural errors")]
    PatchInvalid {
        song: SongId,
        patch: PatchId,
        errors: Vec<GraphError>,
    },
}

impl Show {
    /// Walks the show + every embedded patch graph and returns the full set
    /// of structural problems. Empty `Ok` means the show is structurally
    /// sound; the engine still owns "is this runnable" semantics.
    pub fn validate(&self) -> Result<(), Vec<ShowValidationError>> {
        let mut errors = Vec::new();

        let mut seen_song = HashSet::new();
        for song in &self.songs {
            if !seen_song.insert(&song.id) {
                errors.push(ShowValidationError::DuplicateSongId(song.id.clone()));
            }
        }

        // Patch ids must be unique show-wide, not just within one song.
        let mut seen_patch = HashSet::new();
        for song in &self.songs {
            for patch in &song.patches {
                if !seen_patch.insert(&patch.id) {
                    errors.push(ShowValidationError::DuplicatePatchId(patch.id.clone()));
                }
                if let Err(graph_errs) = patch.graph.validate() {
                    errors.push(ShowValidationError::PatchInvalid {
                        song: song.id.clone(),
                        patch: patch.id.clone(),
                        errors: graph_errs,
                    });
                }
            }
        }

        let mut seen_component = HashSet::new();
        for component in &self.rig.components {
            if !seen_component.insert(&component.id) {
                errors.push(ShowValidationError::DuplicateRigComponentId(
                    component.id.clone(),
                ));
            }
        }

        // A dangling `rigComponentId` on a source node is deliberately NOT
        // a validation error: deleting a component must never make a show
        // unloadable. The node behaves as unassigned (silent + flagged).

        let mut seen_block = HashSet::new();
        for block in &self.saved_blocks {
            if !seen_block.insert(&block.id) {
                errors.push(ShowValidationError::DuplicateBlockId(block.id.clone()));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
