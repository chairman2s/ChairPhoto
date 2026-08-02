//! Smart albums: saved, named RULES that resolve to a photo set — the dynamic
//! counterpart to manual albums (`albums.rs`). See docs/smart-albums.md.
//!
//! Membership is evaluated **live**, never stored: a smart album's `rule_json` is
//! translated by [`rule_to_sql`] into a list of `WHERE` fragments (over the `photos`
//! alias `p`) plus ordered bound parameters, which are ANDed into `list_photos`
//! (C2b). So `photo_count` is computed on the fly by running the translated query as
//! a `COUNT` — there is no `smart_album_photos` table and no rebuild/staleness.
//!
//! The translator mirrors the predicate style of `facets.rs`, with a strict
//! field/operator allowlist: every rule value is a **bound parameter** (no string
//! interpolation → injection-safe), and an unknown field or operator is a
//! `CatalogError::Validation` (never silently ignored).

use super::{Catalog, CatalogError, Result, SmartAlbum};
use rusqlite::{params, ToSql};
use std::time::{SystemTime, UNIX_EPOCH};

impl Catalog {
    /// Create a smart album (placed at the end) and return its id. `rule_json` is
    /// validated by translating it; an invalid rule is rejected before insert.
    pub fn create_smart_album(&self, name: &str, rule_json: &str) -> Result<i64> {
        // Reject a malformed rule up front (don't persist something unqueryable).
        rule_to_sql(rule_json)?;
        let position: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM smart_albums",
            [],
            |r| r.get(0),
        )?;
        let ts = now();
        self.conn.execute(
            "INSERT INTO smart_albums(uuid, name, rule_json, position, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?5)",
            params![
                uuid::Uuid::new_v4().to_string(),
                name.trim(),
                rule_json,
                position,
                ts
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rename_smart_album(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE smart_albums SET name = ?1, updated_at = ?2 WHERE id = ?3",
            params![name.trim(), now(), id],
        )?;
        Ok(())
    }

    /// Replace a smart album's rule. The new rule is validated (translated) first.
    pub fn set_smart_album_rule(&self, id: i64, rule_json: &str) -> Result<()> {
        rule_to_sql(rule_json)?;
        self.conn.execute(
            "UPDATE smart_albums SET rule_json = ?1, updated_at = ?2 WHERE id = ?3",
            params![rule_json, now(), id],
        )?;
        Ok(())
    }

    pub fn delete_smart_album(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM smart_albums WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn get_smart_album(&self, id: i64) -> Result<SmartAlbum> {
        let (uuid, name, rule_json): (String, String, String) = self
            .conn
            .query_row(
                "SELECT uuid, name, rule_json FROM smart_albums WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    CatalogError::NotFound(format!("smart album {id}"))
                }
                other => CatalogError::Sqlite(other),
            })?;
        let photo_count = self.smart_album_count(&rule_json)?;
        Ok(SmartAlbum {
            id,
            uuid,
            name,
            rule_json,
            photo_count,
        })
    }

