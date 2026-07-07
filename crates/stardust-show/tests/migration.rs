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
    // The v1 label-only rig source became an unbound v3 component.
    assert_eq!(doc.show.rig.components.len(), 1);
    assert_eq!(doc.show.rig.components[0].name, "kbd");
}

/// v2 show exercising every v2→v3 rule: a label-only rig source, two
/// keyboard nodes (across two patches) bound to the same device+channel
/// with different note ranges, a sustain pedal on the same device, and an
/// any-device binding that must dissolve into an unassigned node.
const V2_SHOW_WITH_BINDINGS: &str = r#"{
  "kind": "stardust.show",
  "schemaVersion": 2,
  "show": {
    "name": "Binding migration test",
    "rig": { "sources": [{ "kind": "source.pads", "label": "Old pads entry" }] },
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
                  "id": "kbd1",
                  "kind": "source.keyboard",
                  "name": "Keys low",
                  "x": 0, "y": 0,
                  "ports": [{ "id": "out", "label": "MIDI out", "signal": "midi", "direction": "out" }],
                  "config": {
                    "hardwareBinding": {
                      "deviceId": "dev-1", "deviceName": "Test Keys",
                      "channel": 1, "noteRange": [21, 60]
                    }
                  }
                },
                {
                  "id": "sus1",
                  "kind": "source.sustain-pedal",
                  "name": "Sustain",
                  "x": 0, "y": 0,
                  "ports": [{ "id": "out", "label": "MIDI out", "signal": "midi", "direction": "out" }],
                  "config": {
                    "hardwareBinding": { "deviceId": "dev-1", "deviceName": "Test Keys", "channel": 1 }
                  }
                },
                {
                  "id": "omni1",
                  "kind": "source.keyboard",
                  "name": "Any device keys",
                  "x": 0, "y": 0,
                  "ports": [{ "id": "out", "label": "MIDI out", "signal": "midi", "direction": "out" }],
                  "config": { "hardwareBinding": { "deviceId": null } }
                }
              ],
              "wires": [],
              "composites": []
            }
          },
          {
            "id": "p2",
            "number": 2,
            "name": "Patch 2",
            "graph": {
              "nodes": [
                {
                  "id": "kbd2",
                  "kind": "source.keyboard",
                  "name": "Keys high",
                  "x": 0, "y": 0,
                  "ports": [{ "id": "out", "label": "MIDI out", "signal": "midi", "direction": "out" }],
                  "config": {
                    "hardwareBinding": {
                      "deviceId": "dev-1", "deviceName": "Test Keys",
                      "channel": 1, "noteRange": [61, 108]
                    }
                  }
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
fn v2_bindings_migrate_to_rig_components() {
    let doc = ShowDocument::from_json(V2_SHOW_WITH_BINDINGS).expect("v2 show loads");
    assert_eq!(doc.header.schema_version, CURRENT_SCHEMA_VERSION);

    // Components: the label-only source + one keyboard (shared by kbd1 +
    // kbd2, same device+channel) + one sustain pedal. The any-device blob
    // creates nothing.
    let rig = &doc.show.rig;
    assert_eq!(rig.components.len(), 3, "components: {:#?}", rig.components);
    assert_eq!(rig.components[0].name, "Old pads entry");
    assert!(
        rig.components[0].config.is_none(),
        "label-only source stays unbound"
    );

    let kbd = rig
        .components
        .iter()
        .find(|c| c.kind == NodeKind::SourceKeyboard)
        .expect("keyboard component created");
    let cfg = kbd.config.as_ref().expect("keyboard component has config");
    assert_eq!(cfg["device"]["id"], "dev-1");
    assert_eq!(cfg["device"]["name"], "Test Keys");
    assert_eq!(cfg["channel"], 1);
    // Note ranges across the two nodes unioned into the learned key range.
    assert_eq!(cfg["lowNote"], 21);
    assert_eq!(cfg["highNote"], 108);

    // Both keyboard nodes point at the same component; the binding blob
    // is gone everywhere.
    let node_cfg = |patch: usize, node: usize| {
        doc.show.songs[0].patches[patch].graph.nodes[node]
            .config
            .clone()
    };
    let kbd1 = node_cfg(0, 0).expect("kbd1 keeps config");
    let kbd2 = node_cfg(1, 0).expect("kbd2 keeps config");
    assert_eq!(kbd1["rigComponentId"], kbd2["rigComponentId"]);
    assert_eq!(kbd1["rigComponentId"].as_str(), Some(kbd.id.as_str()));
    assert!(kbd1.get("hardwareBinding").is_none());

    // The any-device node lost its blob and got no component: unassigned.
    assert_eq!(node_cfg(0, 2), None);

    // The migrated document round-trips and validates.
    doc.show.validate().expect("migrated show validates");
    let reparsed = ShowDocument::from_json(&doc.to_json().unwrap()).unwrap();
    assert_eq!(doc, reparsed);
}
