//! The smart-album rule builder's model (`SmartAlbumEditor.tsx`): the field/operator matrix
//! of docs/smart-albums.md, a row's value as the widgets hold it, and the round trip to the
//! Rule JSON contract `{ match: "all", conditions: [{ field, op, value }, …] }` (AND-only in
//! v1). Pure: the editor view renders it, the core evaluates the JSON (`rule_to_sql`).

use crate::shell::style::COLOR_LABELS;
use serde_json::{json, Value};

/// How a field's value is entered: the value widget and the JSON type it becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// Rating, ISO: a whole number.
    Int,
    /// Aperture, focal length: a decimal.
    Real,
    /// Camera make/model, lens, shutter speed: free text.
    Text,
    /// A closed value set (colour label, pick state, flag).
    Enum,
    /// Capture date, `YYYY-MM-DD`.
    Date,
    /// A tag id, picked from the tree.
    Tag,
    /// An import batch id.
    Batch,
}

/// The operators, with their wire names and labels (React's `OP_LABELS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Gte,
    Lte,
    Between,
    Is,
    IsNot,
    IsSet,
    Contains,
    Before,
    After,
    Under,
}

impl Op {
    pub fn wire(self) -> &'static str {
        match self {
            Op::Eq => "eq",
            Op::Gte => "gte",
            Op::Lte => "lte",
            Op::Between => "between",
            Op::Is => "is",
            Op::IsNot => "isNot",
            Op::IsSet => "isSet",
            Op::Contains => "contains",
            Op::Before => "before",
            Op::After => "after",
            Op::Under => "under",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Op::Eq | Op::Is => "is",
            Op::Gte => "≥",
            Op::Lte => "≤",
            Op::Between => "between",
            Op::IsNot => "is not",
            Op::IsSet => "is set",
            Op::Contains => "contains",
            Op::Before => "before",
            Op::After => "after",
            Op::Under => "under (incl. children)",
        }
    }

    const ALL: [Op; 11] =
        [Op::Eq, Op::Gte, Op::Lte, Op::Between, Op::Is, Op::IsNot, Op::IsSet, Op::Contains, Op::Before, Op::After, Op::Under];

    fn from_wire(s: &str) -> Option<Op> {
        Op::ALL.into_iter().find(|o| o.wire() == s)
    }
}

/// One row of the field matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldDef {
    pub field: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub kind: ValueKind,
    pub ops: &'static [Op],
}

const NUMERIC: &[Op] = &[Op::Eq, Op::Gte, Op::Lte, Op::Between];
const TEXT: &[Op] = &[Op::Is, Op::Contains];

/// Every field, in the order the field menu lists them (grouped by [`GROUPS`]).
pub const FIELDS: [FieldDef; 14] = [
    FieldDef { field: "camera_make", label: "Camera make", group: GROUPS[0], kind: ValueKind::Text, ops: TEXT },
    FieldDef { field: "camera_model", label: "Camera model", group: GROUPS[0], kind: ValueKind::Text, ops: TEXT },
    FieldDef { field: "lens", label: "Lens", group: GROUPS[0], kind: ValueKind::Text, ops: TEXT },
    FieldDef { field: "iso", label: "ISO", group: GROUPS[0], kind: ValueKind::Int, ops: NUMERIC },
    FieldDef { field: "aperture", label: "Aperture", group: GROUPS[0], kind: ValueKind::Real, ops: NUMERIC },
    FieldDef { field: "focal_length", label: "Focal length", group: GROUPS[0], kind: ValueKind::Real, ops: NUMERIC },
    FieldDef { field: "shutter_speed", label: "Shutter speed", group: GROUPS[0], kind: ValueKind::Text, ops: TEXT },
    FieldDef { field: "rating", label: "Rating", group: GROUPS[1], kind: ValueKind::Int, ops: NUMERIC },
    FieldDef {
        field: "color_label",
        label: "Color label",
        group: GROUPS[1],
        kind: ValueKind::Enum,
        ops: &[Op::Is, Op::IsNot, Op::IsSet],
    },
    FieldDef { field: "pick_state", label: "Pick state", group: GROUPS[1], kind: ValueKind::Enum, ops: &[Op::Is, Op::IsNot] },
    FieldDef {
        field: "capture_time",
        label: "Capture date",
        group: GROUPS[2],
        kind: ValueKind::Date,
        ops: &[Op::Before, Op::After, Op::Between],
    },
    FieldDef { field: "tag", label: "Tag", group: GROUPS[3], kind: ValueKind::Tag, ops: &[Op::Under, Op::Is] },
    FieldDef { field: "batch", label: "Import batch", group: GROUPS[3], kind: ValueKind::Batch, ops: &[Op::Is] },
    FieldDef { field: "flag", label: "Flag", group: GROUPS[3], kind: ValueKind::Enum, ops: &[Op::Is] },
];

/// The field menu's groups, in order.
pub const GROUPS: [&str; 4] = ["Capture settings", "Culling", "Date", "Tags / batch / flags"];

