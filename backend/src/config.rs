//! Manage expense-name → category mappings, and the set of known categories,
//! stored in JSON config files.
//!
//! The config files live in `$XDG_CONFIG_HOME/finguard/` (defaulting to
//! `$HOME/.config/finguard/` when `XDG_CONFIG_HOME` is not set):
//!
//! - `category_mappings.json` — object keyed by lower-cased expense name →
//!   `{ "primary_category": str, "secondary_category": str }`.
//! - `known_categories.json` — `{ "primary": [...], "secondary": [...] }`.
//!
//! Both files are written with `serde_json`'s pretty printer (4-space indent),
//! matching Python's `json.dump(..., indent=4, ensure_ascii=False)`. Mapping
//! keys are stored in insertion order (via [`IndexMap`]) so round-trips do not
//! reorder them, exactly like a Python `dict`.
//!
//! Path resolution reads the environment at call time (it is *not* cached) so
//! that tests can override `XDG_CONFIG_HOME` / `HOME` between invocations.

use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const CONFIG_DIR_NAME: &str = "finguard";
const CONFIG_FILE_NAME: &str = "category_mappings.json";
const CATEGORIES_FILE_NAME: &str = "known_categories.json";

/// A category pair associated with an expense name.
///
/// The serde field names match the on-disk JSON keys exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryMapping {
    /// Primary category string (stored stripped + lower-cased).
    pub primary_category: String,
    /// Secondary category string (stored stripped + lower-cased).
    pub secondary_category: String,
}

/// The set of manually registered categories.
///
/// Each list is kept sorted by [`add_known_category`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KnownCategories {
    /// Sorted list of registered primary categories.
    #[serde(default)]
    pub primary: Vec<String>,
    /// Sorted list of registered secondary categories.
    #[serde(default)]
    pub secondary: Vec<String>,
}

/// Return the finguard config directory, creating it if necessary.
fn get_config_dir() -> Result<PathBuf> {
    let base = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        PathBuf::from(xdg)
    } else {
        dirs::home_dir().ok_or(Error::NoHomeDir)?.join(".config")
    };

    let config_dir = base.join(CONFIG_DIR_NAME);
    std::fs::create_dir_all(&config_dir)?;
    Ok(config_dir)
}

/// Return the full path to the category-mappings JSON file.
fn get_config_path() -> Result<PathBuf> {
    Ok(get_config_dir()?.join(CONFIG_FILE_NAME))
}

/// Serialize `value` to pretty JSON, matching Python's
/// `json.dump(..., indent=4, ensure_ascii=False)` byte-for-byte.
///
/// `serde_json`'s default pretty printer uses a *2-space* indent, so I install
/// a [`PrettyFormatter`] with a 4-space indent. serde_json never escapes
/// non-ASCII characters (it emits UTF-8 directly), matching `ensure_ascii=False`.
/// Like Python's `json.dump`, no trailing newline is written.
fn write_json<T: Serialize>(path: &PathBuf, value: &T) -> Result<()> {
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    value.serialize(&mut ser)?;
    std::fs::write(path, buf)?;
    Ok(())
}

// ------------------------------------------------------------------
// Mappings
// ------------------------------------------------------------------

/// Load the mappings file from disk. Returns an empty map if the file does not
/// exist yet.
fn load_mappings() -> Result<IndexMap<String, CategoryMapping>> {
    let path = get_config_path()?;
    if !path.exists() {
        return Ok(IndexMap::new());
    }
    let contents = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&contents)?)
}

/// Persist `mappings` to disk (pretty-printed for easy hand-editing).
fn save_mappings(mappings: &IndexMap<String, CategoryMapping>) -> Result<()> {
    write_json(&get_config_path()?, mappings)
}

/// Add (or update) a mapping from `expense_name` to a category pair.
///
/// The key and both category values are stored stripped + lower-cased. If
/// `overwrite` is `false` and the key already exists, an
/// [`Error::AlreadyExists`] is returned; set `overwrite` to `true` to silently
/// replace an existing entry.
pub fn add_mapping(
    expense_name: &str,
    primary_category: &str,
    secondary_category: &str,
    overwrite: bool,
) -> Result<()> {
    let key = expense_name.trim().to_lowercase();
    let mut mappings = load_mappings()?;

    if mappings.contains_key(&key) && !overwrite {
        let existing = &mappings[&key];
        return Err(Error::AlreadyExists(format!(
            "Mapping for '{key}' already exists: {{'primary_category': '{}', \
             'secondary_category': '{}'}}. Pass overwrite=true to replace it.",
            existing.primary_category, existing.secondary_category
        )));
    }

    mappings.insert(
        key,
        CategoryMapping {
            primary_category: primary_category.trim().to_lowercase(),
            secondary_category: secondary_category.trim().to_lowercase(),
        },
    );
    save_mappings(&mappings)
}

