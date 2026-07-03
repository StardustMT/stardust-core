//! Schema migration tests.
//!
//! Per ADR-0003, every schema-version bump ships with a test that loads a
//! realistic prior-version document and asserts the migration produces the
//! expected current-version shape.

use stardust_patch::{CURRENT_SCHEMA_VERSION, NodeKind, PatchDocument};

/// v1 doc with the legacy `instrument.sine` kind. Pinned here so a
/// reviewer can see exactly what shape the migration is consuming.
const V1_DOC_WITH_SINE: &str = r#"{
  "kind": "stardust.patch",
  "schemaVersion": 1,
  "graph": {
    "nodes": [
      {
        "id": "n1",
        "kind": "source.keyboard",
        "name": "kbd",
        "x": 0, "y": 0,
        "ports": [
          { "id": "out", "label": "MIDI out", "signal": "midi", "direction": "out" }
        ]
      },
      {
        "id": "n2",
        "kind": "instrument.sine",
        "name": "Sine synth",
        "x": 0, "y": 0,
        "ports": [
          { "id": "midi-in", "label": "MIDI in", "signal": "midi", "direction": "in" },
          { "id": "audio-l", "label": "L", "signal": "audio", "direction": "out",
            "config": { "kind": "stereo", "channel": "L" } },
          { "id": "audio-r", "label": "R", "signal": "audio", "direction": "out",
            "config": { "kind": "stereo", "channel": "R" } }
        ],
        "config": { "polyphony": 8 }
      }
    ],
    "wires": [
      { "id": "w1", "fromNode": "n1", "fromPort": "out", "toNode": "n2", "toPort": "midi-in" }
    ],
    "composites": []
  }
}"#;

#[test]
fn v1_sine_node_migrates_to_testtone() {
    let doc = PatchDocument::from_json(V1_DOC_WITH_SINE).expect("v1 doc loads");
    assert_eq!(doc.header.schema_version, CURRENT_SCHEMA_VERSION);
    let kinds: Vec<_> = doc.graph.nodes.iter().map(|n| n.kind).collect();
    assert!(
        kinds.contains(&NodeKind::InstrumentTestTone),
        "expected migrated kind testtone, got {kinds:?}"
    );
    // Per-node config survives untouched.
    let migrated = doc
        .graph
        .nodes
        .iter()
        .find(|n| matches!(n.kind, NodeKind::InstrumentTestTone))
        .unwrap();
    assert_eq!(
        migrated.config.as_ref().and_then(|c| c.get("polyphony")),
        Some(&serde_json::json!(8))
    );
}

#[test]
fn newer_schema_is_rejected() {
    let raw = V1_DOC_WITH_SINE.replace("\"schemaVersion\": 1", "\"schemaVersion\": 999");
    let err = PatchDocument::from_json(&raw).expect_err("future schema must error");
    let msg = format!("{err}");
    assert!(msg.contains("999"), "unexpected error: {msg}");
}
