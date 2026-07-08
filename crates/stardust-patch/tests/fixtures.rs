//! Round-trip + validation tests against the four hand-built fixtures
//! ported from `stardust-pit/src/src/screens/_seed-data.ts`.
//!
//! Per ADR-0004, the bar for "data model is done" is that these fixtures
//! parse, validate, and survive a parse -> serialize -> parse round-trip.

use stardust_patch::PatchDocument;

fn load(name: &str) -> PatchDocument {
    let path = format!(
        "{}/tests/fixtures/{}.json",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    PatchDocument::from_json(&raw).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

fn assert_round_trip(name: &str) {
    let doc = load(name);
    doc.graph
        .validate()
        .unwrap_or_else(|errs| panic!("{name} did not validate: {errs:#?}"));

    let serialized = doc.to_json().expect("serialize");
    let reparsed =
        PatchDocument::from_json(&serialized).unwrap_or_else(|e| panic!("re-parse {name}: {e}"));

    assert_eq!(doc, reparsed, "round-trip changed the document for {name}");
}

#[test]
fn casual_round_trips() {
    assert_round_trip("casual");
}

#[test]
fn split_transpose_round_trips() {
    assert_round_trip("split-transpose");
}

#[test]
fn piano_with_sends_round_trips() {
    assert_round_trip("piano-with-sends");
}

#[test]
fn composite_block_round_trips() {
    assert_round_trip("composite-block");
}

#[test]
fn header_is_well_formed() {
    let doc = load("casual");
    assert_eq!(doc.header.kind, "stardust.patch");
    assert_eq!(doc.header.schema_version, 3);
}
