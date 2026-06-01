//! Negative tests: validation must catch the structural problems it claims to.

use stardust_show::{ShowDocument, ShowValidationError};

const BASE: &str = include_str!("fixtures/lsoh.json");

fn mutate(f: impl FnOnce(&mut serde_json::Value)) -> Vec<ShowValidationError> {
    let mut v: serde_json::Value = serde_json::from_str(BASE).unwrap();
    f(&mut v);
    let s = v.to_string();
    let doc: ShowDocument = ShowDocument::from_json(&s).expect("parses");
    doc.show.validate().expect_err("expected validation errors")
}

#[test]
fn detects_duplicate_song_id() {
    let errs = mutate(|v| {
        v["show"]["songs"][1]["id"] = "s1".into();
    });
    assert!(
        errs.iter()
            .any(|e| matches!(e, ShowValidationError::DuplicateSongId(_))),
        "got: {errs:#?}"
    );
}

#[test]
fn detects_duplicate_patch_id_across_songs() {
    // Patch ids must be unique show-wide, not just within one song.
    let errs = mutate(|v| {
        v["show"]["songs"][1]["patches"][0]["id"] = "p1.1".into();
    });
    assert!(
        errs.iter()
            .any(|e| matches!(e, ShowValidationError::DuplicatePatchId(_))),
        "got: {errs:#?}"
    );
}

#[test]
fn detects_duplicate_block_id() {
    let errs = mutate(|v| {
        v["show"]["savedBlocks"] = serde_json::json!([
            { "id": "b1", "name": "A", "nodeCount": 2 },
            { "id": "b1", "name": "B", "nodeCount": 3 }
        ]);
    });
    assert!(
        errs.iter()
            .any(|e| matches!(e, ShowValidationError::DuplicateBlockId(_))),
        "got: {errs:#?}"
    );
}

#[test]
fn embedded_graph_errors_are_wrapped_with_patch_context() {
    let errs = mutate(|v| {
        v["show"]["songs"][0]["patches"][0]["graph"]["wires"][0]["toNode"] = "nope".into();
    });
    let wrapped = errs.iter().find_map(|e| match e {
        ShowValidationError::PatchInvalid {
            song,
            patch,
            errors,
        } => Some((song.clone(), patch.clone(), errors.clone())),
        _ => None,
    });
    let (song, patch, graph_errs) = wrapped.expect("expected a PatchInvalid error");
    assert_eq!(song.as_str(), "s1");
    assert_eq!(patch.as_str(), "p1.1");
    assert!(
        !graph_errs.is_empty(),
        "wrapped graph errors should not be empty"
    );
}

#[test]
fn rejects_wrong_kind() {
    let mut v: serde_json::Value = serde_json::from_str(BASE).unwrap();
    v["kind"] = "stardust.patch".into();
    let err = ShowDocument::from_json(&v.to_string()).expect_err("wrong kind should fail to load");
    let msg = err.to_string();
    assert!(msg.contains("stardust.show"), "got: {msg}");
}

#[test]
fn rejects_newer_schema_version() {
    let mut v: serde_json::Value = serde_json::from_str(BASE).unwrap();
    v["schemaVersion"] = 99.into();
    let err =
        ShowDocument::from_json(&v.to_string()).expect_err("newer schema should fail to load");
    let msg = err.to_string();
    assert!(msg.contains("v99"), "got: {msg}");
}