    /// The raw rule string of a smart album (for the query layer to translate).
    /// Used by `list_photos`' `smart_album_id` integration.
    pub(crate) fn smart_album_rule_json(&self, id: i64) -> Result<String> {
        self.conn
            .query_row(
                "SELECT rule_json FROM smart_albums WHERE id = ?1",
                params![id],
                |r| r.get::<_, String>(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    CatalogError::NotFound(format!("smart album {id}"))
                }
                other => CatalogError::Sqlite(other),
            })
    }

    /// All smart albums, ordered, each with its **live** count of (non-missing)
    /// matching photos.
    pub fn list_smart_albums(&self) -> Result<Vec<SmartAlbum>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, uuid, name, rule_json FROM smart_albums ORDER BY position, name")?;
        let rows: Vec<(i64, String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, uuid, name, rule_json) in rows {
            let photo_count = self.smart_album_count(&rule_json)?;
            out.push(SmartAlbum {
                id,
                uuid,
                name,
                rule_json,
                photo_count,
            });
        }
        Ok(out)
    }

    /// Reorder smart albums to match `ordered_ids` (sidebar drag-and-drop). Ids not
    /// listed keep their relative order after the listed ones.
    pub fn reorder_smart_albums(&self, ordered_ids: &[i64]) -> Result<()> {
        for (position, id) in ordered_ids.iter().enumerate() {
            self.conn.execute(
                "UPDATE smart_albums SET position = ?1, updated_at = ?2 WHERE id = ?3",
                params![position as i64, now(), id],
            )?;
        }
        Ok(())
    }

    /// Live count of (non-missing) photos matching an arbitrary `rule_json`. This is
    /// the helper the command layer reuses for the builder's live preview
    /// (`smart_album_count`) and for `list_smart_albums`' per-row count. It runs the
    /// translated predicates as a `COUNT(*)`, so it always reflects the current library.
    pub fn smart_album_count(&self, rule_json: &str) -> Result<i64> {
        let (wheres, binds) = rule_to_sql(rule_json)?;
        let mut sql = String::from("SELECT COUNT(*) FROM photos p WHERE p.missing = 0");
        for w in &wheres {
            sql.push_str(" AND ");
            sql.push_str(w);
        }
        let params: Vec<&dyn ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        Ok(self
            .conn
            .query_row(&sql, params.as_slice(), |r| r.get(0))?)
    }
}

// ----------------------------------------------------------------------------
// The rule_to_sql translator (the linchpin — see docs/smart-albums.md).
// ----------------------------------------------------------------------------

/// Translate a smart-album Rule JSON into `WHERE` fragments (each over the `photos`
/// alias `p`, with `?` placeholders) plus the ordered bound parameters to supply.
///
/// The caller ANDs the fragments into the existing `list_photos` builder (or a
/// `COUNT`). An empty condition list yields **no** predicates (matches all photos).
/// Every value is bound (no interpolation); an unknown `field`/`op` — or a value of
/// the wrong shape for the operator — is a `CatalogError::Validation`.
pub fn rule_to_sql(rule_json: &str) -> Result<(Vec<String>, Vec<Box<dyn ToSql>>)> {
    let rule: serde_json::Value = serde_json::from_str(rule_json)
        .map_err(|e| CatalogError::Validation(format!("invalid rule JSON: {e}")))?;

    // v1 is AND-only; "match" is reserved for future "any" and must be "all" if present.
    if let Some(m) = rule.get("match").and_then(|v| v.as_str()) {
        if m != "all" {
            return Err(CatalogError::Validation(format!(
                "unsupported match mode: {m} (v1 is AND-only)"
            )));
        }
    }

    let mut wheres: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn ToSql>> = Vec::new();

    let conditions = match rule.get("conditions") {
        None => return Ok((wheres, binds)), // empty rule → matches all
        Some(serde_json::Value::Array(a)) => a,
        Some(_) => return Err(CatalogError::Validation("conditions must be an array".into())),
    };

    for cond in conditions {
        let field = cond
            .get("field")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CatalogError::Validation("condition missing 'field'".into()))?;
        let op = cond
            .get("op")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CatalogError::Validation("condition missing 'op'".into()))?;
        let value = cond.get("value").unwrap_or(&serde_json::Value::Null);

        translate_condition(field, op, value, &mut wheres, &mut binds)?;
    }

    Ok((wheres, binds))
}

/// Numeric fields (int + real) supporting `eq` `gte` `lte` `between`.
const NUMERIC_FIELDS: &[(&str, &str)] = &[
    ("rating", "p.rating"),
    ("iso", "p.iso"),
    ("aperture", "p.aperture"),
    ("focal_length", "p.focal_length"),
];

/// Free-text fields supporting `is` `contains` `isNot` `isSet`.
const TEXT_FIELDS: &[(&str, &str)] = &[
    ("camera_make", "p.camera_make"),
    ("camera_model", "p.camera_model"),
    ("lens", "p.lens"),
    ("shutter_speed", "p.shutter_speed"),
    ("color_label", "p.color_label"),
];

