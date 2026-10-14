//! Host tests of the pure half of the credential seam: the frozen entry-name rule, decoding of
//! the Kotlin `read` answer, and the localized messages. The Kotlin store itself (Keystore key,
//! encryption, persistence across restarts, unreadable entries) can only be checked on a device.

use super::{CredentialError, decode_read_answer, entry_name};
use crate::test_catalog::{assert_key_in_shipped_catalogs, install_english};
use crate::{PlatformFeature, Unavailable, UnavailableReason};

fn answer(fields: &[&str]) -> Vec<String> {
    fields.iter().map(|field| (*field).to_owned()).collect()
}

// Persistence contract: changing the rule orphans every secret stored on a device.
#[test]
fn entry_name_is_frozen() {
    assert_eq!(entry_name("ManhwaStudio Hugging Face", "default"), "v1.4d616e68776153747564696f2048756767696e672046616365.64656661756c74");
    assert_eq!(
        entry_name("ManhwaStudio AI API OCR", "openai_compatible@http://127.0.0.1:8080/v1/"),
        "v1.4d616e68776153747564696f20414920415049204f4352.6f70656e61695f636f6d70617469626c6540687474703a2f2f3132372e302e302e313a383038302f76312f"
    );
    assert_eq!(entry_name("é", ""), "v1.c3a9.");
}

#[test]
fn entry_names_of_different_pairs_never_collide() {
    // The same concatenation split differently must give different names.
    assert_ne!(entry_name("a", "bc"), entry_name("ab", "c"));
    assert_ne!(entry_name("a.", "b"), entry_name("a", ".b"));
    assert_ne!(entry_name("", "x"), entry_name("x", ""));
    // Only lowercase hex, the version prefix and the one separator ever appear.
    let name = entry_name("Ünïcode Service: 1", "image_edit:dashscope@intl");
    let body = name.strip_prefix("v1.").unwrap_or_else(|| panic!("missing prefix in {name}"));
    assert_eq!(body.matches('.').count(), 1, "{name}");
    assert!(body.chars().all(|ch| ch == '.' || ch.is_ascii_digit() || ('a'..='f').contains(&ch)), "{name}");
}

#[test]
fn read_answer_ok_and_missing() {
    assert_eq!(decode_read_answer(answer(&["ok", "sk-secret"])), Ok(Some("sk-secret".to_owned())));
    // An empty stored secret is still "stored" (callers decide what empty means).
    assert_eq!(decode_read_answer(answer(&["ok", ""])), Ok(Some(String::new())));
    assert_eq!(decode_read_answer(answer(&["missing"])), Ok(None));
}

#[test]
fn read_answer_unreadable_is_never_missing() {
    assert_eq!(
        decode_read_answer(answer(&["unreadable", "AEADBadTagException"])),
        Err(CredentialError::Unreadable { detail: "AEADBadTagException".to_owned() })
    );
    assert_eq!(decode_read_answer(answer(&["failed", "KeyStoreException: busy"])), Err(CredentialError::Backend { detail: "KeyStoreException: busy".to_owned() }));
}

#[test]
fn malformed_read_answers_never_echo_a_field() {
    for fields in [&[][..], &["ok"][..], &["missing", "sk-secret"][..], &["ok", "sk-secret", "x"][..], &["sk-secret", "sk-secret"][..]] {
        match decode_read_answer(answer(fields)) {
            Err(CredentialError::Backend { detail }) => assert!(!detail.contains("sk-secret"), "{fields:?}: {detail}"),
            other => panic!("{fields:?} decoded as {other:?}"),
        }
    }
}

#[test]
fn messages_are_localized_and_carry_the_detail() {
    install_english();
    for key in ["platform.credentials.unreadable_error", "platform.credentials.backend_error"] {
        assert_key_in_shipped_catalogs(key);
    }
    let unreadable = CredentialError::Unreadable { detail: "AEADBadTagException".to_owned() }.to_string();
    assert!(unreadable.contains("AEADBadTagException"), "{unreadable}");
    let backend = CredentialError::Backend { detail: "commit failed".to_owned() }.to_string();
    assert!(backend.contains("commit failed"), "{backend}");
    let unavailable = Unavailable::new(PlatformFeature::CredentialStore, UnavailableReason::BridgeMissing);
    assert_eq!(CredentialError::Unavailable(unavailable).to_string(), unavailable.user_message());
}
