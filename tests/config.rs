//! Tests for `finguard_rs::config`.

mod common;

use common::TestEnv;
use finguard_rs::Error;
use finguard_rs::config::{
    add_known_category, add_mapping, clear_all_mappings, get_all_mappings, get_known_categories,
    get_mapping, remove_known_category, remove_mapping,
};

// ------------------------------------------------------------------
// Mappings
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn add_and_get_mapping_normalizes() {
    let _env = TestEnv::new();
    // Mixed-case / padded inputs are trimmed + lower-cased.
    add_mapping("  PAM  ", " Groceries ", " SuperMarket ", false).unwrap();

    // Lookup is also normalized.
    let m = get_mapping("pam").unwrap().unwrap();
    assert_eq!(m.primary_category, "groceries");
    assert_eq!(m.secondary_category, "supermarket");

    // Different casing of the key resolves to the same entry.
    assert_eq!(get_mapping("  pAm ").unwrap().unwrap(), m);
}

#[test]
#[serial_test::serial]
fn get_missing_mapping_is_none() {
    let _env = TestEnv::new();
    assert!(get_mapping("nope").unwrap().is_none());
}

#[test]
#[serial_test::serial]
fn add_mapping_no_overwrite_errors_when_exists() {
    let _env = TestEnv::new();
    add_mapping("pam", "groceries", "", false).unwrap();
    let err = add_mapping("pam", "housing", "", false).unwrap_err();
    assert!(matches!(err, Error::AlreadyExists(_)));
    // Original value is preserved.
    assert_eq!(
        get_mapping("pam").unwrap().unwrap().primary_category,
        "groceries"
    );
}

#[test]
#[serial_test::serial]
fn add_mapping_overwrite_replaces() {
    let _env = TestEnv::new();
    add_mapping("pam", "groceries", "a", false).unwrap();
    add_mapping("pam", "housing", "b", true).unwrap();
    let m = get_mapping("pam").unwrap().unwrap();
    assert_eq!(m.primary_category, "housing");
    assert_eq!(m.secondary_category, "b");
}

#[test]
#[serial_test::serial]
fn remove_mapping_returns_entry() {
    let _env = TestEnv::new();
    add_mapping("pam", "groceries", "supermarket", false).unwrap();
    let removed = remove_mapping("PAM").unwrap();
    assert_eq!(removed.primary_category, "groceries");
    assert_eq!(removed.secondary_category, "supermarket");
    assert!(get_mapping("pam").unwrap().is_none());
}

#[test]
#[serial_test::serial]
fn remove_missing_mapping_is_not_found() {
    let _env = TestEnv::new();
    let err = remove_mapping("nope").unwrap_err();
    assert!(matches!(err, Error::NotFound(_)));
}

#[test]
#[serial_test::serial]
fn mappings_preserve_insertion_order() {
    let _env = TestEnv::new();
    add_mapping("zeta", "a", "", false).unwrap();
    add_mapping("alpha", "b", "", false).unwrap();
    add_mapping("mid", "c", "", false).unwrap();

    let all = get_all_mappings().unwrap();
    let keys: Vec<&String> = all.keys().collect();
    assert_eq!(keys, vec!["zeta", "alpha", "mid"]);
}

#[test]
#[serial_test::serial]
fn clear_all_mappings_empties() {
    let _env = TestEnv::new();
    add_mapping("pam", "groceries", "", false).unwrap();
    clear_all_mappings().unwrap();
    assert!(get_all_mappings().unwrap().is_empty());
}

// ------------------------------------------------------------------
// JSON byte-compatibility
// ------------------------------------------------------------------