fn translate_condition(
    field: &str,
    op: &str,
    value: &serde_json::Value,
    wheres: &mut Vec<String>,
    binds: &mut Vec<Box<dyn ToSql>>,
) -> Result<()> {
    if let Some((_, col)) = NUMERIC_FIELDS.iter().find(|(f, _)| *f == field) {
        return numeric_predicate(col, op, value, wheres, binds);
    }
    if let Some((_, col)) = TEXT_FIELDS.iter().find(|(f, _)| *f == field) {
        return text_predicate(col, op, value, wheres, binds);
    }

    match field {
        "pick_state" => pick_state_predicate(op, value, wheres, binds),
        "capture_time" => capture_time_predicate(op, value, wheres, binds),
        "tag" => tag_predicate(op, value, wheres, binds),
        "batch" => batch_predicate(op, value, wheres, binds),
        "flag" => flag_predicate(op, value, wheres),
        other => Err(CatalogError::Validation(format!("unknown field: {other}"))),
    }
}

fn numeric_predicate(
    col: &str,
    op: &str,
    value: &serde_json::Value,
    wheres: &mut Vec<String>,
    binds: &mut Vec<Box<dyn ToSql>>,
) -> Result<()> {
    match op {
        "eq" => {
            wheres.push(format!("{col} = ?"));
            binds.push(json_number(value)?);
        }
        "gte" => {
            wheres.push(format!("{col} >= ?"));
            binds.push(json_number(value)?);
        }
        "lte" => {
            wheres.push(format!("{col} <= ?"));
            binds.push(json_number(value)?);
        }
        "between" => {
            let (lo, hi) = json_pair(value)?;
            wheres.push(format!("{col} BETWEEN ? AND ?"));
            binds.push(json_number(lo)?);
            binds.push(json_number(hi)?);
        }
        other => return Err(CatalogError::Validation(format!("unknown op '{other}' for numeric field"))),
    }
    Ok(())
}

fn text_predicate(
    col: &str,
    op: &str,
    value: &serde_json::Value,
    wheres: &mut Vec<String>,
    binds: &mut Vec<Box<dyn ToSql>>,
) -> Result<()> {
    match op {
        // Case-insensitive equality (COLLATE NOCASE), so a "Red" color label matches a rule
        // value of "red"/"Red" and text fields behave like the case-insensitive `contains`.
        "is" => {
            wheres.push(format!("{col} = ? COLLATE NOCASE"));
            binds.push(json_string(value)?);
        }
        "isNot" => {
            wheres.push(format!("{col} <> ? COLLATE NOCASE"));
            binds.push(json_string(value)?);
        }
        // Case-insensitive substring. LIKE is already ASCII case-insensitive in SQLite;
        // the bound value carries its own %…% wildcards (the column name is never
        // interpolated with user input).
        "contains" => {
            let s = require_string(value)?;
            wheres.push(format!("{col} LIKE ?"));
            binds.push(Box::new(format!("%{s}%")));
        }
        // "is set" = present and non-empty (text columns default to '').
        "isSet" => {
            wheres.push(format!("({col} IS NOT NULL AND {col} <> '')"));
        }
        other => return Err(CatalogError::Validation(format!("unknown op '{other}' for text field"))),
    }
    Ok(())
}

fn pick_state_predicate(
    op: &str,
    value: &serde_json::Value,
    wheres: &mut Vec<String>,
    binds: &mut Vec<Box<dyn ToSql>>,
) -> Result<()> {
    let s = require_string(value)?;
    if !matches!(s.as_str(), "none" | "pick" | "reject") {
        return Err(CatalogError::Validation(format!("invalid pick_state: {s}")));
    }
    match op {
        "is" => {
            wheres.push("p.pick_state = ?".into());
            binds.push(Box::new(s));
        }
        "isNot" => {
            wheres.push("p.pick_state <> ?".into());
            binds.push(Box::new(s));
        }
        other => return Err(CatalogError::Validation(format!("unknown op '{other}' for pick_state"))),
    }
    Ok(())
}

