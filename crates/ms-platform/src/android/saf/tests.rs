/*
File: android/saf/tests.rs

Purpose:
Host tests of the pure SAF bridge half: wire encoding (kind codes, result and tree-listing
decoding), staging name rules and the pending-request registry.
*/

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{
    BridgeOutcome, PendingRegistry, PickedDocument, PlannedCopy, STATUS_CANCELLED, STATUS_FAILED, STATUS_PICKED, TreeEntry, decode_outcome,
    create_document_mime, decode_tree_listing, extension_of, kind_code, name_with_default_extension, plan_file_copies, plan_tree_copy, sanitize_file_name,
    staging_file_name, unique_name,
};
use crate::file_dialog::DialogKind;

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn register_then_resolve_once() {
    let registry: PendingRegistry<&str> = PendingRegistry::new();
    let first = registry.register("a");
    let second = registry.register("b");
    assert_ne!(first, second);
    assert_ne!(first, 0);
    assert!(registry.contains(second));
    assert_eq!(registry.take(second), Some("b"));
    assert!(!registry.contains(second));
    assert_eq!(registry.take(second), None);
    assert_eq!(registry.len(), 1);
    assert_eq!(registry.take(first), Some("a"));
    assert_eq!(registry.len(), 0);
}

#[test]
fn late_result_after_withdrawal_is_discarded() {
    let registry: PendingRegistry<std::sync::mpsc::Sender<u32>> = PendingRegistry::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    let id = registry.register(sender);
    // The requester goes away: its drop guard withdraws the slot, closing the channel.
    drop(registry.take(id));
    assert!(receiver.recv().is_err());
    // The bridge reports later: nothing is registered under the id any more.
    assert!(registry.take(id).is_none());
}

#[test]
fn kind_codes_follow_the_declaration_order() {
    assert_eq!(kind_code(DialogKind::OpenFile), Some(0));
    assert_eq!(kind_code(DialogKind::OpenFiles), Some(1));
    assert_eq!(kind_code(DialogKind::OpenFolder), Some(2));
    assert_eq!(kind_code(DialogKind::SaveFile), Some(3));
    assert_eq!(kind_code(DialogKind::SaveIntoFolder), Some(4));
    assert_eq!(kind_code(DialogKind::PickDirectoryPath), None);
}

#[test]
fn outcome_decoding() {
    assert_eq!(
        decode_outcome(STATUS_PICKED, strings(&["content://a", "content://b"]), strings(&["a.png", "b.png"]), None),
        BridgeOutcome::Picked(vec![
            PickedDocument { uri: "content://a".to_owned(), name: "a.png".to_owned() },
            PickedDocument { uri: "content://b".to_owned(), name: "b.png".to_owned() },
        ])
    );
    assert_eq!(decode_outcome(STATUS_CANCELLED, Vec::new(), Vec::new(), None), BridgeOutcome::Cancelled);
    assert_eq!(decode_outcome(STATUS_FAILED, Vec::new(), Vec::new(), Some("boom".to_owned())), BridgeOutcome::Failed("boom".to_owned()));
    assert!(matches!(decode_outcome(STATUS_FAILED, Vec::new(), Vec::new(), None), BridgeOutcome::Failed(_)));
    // Malformed picked reports and unknown statuses never read as a pick.
    assert!(matches!(decode_outcome(STATUS_PICKED, Vec::new(), Vec::new(), None), BridgeOutcome::Failed(_)));
    assert!(matches!(decode_outcome(STATUS_PICKED, strings(&["content://a"]), Vec::new(), None), BridgeOutcome::Failed(_)));
    assert!(matches!(decode_outcome(7, strings(&["content://a"]), strings(&["a"]), None), BridgeOutcome::Failed(_)));
}

#[test]
fn tree_listing_decoding_and_plan() {
    let raw = strings(&[
        "ok", "d", "-1", "pages", "u:pages", "f", "0", "01.png", "u:01", "f", "0", "01.png", "u:01b", "f", "-1", "../evil", "u:evil", "d", "0",
        "", "u:empty",
    ]);
    let entries = decode_tree_listing(raw).unwrap_or_else(|err| panic!("valid listing rejected: {err}"));
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[1], TreeEntry { is_dir: false, parent: Some(0), name: "01.png".to_owned(), uri: "u:01".to_owned() });
    let plan = plan_tree_copy(&entries);
    let paths: Vec<(PathBuf, bool)> = plan.iter().map(|step| (step.relative.clone(), step.is_dir)).collect();
    assert_eq!(
        paths,
        vec![
            (PathBuf::from("pages"), true),
            (PathBuf::from("pages/01.png"), false),
            (PathBuf::from("pages/01 (2).png"), false),
            (PathBuf::from(".._evil"), false),
            (PathBuf::from("pages/unnamed"), true),
        ]
    );
    assert_eq!(plan[2], PlannedCopy { relative: PathBuf::from("pages/01 (2).png"), is_dir: false, uri: "u:01b".to_owned() });
}

