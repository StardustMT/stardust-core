//! Round-trip + validation tests against the LSOH show fixture.
//!
//! Per ADR-0005, the bar for "data model is done" is that the fixture
//! parses, validates (including every embedded patch graph), and survives
//! a parse -> serialize -> parse round-trip.

use stardust_show::ShowDocument;

fn load(name: &str) -> ShowDocument {
    let path = format!(
        "{}/tests/fixtures/{}.json",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    ShowDocument::from_json(&raw).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

fn assert_round_trip(name: &str) {
    let doc = load(name);
    doc.show
        .validate()
        .unwrap_or_else(|errs| panic!("{name} did not validate: {errs:#?}"));

    let serialized = doc.to_json().expect("serialize");
    let reparsed =
        ShowDocument::from_json(&serialized).unwrap_or_else(|e| panic!("re-parse {name}: {e}"));

    assert_eq!(doc, reparsed, "round-trip changed the document for {name}");
}

#[test]
fn lsoh_round_trips() {
    assert_round_trip("lsoh");
}

#[test]
fn header_is_well_formed() {
    let doc = load("lsoh");
    assert_eq!(doc.header.kind, "stardust.show");
    assert_eq!(doc.header.schema_version, 3);
}

#[test]
fn show_contents_parse_correctly() {
    let doc = load("lsoh");
    assert_eq!(doc.show.name, "Little Shop of Horrors");
    assert_eq!(doc.show.songs.len(), 2);
    assert_eq!(doc.show.songs[0].patches.len(), 2);
    assert_eq!(doc.show.rig.components.len(), 5);
    // Embedded graph is reachable.
    assert_eq!(doc.show.songs[0].patches[0].graph.nodes.len(), 3);
}

#[test]
fn find_patch_locates_by_id() {
    let doc = load("lsoh");
    let id = "p1.2".into();
    let (song, patch) = doc.show.find_patch(&id).expect("patch exists");
    assert_eq!(song.name, "Prologue");
    assert_eq!(patch.name, "Underscoring");
}