fn capture_time_predicate(
    op: &str,
    value: &serde_json::Value,
    wheres: &mut Vec<String>,
    binds: &mut Vec<Box<dyn ToSql>>,
) -> Result<()> {
    // capture_time is stored as ISO 8601 text, so lexicographic comparison == time order.
    match op {
        "before" => {
            wheres.push("p.capture_time < ?".into());
            binds.push(json_string(value)?);
        }
        "after" => {
            wheres.push("p.capture_time > ?".into());
            binds.push(json_string(value)?);
        }
        "between" => {
            let (lo, hi) = json_pair(value)?;
            wheres.push("p.capture_time BETWEEN ? AND ?".into());
            binds.push(json_string(lo)?);
            binds.push(json_string(hi)?);
        }
        other => return Err(CatalogError::Validation(format!("unknown op '{other}' for capture_time"))),
    }
    Ok(())
}

fn tag_predicate(
    op: &str,
    value: &serde_json::Value,
    wheres: &mut Vec<String>,
    binds: &mut Vec<Box<dyn ToSql>>,
) -> Result<()> {
    let tag_id = json_integer(value)?;
    match op {
        // Exact tag (no descendants).
        "is" => {
            wheres.push(
                "EXISTS (SELECT 1 FROM photo_tags pt WHERE pt.photo_id = p.id AND pt.tag_id = ?)"
                    .into(),
            );
            binds.push(Box::new(tag_id));
        }
        // The tag and all of its descendants. Expanded with the same recursive CTE as
        // `descendant_tag_ids`, inlined so the translator stays a pure function (no DB
        // handle); only the root tag id is bound. EXISTS keeps multiple tag conditions
        // ANDing correctly.
        "under" => {
            wheres.push(
                "EXISTS (\
                   WITH RECURSIVE descendants(id) AS (\
                     SELECT id FROM tags WHERE id = ? \
                     UNION ALL \
                     SELECT tags.id FROM tags JOIN descendants ON tags.parent_id = descendants.id\
                   ) \
                   SELECT 1 FROM photo_tags pt \
                   WHERE pt.photo_id = p.id AND pt.tag_id IN (SELECT id FROM descendants)\
                 )"
                .into(),
            );
            binds.push(Box::new(tag_id));
        }
        other => return Err(CatalogError::Validation(format!("unknown op '{other}' for tag"))),
    }
    Ok(())
}

fn batch_predicate(
    op: &str,
    value: &serde_json::Value,
    wheres: &mut Vec<String>,
    binds: &mut Vec<Box<dyn ToSql>>,
) -> Result<()> {
    match op {
        "is" => {
            wheres.push("p.import_batch_id = ?".into());
            binds.push(Box::new(json_integer(value)?));
        }
        other => return Err(CatalogError::Validation(format!("unknown op '{other}' for batch"))),
    }
    Ok(())
}

fn flag_predicate(op: &str, value: &serde_json::Value, wheres: &mut Vec<String>) -> Result<()> {
    if op != "is" {
        return Err(CatalogError::Validation(format!("unknown op '{op}' for flag")));
    }
    // Derived predicates only — no user value is interpolated (the flag name selects a
    // fixed, allowlisted SQL fragment).
    let flag = require_string(value)?;
    let pred = match flag.as_str() {
        "has-gps" => "(p.gps_latitude IS NOT NULL AND p.gps_longitude IS NOT NULL)",
        "monochrome" => "p.is_grayscale = 1",
        "is-raw" => RAW_EXTENSION_PREDICATE,
        other => return Err(CatalogError::Validation(format!("unknown flag: {other}"))),
    };
    wheres.push(pred.to_string());
    Ok(())
}