#[test]
fn tree_listing_rejects_malformed_input() {
    assert_eq!(decode_tree_listing(strings(&["err", "denied"])), Err("denied".to_owned()));
    assert!(decode_tree_listing(strings(&["err"])).is_err());
    assert!(decode_tree_listing(Vec::new()).is_err());
    assert!(decode_tree_listing(strings(&["ok", "f", "-1", "a"])).is_err(), "truncated entry");
    assert!(decode_tree_listing(strings(&["ok", "x", "-1", "a", "u"])).is_err(), "unknown kind");
    assert!(decode_tree_listing(strings(&["ok", "f", "0", "a", "u"])).is_err(), "parent is itself");
    assert!(decode_tree_listing(strings(&["ok", "f", "-1", "a", "u", "f", "0", "b", "u"])).is_err(), "parent is a file");
    assert!(decode_tree_listing(strings(&["ok", "d", "x", "a", "u"])).is_err(), "parent not a number");
    assert_eq!(decode_tree_listing(strings(&["ok"])), Ok(Vec::new()), "an empty tree is valid");
}

#[test]
fn sanitizing_keeps_names_inside_their_directory() {
    assert_eq!(sanitize_file_name("page 01.png"), "page 01.png");
    assert_eq!(sanitize_file_name("a/b\\c"), "a_b_c");
    assert_eq!(sanitize_file_name("  .. "), "unnamed");
    assert_eq!(sanitize_file_name("."), "unnamed");
    assert_eq!(sanitize_file_name(""), "unnamed");
    assert_eq!(sanitize_file_name("x\0y\nz"), "x_y_z");
    assert_eq!(sanitize_file_name("Глава 1.psd"), "Глава 1.psd");
    // Over-long names are cut at a char boundary and keep a short extension.
    let long = format!("{}.png", "я".repeat(200));
    let cut = sanitize_file_name(&long);
    assert!(cut.len() <= 255, "{} bytes", cut.len());
    assert_eq!(extension_of(&cut), Some("png"));
    let no_extension = "ж".repeat(300);
    assert!(sanitize_file_name(&no_extension).len() <= 255);
}

#[test]
fn unique_names_and_copy_plans() {
    let mut taken = BTreeSet::new();
    assert_eq!(unique_name(&mut taken, "a.png"), "a.png");
    assert_eq!(unique_name(&mut taken, "a.png"), "a (2).png");
    assert_eq!(unique_name(&mut taken, "a.png"), "a (3).png");
    assert_eq!(unique_name(&mut taken, ".hidden"), ".hidden");
    assert_eq!(unique_name(&mut taken, ".hidden"), ".hidden (2)");
    let documents = vec![
        PickedDocument { uri: "u1".to_owned(), name: "x.jpg".to_owned() },
        PickedDocument { uri: "u2".to_owned(), name: "x.jpg".to_owned() },
        PickedDocument { uri: "u3".to_owned(), name: "dir/y.jpg".to_owned() },
    ];
    assert_eq!(plan_file_copies(&documents), strings(&["x.jpg", "x (2).jpg", "dir_y.jpg"]));
}

#[test]
fn create_document_names_keep_or_gain_the_extension() {
    assert_eq!(name_with_default_extension("chapter", Some("mschapter")), "chapter.mschapter");
    assert_eq!(name_with_default_extension("chapter.zip", Some("mschapter")), "chapter.zip");
    assert_eq!(name_with_default_extension("", Some("pdf")), "");
    assert_eq!(name_with_default_extension("notes", None), "notes");
    assert_eq!(extension_of("a.tar.gz"), Some("gz"));
    assert_eq!(extension_of(".bashrc"), None);
    assert_eq!(extension_of("trailing."), None);
}

#[test]
fn staging_names_preserve_the_written_format() {
    // The provider's name wins when it has an extension (renamed or de-duplicated by the picker).
    assert_eq!(staging_file_name("export (1).pdf", Some("pdf")), "export (1).pdf");
    assert_eq!(staging_file_name("export.png", Some("pdf")), "export.png");
    // A name without one gets the requested extension, so callers pick the right format.
    assert_eq!(staging_file_name("export", Some("pdf")), "export.pdf");
    assert_eq!(staging_file_name("", Some("mschapter")), "unnamed.mschapter");
    assert_eq!(staging_file_name("log/out", None), "log_out");
}

#[test]
fn created_documents_get_a_specific_type_or_octet_stream() {
    assert_eq!(create_document_mime(Some("pdf")), "application/pdf");
    assert_eq!(create_document_mime(Some("png")), "image/png");
    assert_eq!(create_document_mime(Some("mschapter")), "application/octet-stream");
    assert_eq!(create_document_mime(None), "application/octet-stream");
}
