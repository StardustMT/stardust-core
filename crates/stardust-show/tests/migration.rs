//! Schema migration tests for `stardust.show` documents.
//!
//! Per ADR-0003, every schema-version bump ships with a test that loads a
//! realistic prior-version document and asserts the migration produces the
//! expected current-version shape. Show docs embed patch graphs, so this
//! also exercises that the embedded migration runs on each patch.

use stardust_patch::NodeKind;
use stardust_show::{CURRENT_SCHEMA_VERSION, ShowDocument};

const V1_SHOW_WITH_SINE: &str = r#"{
  "kind": "stardust.show",
  "schemaVersion": 1,
  "show": {
    "name": "Migration smoke test",
    "rig": { "sources": [{ "kind": "source.keyboard", "label": "kbd" }] },
    "songs": [
      {
        "id": "s1",
        "number": 1,
        "name": "Song 1",
        "patches": [
          {
            "id": "p1",
            "number": 1,
            "name": "Patch 1",
            "graph": {
              "nodes": [
                {
                  "id": "n1",
                  "kind": "instrument.sine",
                  "name": "Sine",
                  "x": 0, "y": 0,
                  "ports": [
                    { "id": "audio-l", "label": "L", "signal": "audio", "direction": "out",
                      "config": { "kind": "stereo", "channel": "L" } },
                    { "id": "audio-r", "label": "R", "signal": "audio", "direction": "out",
                      "config": { "kind": "stereo", "channel": "R" } }
                  ]
                }
              ],
              "wires": [],
              "composites": []
            }
          }
        ]
      }
    ]
  }
}"#;

#[test]
fn v1_show_embedded_sine_nodes_migrate_to_testtone() {
    let doc = ShowDocument::from_json(V1_SHOW_WITH_SINE).expect("v1 show loads");
    assert_eq!(doc.header.schema_version, CURRENT_SCHEMA_VERSION);
    let kind = doc.show.songs[0].patches[0].graph.nodes[0].kind;
    assert_eq!(kind, NodeKind::InstrumentTestTone);
}