/// `p.extension IN (…)` over the RAW extension set (kept in sync with
/// `scanner::RAW_EXTENSIONS`; stored extensions are lowercased on upsert). Built once
/// as a static string — no user value is interpolated.
const RAW_EXTENSION_PREDICATE: &str = "p.extension IN (\
    '3fr','ari','arw','bay','braw','cr2','cr3','crw','dcr','dng','erf','fff','iiq',\
    'k25','kdc','mef','mos','mrw','nef','nrw','orf','pef','raf','raw','rw2','rwl',\
    'sr2','srf','srw','x3f')";

// --- JSON value coercion helpers -------------------------------------------

/// A JSON number (int or real) as a bound parameter. Integers bind as i64 so
/// `rating`/`iso` compare cleanly; non-integers bind as f64.
fn json_number(value: &serde_json::Value) -> Result<Box<dyn ToSql>> {
    if let Some(i) = value.as_i64() {
        Ok(Box::new(i))
    } else if let Some(f) = value.as_f64() {
        Ok(Box::new(f))
    } else {
        Err(CatalogError::Validation(format!(
            "expected a number, got {value}"
        )))
    }
}

fn json_integer(value: &serde_json::Value) -> Result<i64> {
    value
        .as_i64()
        .ok_or_else(|| CatalogError::Validation(format!("expected an integer, got {value}")))
}

/// A JSON string bound as a parameter.
fn json_string(value: &serde_json::Value) -> Result<Box<dyn ToSql>> {
    Ok(Box::new(require_string(value)?))
}

fn require_string(value: &serde_json::Value) -> Result<String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| CatalogError::Validation(format!("expected a string, got {value}")))
}