/// Build the expected JSON text via python3's `json.dump(indent=4,
/// ensure_ascii=False)` to cross-validate byte-for-byte.
fn python_json(py_dict_literal: &str) -> String {
    let code = format!(
        "import json,sys; sys.stdout.write(json.dumps({py_dict_literal}, indent=4, ensure_ascii=False))"
    );
    let out = std::process::Command::new("python3")
        .arg("-c")
        .arg(code)
        .output()
        .expect("run python3");
    assert!(
        out.status.success(),
        "python3 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
#[serial_test::serial]
fn mapping_file_is_python_json_byte_compatible() {
    let env = TestEnv::new();
    add_mapping("pam", "groceries", "supermarket", false).unwrap();
    add_mapping("nowtv", "housing", "tv", false).unwrap();

    let path = env.root().join("finguard").join("category_mappings.json");
    let on_disk = std::fs::read_to_string(&path).unwrap();

    // 4-space indent, insertion order preserved, no trailing newline.
    let expected = python_json(
        "{\"pam\": {\"primary_category\": \"groceries\", \"secondary_category\": \"supermarket\"}, \
         \"nowtv\": {\"primary_category\": \"housing\", \"secondary_category\": \"tv\"}}",
    );
    assert_eq!(on_disk, expected);
    assert!(!on_disk.ends_with('\n'), "no trailing newline");
}

#[test]
#[serial_test::serial]
fn mapping_file_non_ascii_unescaped() {
    let env = TestEnv::new();
    // Non-ASCII must be emitted verbatim (ensure_ascii=False).
    add_mapping("caffè", "caffè", "café", false).unwrap();

    let path = env.root().join("finguard").join("category_mappings.json");
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("caffè"), "non-ASCII key kept verbatim");
    assert!(!on_disk.contains("\\u"), "no \\u escapes");

    let expected = python_json(
        "{\"caffè\": {\"primary_category\": \"caffè\", \"secondary_category\": \"café\"}}",
    );
    assert_eq!(on_disk, expected);
}

#[test]
#[serial_test::serial]
fn mapping_file_hand_written_literal() {
    // Independent of python3: explicit expected bytes.
    let env = TestEnv::new();
    add_mapping("pam", "groceries", "supermarket", false).unwrap();

    let path = env.root().join("finguard").join("category_mappings.json");
    let on_disk = std::fs::read_to_string(&path).unwrap();
    let expected = "{\n    \"pam\": {\n        \"primary_category\": \"groceries\",\n        \"secondary_category\": \"supermarket\"\n    }\n}";
    assert_eq!(on_disk, expected);
}

// ------------------------------------------------------------------
// Known categories
// ------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn add_known_categories_sorts_and_separates_kinds() {
    let _env = TestEnv::new();
    add_known_category("Zoo", "primary").unwrap();
    add_known_category("Apple", "primary").unwrap();
    add_known_category("Mango", "primary").unwrap();
    add_known_category("Bus", "secondary").unwrap();

    let known = get_known_categories().unwrap();
    assert_eq!(known.primary, vec!["Apple", "Mango", "Zoo"]);
    assert_eq!(known.secondary, vec!["Bus"]);
}

#[test]
#[serial_test::serial]
fn add_known_category_invalid_kind() {
    let _env = TestEnv::new();
    let err = add_known_category("X", "tertiary").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)));
}

#[test]
#[serial_test::serial]
fn add_known_category_duplicate_errors() {
    let _env = TestEnv::new();
    add_known_category("Apple", "primary").unwrap();
    let err = add_known_category("Apple", "primary").unwrap_err();
    assert!(matches!(err, Error::AlreadyExists(_)));
}

#[test]
#[serial_test::serial]
fn remove_known_category_works() {
    let _env = TestEnv::new();
    add_known_category("Apple", "primary").unwrap();
    add_known_category("Mango", "primary").unwrap();
    remove_known_category("Apple", "primary").unwrap();
    assert_eq!(get_known_categories().unwrap().primary, vec!["Mango"]);
}

#[test]
#[serial_test::serial]
fn remove_known_category_missing_is_not_found() {
    let _env = TestEnv::new();
    let err = remove_known_category("Nope", "primary").unwrap_err();
    assert!(matches!(err, Error::NotFound(_)));
}

#[test]
#[serial_test::serial]
fn remove_known_category_invalid_kind() {
    let _env = TestEnv::new();
    let err = remove_known_category("X", "bad").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)));
}