/// Remove the mapping for `expense_name` and return the deleted entry.
///
/// # Errors
///
/// Returns [`Error::NotFound`] if the name is not found.
pub fn remove_mapping(expense_name: &str) -> Result<CategoryMapping> {
    let key = expense_name.trim().to_lowercase();
    let mut mappings = load_mappings()?;

    let removed = mappings
        .shift_remove(&key)
        .ok_or_else(|| Error::NotFound(format!("No mapping found for '{key}'.")))?;

    save_mappings(&mappings)?;
    Ok(removed)
}

/// Look up the category mapping for `expense_name`.
///
/// Returns `Ok(None)` if no mapping exists.
pub fn get_mapping(expense_name: &str) -> Result<Option<CategoryMapping>> {
    let key = expense_name.trim().to_lowercase();
    Ok(load_mappings()?.shift_remove(&key))
}

/// Return a copy of every stored mapping, in insertion order.
pub fn get_all_mappings() -> Result<IndexMap<String, CategoryMapping>> {
    load_mappings()
}

/// Delete all mappings (the file is kept but emptied).
pub fn clear_all_mappings() -> Result<()> {
    save_mappings(&IndexMap::new())
}

// ------------------------------------------------------------------
// Known categories
// ------------------------------------------------------------------

/// Return the full path to the known-categories JSON file.
fn get_categories_path() -> Result<PathBuf> {
    Ok(get_config_dir()?.join(CATEGORIES_FILE_NAME))
}

/// Load known categories from disk. Returns an empty structure if the file does
/// not exist yet.
fn load_known_categories() -> Result<KnownCategories> {
    let path = get_categories_path()?;
    if !path.exists() {
        return Ok(KnownCategories::default());
    }
    let contents = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&contents)?)
}

/// Persist known categories to disk (pretty-printed).
fn save_known_categories(data: &KnownCategories) -> Result<()> {
    write_json(&get_categories_path()?, data)
}

/// Validate a category `kind`, returning an [`Error::InvalidArgument`] if it is
/// neither `"primary"` nor `"secondary"`.
fn validate_kind(kind: &str) -> Result<()> {
    if kind != "primary" && kind != "secondary" {
        return Err(Error::InvalidArgument(format!(
            "kind must be 'primary' or 'secondary', got '{kind}'"
        )));
    }
    Ok(())
}

/// Return the appropriate category list for `kind`. Assumes `kind` is valid.
fn list_for_kind<'a>(data: &'a mut KnownCategories, kind: &str) -> &'a mut Vec<String> {
    if kind == "primary" {
        &mut data.primary
    } else {
        &mut data.secondary
    }
}

/// Return all manually registered categories.
///
/// Each list is a sorted list of category-name strings.
pub fn get_known_categories() -> Result<KnownCategories> {
    load_known_categories()
}

/// Register a new known category.
///
/// `kind` must be `"primary"` or `"secondary"`. The list is kept sorted.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] if `kind` is invalid, or
/// [`Error::AlreadyExists`] if the category already exists.
pub fn add_known_category(name: &str, kind: &str) -> Result<()> {
    validate_kind(kind)?;
    let mut data = load_known_categories()?;
    let list = list_for_kind(&mut data, kind);
    if list.iter().any(|c| c == name) {
        return Err(Error::AlreadyExists(format!(
            "Category '{name}' already exists in {kind} categories."
        )));
    }
    list.push(name.to_string());
    list.sort();
    save_known_categories(&data)
}

/// Remove a manually registered category.
///
/// `kind` must be `"primary"` or `"secondary"`.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] if `kind` is invalid, or
/// [`Error::NotFound`] if the category is not found.
pub fn remove_known_category(name: &str, kind: &str) -> Result<()> {
    validate_kind(kind)?;
    let mut data = load_known_categories()?;
    let list = list_for_kind(&mut data, kind);
    let Some(pos) = list.iter().position(|c| c == name) else {
        return Err(Error::NotFound(format!(
            "Category '{name}' not found in {kind} categories."
        )));
    };
    list.remove(pos);
    save_known_categories(&data)
}
