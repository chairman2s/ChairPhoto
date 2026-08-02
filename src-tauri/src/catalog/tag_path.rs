//! Tag path normalization. Port of the old Python `tags.py`.
//!
//! A tag path is a hierarchy like "Transportation/Watercraft/Ferry". Both `/`
//! and `|` are accepted as separators on input (Lightroom uses `|` in XMP). The
//! canonical storage form joins components with `/`; the XMP form joins with `|`.
//!
//! `key()` is the case-folded, whitespace-collapsed lookup form used for the
//! UNIQUE `full_path_norm` column — two tags whose paths differ only by case or
//! spacing are the same tag.

/// A validated, split tag path.
pub struct NormalizedTag {
    pub components: Vec<String>,
}

impl NormalizedTag {
    /// Canonical storage form: components joined with '/'.
    pub fn storage_path(&self) -> String {
        self.components.join("/")
    }

    /// XMP/Lightroom form: components joined with '|'. Used by tag export (planned).
    #[allow(dead_code)]
    pub fn xmp_path(&self) -> String {
        self.components.join("|")
    }

    /// Last component (the tag's own name).
    #[allow(dead_code)]
    pub fn leaf(&self) -> &str {
        self.components.last().map(String::as_str).unwrap_or("")
    }

    /// Case-folded lookup key for the UNIQUE full_path_norm column.
    pub fn key(&self) -> String {
        normalize_lookup(&self.storage_path())
    }
}

/// Collapse internal whitespace and case-fold for lookups.
pub fn normalize_lookup(value: &str) -> String {
    collapse_whitespace(value.trim()).to_lowercase()
}

/// Split a tag path on `/` or `|`, trim and collapse each component, and validate.
pub fn normalize_tag_path(value: &str) -> Result<NormalizedTag, String> {
    let components: Vec<String> = value
        .split(|c| c == '/' || c == '|')
        .map(|part| collapse_whitespace(part.trim()))
        .filter(|part| !part.is_empty())
        .collect();

    if components.is_empty() {
        return Err("Tag path cannot be empty.".into());
    }
    if components.iter().any(|c| c == "." || c == "..") {
        return Err("Tag path cannot contain '.' or '..'.".into());
    }
    Ok(NormalizedTag { components })
}

/// Replace any run of whitespace with a single space.
fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_both_separators() {
        let tag = normalize_tag_path("Transportation | Watercraft / Ferry").unwrap();
        assert_eq!(tag.storage_path(), "Transportation/Watercraft/Ferry");
        assert_eq!(tag.xmp_path(), "Transportation|Watercraft|Ferry");
        assert_eq!(tag.leaf(), "Ferry");
    }

    #[test]
    fn key_is_case_and_space_insensitive() {
        let a = normalize_tag_path("Birds/Owls").unwrap();
        let b = normalize_tag_path("birds /  owls").unwrap();
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn rejects_empty_and_dot_paths() {
        assert!(normalize_tag_path("   ").is_err());
        assert!(normalize_tag_path("a/../b").is_err());
    }
}