/// The two-element `[lo, hi]` array a `between` operator requires.
fn json_pair(value: &serde_json::Value) -> Result<(&serde_json::Value, &serde_json::Value)> {
    match value.as_array() {
        Some(a) if a.len() == 2 => Ok((&a[0], &a[1])),
        _ => Err(CatalogError::Validation(
            "'between' requires a [low, high] array".into(),
        )),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use std::path::PathBuf;

    /// Translate, asserting success, and return (wheres, bind count).
    fn tr(rule: &str) -> (Vec<String>, usize) {
        let (wheres, binds) = rule_to_sql(rule).expect("rule should translate");
        (wheres, binds.len())
    }

    fn cond(field: &str, op: &str, value: serde_json::Value) -> String {
        serde_json::json!({ "conditions": [{ "field": field, "op": op, "value": value }] })
            .to_string()
    }

    #[test]
    fn empty_rule_matches_all() {
        let (wheres, binds) = tr(r#"{"match":"all","conditions":[]}"#);
        assert!(wheres.is_empty());
        assert_eq!(binds, 0);
        // No "conditions" key at all is also "matches all".
        let (wheres, binds) = tr(r#"{}"#);
        assert!(wheres.is_empty());
        assert_eq!(binds, 0);
    }

    #[test]
    fn numeric_operators() {
        let (w, b) = tr(&cond("rating", "gte", serde_json::json!(4)));
        assert_eq!(w, vec!["p.rating >= ?".to_string()]);
        assert_eq!(b, 1);

        let (w, b) = tr(&cond("iso", "lte", serde_json::json!(400)));
        assert_eq!(w, vec!["p.iso <= ?".to_string()]);
        assert_eq!(b, 1);

        let (w, b) = tr(&cond("aperture", "eq", serde_json::json!(2.8)));
        assert_eq!(w, vec!["p.aperture = ?".to_string()]);
        assert_eq!(b, 1);

        let (w, b) = tr(&cond("focal_length", "between", serde_json::json!([24, 70])));
        assert_eq!(w, vec!["p.focal_length BETWEEN ? AND ?".to_string()]);
        assert_eq!(b, 2);
    }

    #[test]
    fn text_operators() {
        let (w, b) = tr(&cond("camera_model", "is", serde_json::json!("ILCE-7RM6")));
        assert_eq!(w, vec!["p.camera_model = ? COLLATE NOCASE".to_string()]);
        assert_eq!(b, 1);

        let (w, b) = tr(&cond("lens", "contains", serde_json::json!("24-70")));
        assert_eq!(w, vec!["p.lens LIKE ?".to_string()]);
        assert_eq!(b, 1);

        let (w, b) = tr(&cond("color_label", "isNot", serde_json::json!("red")));
        assert_eq!(w, vec!["p.color_label <> ? COLLATE NOCASE".to_string()]);
        assert_eq!(b, 1);

        // isSet is a no-bind presence check.
        let (w, b) = tr(&cond("camera_make", "isSet", serde_json::Value::Null));
        assert_eq!(
            w,
            vec!["(p.camera_make IS NOT NULL AND p.camera_make <> '')".to_string()]
        );
        assert_eq!(b, 0);
    }

    #[test]
    fn pick_state_and_capture_time() {
        let (w, b) = tr(&cond("pick_state", "is", serde_json::json!("pick")));
        assert_eq!(w, vec!["p.pick_state = ?".to_string()]);
        assert_eq!(b, 1);

        let (w, b) = tr(&cond(
            "capture_time",
            "between",
            serde_json::json!(["2026-06-01", "2026-07-01"]),
        ));
        assert_eq!(w, vec!["p.capture_time BETWEEN ? AND ?".to_string()]);
        assert_eq!(b, 2);

        let (w, b) = tr(&cond("capture_time", "after", serde_json::json!("2026-01-01")));
        assert_eq!(w, vec!["p.capture_time > ?".to_string()]);
        assert_eq!(b, 1);
    }

    #[test]
    fn batch_and_flags() {
        let (w, b) = tr(&cond("batch", "is", serde_json::json!(7)));
        assert_eq!(w, vec!["p.import_batch_id = ?".to_string()]);
        assert_eq!(b, 1);

        // Flags are derived predicates with NO binds (the flag name selects a fixed SQL).
        let (w, b) = tr(&cond("flag", "is", serde_json::json!("has-gps")));
        assert_eq!(
            w,
            vec!["(p.gps_latitude IS NOT NULL AND p.gps_longitude IS NOT NULL)".to_string()]
        );
        assert_eq!(b, 0);

        let (w, b) = tr(&cond("flag", "is", serde_json::json!("monochrome")));
        assert_eq!(w, vec!["p.is_grayscale = 1".to_string()]);
        assert_eq!(b, 0);

        let (w, b) = tr(&cond("flag", "is", serde_json::json!("is-raw")));
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("p.extension IN ("));
        assert_eq!(b, 0);
    }

    #[test]
    fn tag_is_and_under() {
        let (w, b) = tr(&cond("tag", "is", serde_json::json!(42)));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("pt.tag_id = ?"));
        assert!(!w[0].contains("RECURSIVE"));
        assert_eq!(b, 1);

        let (w, b) = tr(&cond("tag", "under", serde_json::json!(42)));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("RECURSIVE descendants"));
        assert!(w[0].contains("pt.tag_id IN (SELECT id FROM descendants)"));
        assert_eq!(b, 1, "only the root tag id is bound");
    }

    #[test]
    fn and_composition_preserves_order_and_binds() {
        let rule = serde_json::json!({
            "match": "all",
            "conditions": [
                { "field": "rating",       "op": "gte",      "value": 4 },
                { "field": "camera_model", "op": "is",       "value": "ILCE-7RM6" },
                { "field": "capture_time", "op": "between",  "value": ["a", "b"] },
                { "field": "flag",         "op": "is",       "value": "has-gps" }
            ]
        })
        .to_string();
        let (wheres, binds) = rule_to_sql(&rule).unwrap();
        assert_eq!(wheres.len(), 4, "one fragment per condition, in order");
        assert_eq!(wheres[0], "p.rating >= ?");
        assert_eq!(wheres[1], "p.camera_model = ? COLLATE NOCASE");
        assert_eq!(wheres[2], "p.capture_time BETWEEN ? AND ?");
        // rating(1) + camera_model(1) + capture_time(2) + flag(0) = 4
        assert_eq!(binds.len(), 4);
    }

    #[test]
    fn unknown_field_and_op_are_validation_errors() {
        assert!(matches!(
            rule_to_sql(&cond("nope", "is", serde_json::json!(1))),
            Err(CatalogError::Validation(_))
        ));
        assert!(matches!(
            rule_to_sql(&cond("rating", "contains", serde_json::json!(1))),
            Err(CatalogError::Validation(_))
        ));
        assert!(matches!(
            rule_to_sql(&cond("camera_make", "gte", serde_json::json!("x"))),
            Err(CatalogError::Validation(_))
        ));
        assert!(matches!(
            rule_to_sql(&cond("tag", "before", serde_json::json!(1))),
            Err(CatalogError::Validation(_))
        ));
        // Non-"all" match mode is rejected (AND-only in v1).
        assert!(matches!(
            rule_to_sql(r#"{"match":"any","conditions":[]}"#),
            Err(CatalogError::Validation(_))
        ));
        // Malformed JSON is a validation error, not a panic.
        assert!(matches!(
            rule_to_sql("not json"),
            Err(CatalogError::Validation(_))
        ));
    }

    fn temp_catalog(tag: &str) -> (Catalog, PathBuf) {
        let dir = std::env::temp_dir().join(format!("chairphoto-sa-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        (catalog, root)
    }

    #[test]
    fn crud_roundtrip_and_validation() {
        let (cat, _root) = temp_catalog("crud");
        let id = cat
            .create_smart_album("Keepers", r#"{"conditions":[{"field":"rating","op":"gte","value":4}]}"#)
            .unwrap();
        let album = cat.get_smart_album(id).unwrap();
        assert_eq!(album.name, "Keepers");
        assert_eq!(album.photo_count, 0); // empty catalog

        cat.rename_smart_album(id, "Best").unwrap();
        cat.set_smart_album_rule(id, r#"{"conditions":[]}"#).unwrap();
        assert_eq!(cat.get_smart_album(id).unwrap().name, "Best");
        assert_eq!(cat.list_smart_albums().unwrap().len(), 1);

        // A bad rule is rejected at create time (not persisted).
        assert!(cat.create_smart_album("Bad", r#"{"conditions":[{"field":"x"}]}"#).is_err());

        cat.delete_smart_album(id).unwrap();
        assert!(cat.list_smart_albums().unwrap().is_empty());
    }

    #[test]
    fn live_count_with_tag_descendants() {
        let (cat, root) = temp_catalog("count");
        // Two photos; one tagged under a parent, one under its child.
        let p1 = cat
            .upsert_photo(&root.join("a.arw"), None, 1, 10)
            .unwrap()
            .id;
        let p2 = cat
            .upsert_photo(&root.join("b.arw"), None, 2, 20)
            .unwrap()
            .id;
        let parent = cat.create_tag("Animals/Birds").unwrap();
        let child = cat.create_tag("Animals/Birds/Owl").unwrap();
        cat.assign_tag(p1, parent).unwrap();
        cat.assign_tag(p2, child).unwrap();

        // `under` the parent matches both (descendant expansion).
        let under = format!(
            r#"{{"conditions":[{{"field":"tag","op":"under","value":{parent}}}]}}"#
        );
        assert_eq!(cat.smart_album_count(&under).unwrap(), 2);

        // `is` the child (exact) matches only the owl photo.
        let exact = format!(
            r#"{{"conditions":[{{"field":"tag","op":"is","value":{child}}}]}}"#
        );
        assert_eq!(cat.smart_album_count(&exact).unwrap(), 1);

        // Empty rule counts all (non-missing) photos.
        assert_eq!(cat.smart_album_count(r#"{"conditions":[]}"#).unwrap(), 2);
    }
}