pub fn field_def(field: &str) -> Option<&'static FieldDef> {
    FIELDS.iter().find(|f| f.field == field)
}

/// An enum field's closed value set, `(stored value, label)`.
pub fn enum_values(field: &str) -> Vec<(&'static str, &'static str)> {
    match field {
        // Canonical casing: the backend matches case-insensitively, but the stored value and
        // the rule agree.
        "color_label" => COLOR_LABELS.iter().map(|l| (l.name, l.name)).collect(),
        "pick_state" => vec![("none", "none"), ("pick", "pick"), ("reject", "reject")],
        "flag" => vec![("has-gps", "has GPS"), ("monochrome", "monochrome"), ("is-raw", "is RAW")],
        _ => Vec::new(),
    }
}

/// One condition row as the editor holds it: the raw text of its one or two values
/// (`between` uses both; a tag or batch holds its id as text).
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub field: &'static str,
    pub op: Op,
    pub value: [String; 2],
}

impl Condition {
    /// A fresh row on `field`: its first operator and that operator's default value.
    pub fn new(field: &'static FieldDef) -> Self {
        let op = field.ops[0];
        Condition { field: field.field, op, value: default_value(field, op) }
    }

    pub fn def(&self) -> &'static FieldDef {
        field_def(self.field).expect("a condition names a known field")
    }

    /// Change the field: its first operator, a fresh value (React's `setField`).
    pub fn set_field(&mut self, field: &'static FieldDef) {
        *self = Condition::new(field);
    }

    /// Change the operator: a fresh value (React's `setOp`). An operator the field does not
    /// offer is ignored.
    pub fn set_op(&mut self, op: Op) {
        let def = self.def();
        if def.ops.contains(&op) {
            self.op = op;
            self.value = default_value(def, op);
        }
    }

    /// Whether the row has every value its operator needs — what gates its place in the
    /// rule (an incomplete row is left out of the JSON, as in React).
    pub fn complete(&self) -> bool {
        let def = self.def();
        if !def.ops.contains(&self.op) {
            return false;
        }
        match self.op {
            Op::IsSet => true,
            Op::Between => self.value.iter().all(|v| !v.trim().is_empty()) && self.coerce_all().is_some(),
            _ => match def.kind {
                ValueKind::Tag | ValueKind::Batch => self.value[0].trim().parse::<i64>().is_ok_and(|n| n > 0),
                _ => !self.value[0].trim().is_empty() && coerce(def.kind, &self.value[0]).is_some(),
            },
        }
    }

    fn coerce_all(&self) -> Option<Value> {
        let kind = self.def().kind;
        Some(json!([coerce(kind, &self.value[0])?, coerce(kind, &self.value[1])?]))
    }

    /// The contract's `{ field, op, value }`, or `None` while incomplete.
    pub fn to_json(&self) -> Option<Value> {
        if !self.complete() {
            return None;
        }
        let value = match self.op {
            Op::IsSet => Value::Null,
            Op::Between => self.coerce_all()?,
            _ => coerce(self.def().kind, &self.value[0])?,
        };
        Some(json!({ "field": self.field, "op": self.op.wire(), "value": value }))
    }
}

/// A sensible empty value for a freshly chosen `(field, op)`: an enum starts on its first
/// value, everything else empty.
fn default_value(def: &FieldDef, op: Op) -> [String; 2] {
    if def.kind == ValueKind::Enum && op != Op::IsSet {
        let first = enum_values(def.field).first().map(|(v, _)| v.to_string()).unwrap_or_default();
        return [first, String::new()];
    }
    [String::new(), String::new()]
}

/// One raw value as the JSON type the backend expects for `kind`; `None` when it does not
/// parse (React sent `NaN` — `null` on the wire — which the backend then rejected).
fn coerce(kind: ValueKind, raw: &str) -> Option<Value> {
    let raw = raw.trim();
    match kind {
        ValueKind::Int => raw.parse::<i64>().ok().map(Value::from),
        ValueKind::Real => raw.parse::<f64>().ok().filter(|v| v.is_finite()).map(Value::from),
        ValueKind::Tag | ValueKind::Batch => raw.parse::<i64>().ok().map(Value::from),
        ValueKind::Text | ValueKind::Enum | ValueKind::Date => Some(Value::from(raw)),
    }
}

/// The Rule JSON for `conditions`, leaving out incomplete rows. No conditions match every
/// photo.
pub fn build_rule_json(conditions: &[Condition]) -> String {
    let conds: Vec<Value> = conditions.iter().filter_map(Condition::to_json).collect();
    json!({ "match": "all", "conditions": conds }).to_string()
}

/// An existing album's rule back into editable rows. Unknown fields and operators are
/// dropped, so a forward-compatible rule still opens; unparseable JSON opens empty.
pub fn parse_rule_json(rule: &str) -> Vec<Condition> {
    let Ok(parsed) = serde_json::from_str::<Value>(rule) else { return Vec::new() };
    let Some(conds) = parsed.get("conditions").and_then(Value::as_array) else { return Vec::new() };
    conds
        .iter()
        .filter_map(|c| {
            let def = field_def(c.get("field")?.as_str()?)?;
            let op = Op::from_wire(c.get("op")?.as_str()?).filter(|op| def.ops.contains(op))?;
            let text = |v: &Value| match v {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            let value = match (op, c.get("value")) {
                (Op::IsSet, _) | (_, None) => [String::new(), String::new()],
                (_, Some(Value::Array(pair))) => {
                    [pair.first().map(text).unwrap_or_default(), pair.get(1).map(text).unwrap_or_default()]
                }
                (_, Some(v)) => [text(v), String::new()],
            };
            Some(Condition { field: def.field, op, value })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(field: &str, op: Op, a: &str, b: &str) -> Condition {
        Condition { field: field_def(field).unwrap().field, op, value: [a.into(), b.into()] }
    }

    #[test]
    fn every_fields_operators_are_ones_the_backend_accepts() {
        for f in FIELDS {
            assert!(!f.ops.is_empty(), "{}", f.field);
            assert!(GROUPS.contains(&f.group));
            if f.kind == ValueKind::Enum {
                assert!(!enum_values(f.field).is_empty(), "{}", f.field);
            }
        }
    }

    #[test]
    fn a_new_row_takes_the_fields_first_operator_and_an_enum_its_first_value() {
        let c = Condition::new(field_def("pick_state").unwrap());
        assert_eq!((c.op, c.value[0].as_str()), (Op::Is, "none"));
        let c = Condition::new(field_def("iso").unwrap());
        assert_eq!((c.op, c.value[0].as_str()), (Op::Eq, ""));
        assert!(!c.complete());
    }

    #[test]
    fn changing_the_field_or_operator_resets_the_value() {
        let mut c = row("iso", Op::Eq, "400", "");
        c.set_op(Op::Between);
        assert_eq!(c.value, [String::new(), String::new()]);
        c.set_op(Op::Contains); // not an ISO operator
        assert_eq!(c.op, Op::Between);
        c.set_field(field_def("color_label").unwrap());
        assert_eq!((c.field, c.op, c.value[0].as_str()), ("color_label", Op::Is, "Red"));
    }

    #[test]
    fn values_serialize_as_the_type_their_field_needs() {
        let rule = build_rule_json(&[
            row("rating", Op::Gte, "4", ""),
            row("aperture", Op::Between, "1.4", "2.8"),
            row("lens", Op::Contains, " 24-70 ", ""),
            row("color_label", Op::IsSet, "", ""),
            row("tag", Op::Under, "42", ""),
            row("capture_time", Op::After, "2026-01-01", ""),
        ]);
        let v: Value = serde_json::from_str(&rule).unwrap();
        assert_eq!(
            v,
            json!({ "match": "all", "conditions": [
                { "field": "rating", "op": "gte", "value": 4 },
                { "field": "aperture", "op": "between", "value": [1.4, 2.8] },
                { "field": "lens", "op": "contains", "value": "24-70" },
                { "field": "color_label", "op": "isSet", "value": null },
                { "field": "tag", "op": "under", "value": 42 },
                { "field": "capture_time", "op": "after", "value": "2026-01-01" },
            ]})
        );
    }

    #[test]
    fn incomplete_rows_are_left_out_of_the_rule() {
        let rule = build_rule_json(&[
            row("iso", Op::Eq, "", ""),
            row("iso", Op::Between, "100", ""),
            row("rating", Op::Eq, "four", ""),
            row("tag", Op::Is, "", ""),
            row("batch", Op::Is, "0", ""),
        ]);
        let v: Value = serde_json::from_str(&rule).unwrap();
        assert_eq!(v, json!({ "match": "all", "conditions": [] }));
    }

    #[test]
    fn a_rule_round_trips_through_the_editor_rows() {
        let rows = vec![
            row("rating", Op::Gte, "4", ""),
            row("focal_length", Op::Between, "24", "70"),
            row("pick_state", Op::IsNot, "reject", ""),
            row("camera_make", Op::IsSet, "", ""),
            row("batch", Op::Is, "7", ""),
        ];
        // `camera_make isSet` is not on offer for text fields: left out like an incomplete row.
        let rule = build_rule_json(&rows);
        let back = parse_rule_json(&rule);
        assert_eq!(back.len(), 4);
        assert_eq!((back[1].field, back[1].op), ("focal_length", Op::Between));
        assert_eq!(build_rule_json(&back), rule);
    }

    #[test]
    fn unknown_fields_operators_and_bad_json_are_dropped() {
        let rule = r#"{"match":"all","conditions":[
            {"field":"nope","op":"is","value":1},
            {"field":"rating","op":"contains","value":1},
            {"field":"rating","op":"lte","value":2}]}"#;
        assert_eq!(parse_rule_json(rule), [row("rating", Op::Lte, "2", "")]);
        assert!(parse_rule_json("not json").is_empty());
    }
}
