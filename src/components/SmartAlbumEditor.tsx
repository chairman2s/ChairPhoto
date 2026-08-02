import { useEffect, useMemo, useRef, useState } from "react";
import {
  ImportBatch,
  SmartAlbum,
  TagWithCount,
  createSmartAlbum,
  listImportBatches,
  listTags,
  renameSmartAlbum,
  setSmartAlbumRule,
  smartAlbumCount,
} from "../modules/api";
import { COLOR_LABELS as CANONICAL_LABELS } from "../modules/labels";

// Rule builder modal (C2d) — see docs/smart-albums.md. Builds the Rule JSON contract:
// { match: "all", conditions: [{ field, op, value }, …] } (AND-only in v1). The editor
// round-trips an existing album's ruleJson and previews the live match count as it changes.
// All evaluation/SQL lives in Rust; here we only assemble the JSON the backend understands.

// --- the field/operator matrix (mirrors the table in docs/smart-albums.md) ----------------

/** How a field's value is entered, which drives both the value widget and the op list. */
type ValueKind =
  | "int" // rating, iso
  | "real" // aperture, focal_length
  | "text" // camera_make/model, lens, shutter_speed, color_label
  | "enum" // pick_state, color_label, flag (closed value set)
  | "date" // capture_time
  | "tag" // tag id (tag picker)
  | "batch"; // import_batch_id (batch dropdown)

type Op =
  | "eq"
  | "gte"
  | "lte"
  | "between"
  | "is"
  | "isNot"
  | "isSet"
  | "contains"
  | "before"
  | "after"
  | "under";

interface FieldDef {
  field: string;
  label: string;
  group: string;
  kind: ValueKind;
  ops: Op[];
  /** Closed value set for enum fields (and the "tag" exact/under distinction is in ops). */
  enumValues?: { value: string; label: string }[];
}

const OP_LABELS: Record<Op, string> = {
  eq: "is",
  gte: "≥",
  lte: "≤",
  between: "between",
  is: "is",
  isNot: "is not",
  isSet: "is set",
  contains: "contains",
  before: "before",
  after: "after",
  under: "under (incl. children)",
};

// Canonical color-label values (shared vocabulary) — matching is case-insensitive in
// the backend, but the canonical casing keeps the stored value and the rule agreeing.
const COLOR_LABELS = CANONICAL_LABELS.map((l) => l.name);

// Order here is the order rows render their field <optgroup>s in.
const FIELDS: FieldDef[] = [
  // Capture settings
  { field: "camera_make", label: "Camera make", group: "Capture settings", kind: "text", ops: ["is", "contains"] },
  { field: "camera_model", label: "Camera model", group: "Capture settings", kind: "text", ops: ["is", "contains"] },
  { field: "lens", label: "Lens", group: "Capture settings", kind: "text", ops: ["is", "contains"] },
  { field: "iso", label: "ISO", group: "Capture settings", kind: "int", ops: ["eq", "gte", "lte", "between"] },
  { field: "aperture", label: "Aperture", group: "Capture settings", kind: "real", ops: ["eq", "gte", "lte", "between"] },
  { field: "focal_length", label: "Focal length", group: "Capture settings", kind: "real", ops: ["eq", "gte", "lte", "between"] },
  { field: "shutter_speed", label: "Shutter speed", group: "Capture settings", kind: "text", ops: ["is", "contains"] },
  // Culling
  { field: "rating", label: "Rating", group: "Culling", kind: "int", ops: ["eq", "gte", "lte", "between"] },
  {
    field: "color_label",
    label: "Color label",
    group: "Culling",
    kind: "enum",
    ops: ["is", "isNot", "isSet"],
    enumValues: COLOR_LABELS.map((c) => ({ value: c, label: c })),
  },
  {
    field: "pick_state",
    label: "Pick state",
    group: "Culling",
    kind: "enum",
    ops: ["is", "isNot"],
    enumValues: [
      { value: "none", label: "none" },
      { value: "pick", label: "pick" },
      { value: "reject", label: "reject" },
    ],
  },
  // Date
  { field: "capture_time", label: "Capture date", group: "Date", kind: "date", ops: ["before", "after", "between"] },
  // Tags / batch / flags
  { field: "tag", label: "Tag", group: "Tags / batch / flags", kind: "tag", ops: ["under", "is"] },
  { field: "batch", label: "Import batch", group: "Tags / batch / flags", kind: "batch", ops: ["is"] },
  {
    field: "flag",
    label: "Flag",
    group: "Tags / batch / flags",
    kind: "enum",
    ops: ["is"],
    enumValues: [
      { value: "has-gps", label: "has GPS" },
      { value: "monochrome", label: "monochrome" },
      { value: "is-raw", label: "is RAW" },
    ],
  },
];

const FIELD_GROUPS = ["Capture settings", "Culling", "Date", "Tags / batch / flags"];

function fieldDef(field: string): FieldDef | undefined {
  return FIELDS.find((f) => f.field === field);
}

/** A condition value carries everything any widget might need; we serialize the right
 *  slice per (field, op) when we emit the rule JSON. */
type RuleValue = number | string | (number | string)[];

interface Condition {
  field: string;
  op: Op;
  value: RuleValue;
}

// --- value defaults & serialization -------------------------------------------------------

/** A sensible empty value for a freshly chosen (field, op). */
function defaultValue(def: FieldDef, op: Op): RuleValue {
  if (op === "isSet") return ""; // no value used
  if (op === "between") return def.kind === "date" ? ["", ""] : ["", ""];
  if (def.kind === "enum") return def.enumValues?.[0]?.value ?? "";
  return "";
}

/** True once every condition has the value(s) its op needs — used to gate Save & live count. */
function conditionComplete(c: Condition): boolean {
  const def = fieldDef(c.field);
  if (!def) return false;
  if (c.op === "isSet") return true;
  if (c.op === "between") {
    return Array.isArray(c.value) && c.value.length === 2 && c.value.every((v) => `${v}`.trim() !== "");
  }
  if (def.kind === "tag" || def.kind === "batch") return Number(c.value) > 0;
  return `${c.value}`.trim() !== "";
}

/** Coerce a single raw value to the JSON type the backend expects for this field kind. */
function coerce(def: FieldDef, raw: number | string): number | string {
  if (def.kind === "int") return parseInt(`${raw}`, 10);
  if (def.kind === "real") return Number(raw);
  if (def.kind === "tag" || def.kind === "batch") return Number(raw);
  return `${raw}`;
}

/** Serialize one condition to the contract { field, op, value }. */
function toRuleCondition(c: Condition): { field: string; op: string; value: unknown } {
  const def = fieldDef(c.field)!;
  let value: unknown;
  if (c.op === "isSet") {
    value = null;
  } else if (c.op === "between" && Array.isArray(c.value)) {
    value = [coerce(def, c.value[0]), coerce(def, c.value[1])];
  } else {
    value = coerce(def, c.value as number | string);
  }
  return { field: c.field, op: c.op, value };
}

/** Build the full Rule JSON string from the current (complete) conditions. */
function buildRuleJson(conditions: Condition[]): string {
  return JSON.stringify({
    match: "all",
    conditions: conditions.filter(conditionComplete).map(toRuleCondition),
  });
}

/** Parse an existing album's ruleJson back into editable rows (round-trip). Unknown
 *  fields/ops are dropped silently so a forward-compat rule still opens. */
function parseRuleJson(json: string): Condition[] {
  try {
    const parsed = JSON.parse(json);
    const conds = Array.isArray(parsed?.conditions) ? parsed.conditions : [];
    const out: Condition[] = [];
    for (const c of conds) {
      const def = fieldDef(c?.field);
      if (!def || !def.ops.includes(c?.op)) continue;
      let value: RuleValue;
      if (c.op === "isSet") value = "";
      else if (Array.isArray(c.value)) value = c.value.map((v: unknown) => `${v}`);
      else value = c.value == null ? "" : `${c.value}`;
      out.push({ field: def.field, op: c.op, value });
    }
    return out;
  } catch {
    return [];
  }
}

// --- the modal ----------------------------------------------------------------------------

export function SmartAlbumEditor({
  album,
  onClose,
  onSaved,
}: {
  /** The album being edited, or null to create a new one. */
  album: SmartAlbum | null;
  onClose: () => void;
  /** Called with the saved album's id after a successful create/update. */
  onSaved: (id: number) => void;
}) {
  const [name, setName] = useState(album?.name ?? "");
  const [conditions, setConditions] = useState<Condition[]>(() =>
    album ? parseRuleJson(album.ruleJson) : [],
  );
  const [tags, setTags] = useState<TagWithCount[]>([]);
  const [batches, setBatches] = useState<ImportBatch[]>([]);
  const [count, setCount] = useState<number | null>(null);
  const [counting, setCounting] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    listTags().then(setTags).catch(() => {});
    listImportBatches().then(setBatches).catch(() => {});
  }, []);

  // The rule JSON for the current rows (memoized so the live-count effect is stable).
  const ruleJson = useMemo(() => buildRuleJson(conditions), [conditions]);

  // Live match count, debounced — recomputed as the rule changes. An empty rule matches all.
  const debounce = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (debounce.current) clearTimeout(debounce.current);
    setCounting(true);
    let alive = true;
    debounce.current = setTimeout(() => {
      smartAlbumCount(ruleJson)
        .then((n) => alive && setCount(n))
        .catch(() => alive && setCount(null))
        .finally(() => alive && setCounting(false));
    }, 300);
    return () => {
      alive = false;
      if (debounce.current) clearTimeout(debounce.current);
    };
  }, [ruleJson]);

  const addRow = () => {
    const def = FIELDS[0];
    setConditions((cs) => [...cs, { field: def.field, op: def.ops[0], value: defaultValue(def, def.ops[0]) }]);
  };

  const removeRow = (i: number) => {
    setConditions((cs) => cs.filter((_, idx) => idx !== i));
  };

  const setField = (i: number, field: string) => {
    const def = fieldDef(field)!;
    const op = def.ops[0];
    setConditions((cs) => cs.map((c, idx) => (idx === i ? { field, op, value: defaultValue(def, op) } : c)));
  };

  const setOp = (i: number, op: Op) => {
    setConditions((cs) =>
      cs.map((c, idx) => {
        if (idx !== i) return c;
        const def = fieldDef(c.field)!;
        return { ...c, op, value: defaultValue(def, op) };
      }),
    );
  };

  const setValue = (i: number, value: RuleValue) => {
    setConditions((cs) => cs.map((c, idx) => (idx === i ? { ...c, value } : c)));
  };

  const setBetween = (i: number, which: 0 | 1, v: string) => {
    setConditions((cs) =>
      cs.map((c, idx) => {
        if (idx !== i) return c;
        const pair = Array.isArray(c.value) ? [...c.value] : ["", ""];
        pair[which] = v;
        return { ...c, value: pair };
      }),
    );
  };

  const save = async () => {
    setError("");
    const trimmed = name.trim();
    if (!trimmed) {
      setError("Give the smart album a name.");
      return;
    }
    setBusy(true);
    try {
      let id: number;
      if (album) {
        await setSmartAlbumRule(album.id, ruleJson);
        if (trimmed !== album.name) await renameSmartAlbum(album.id, trimmed);
        id = album.id;
      } else {
        id = await createSmartAlbum(trimmed, ruleJson);
      }
      onSaved(id);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal sa-editor" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title">{album ? "Edit smart album" : "New smart album"}</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>
        <div className="modal-body">
          <div className="field">
            <label>Name</label>
            <input
              className="folder-input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. Keepers ≥4★ this month"
              autoFocus={!album}
            />
          </div>

          <div className="field">
            <label>Conditions — all must match (AND)</label>
            {conditions.length === 0 && (
              <p className="sa-hint">No conditions — this matches every photo. Add one to narrow it.</p>
            )}
            {conditions.map((c, i) => {
              const def = fieldDef(c.field)!;
              return (
                <div className="sa-row" key={i}>
                  <select
                    className="sa-select"
                    value={c.field}
                    onChange={(e) => setField(i, e.target.value)}
                  >
                    {FIELD_GROUPS.map((g) => (
                      <optgroup key={g} label={g}>
                        {FIELDS.filter((f) => f.group === g).map((f) => (
                          <option key={f.field} value={f.field}>
                            {f.label}
                          </option>
                        ))}
                      </optgroup>
                    ))}
                  </select>

                  <select
                    className="sa-select sa-op"
                    value={c.op}
                    onChange={(e) => setOp(i, e.target.value as Op)}
                  >
                    {def.ops.map((op) => (
                      <option key={op} value={op}>
                        {OP_LABELS[op]}
                      </option>
                    ))}
                  </select>

                  <div className="sa-value">{renderValue(c, i)}</div>

                  <button className="sa-remove" title="Remove condition" onClick={() => removeRow(i)}>
                    ✕
                  </button>
                </div>
              );
            })}
            <button className="chip sa-add" onClick={addRow}>
              ＋ Add condition
            </button>
          </div>

          <div className="row sa-footer">
            <span className="sa-count">
              {counting ? "Counting…" : count == null ? "—" : `${count} photo${count === 1 ? "" : "s"} match`}
            </span>
            <button className="scan-btn" onClick={save} disabled={busy}>
              {busy ? "Saving…" : album ? "Save rule" : "Create"}
            </button>
          </div>
          {error && <div className="modal-error">{error}</div>}
        </div>
      </div>
    </div>
  );

  // Per-type value widget. Defined as a closure so it can reach tags/batches/setters.
  function renderValue(c: Condition, i: number) {
    const def = fieldDef(c.field)!;

    if (c.op === "isSet") {
      return <span className="term-note">(any value)</span>;
    }

    if (c.op === "between") {
      const pair = Array.isArray(c.value) ? c.value : ["", ""];
      const type = def.kind === "date" ? "date" : "number";
      const step = def.kind === "real" ? "0.1" : undefined;
      return (
        <div className="sa-between">
          <input
            className="sa-input"
            type={type}
            step={step}
            value={`${pair[0] ?? ""}`}
            onChange={(e) => setBetween(i, 0, e.target.value)}
          />
          <span className="sa-and">and</span>
          <input
            className="sa-input"
            type={type}
            step={step}
            value={`${pair[1] ?? ""}`}
            onChange={(e) => setBetween(i, 1, e.target.value)}
          />
        </div>
      );
    }

    if (def.kind === "tag") {
      // Tag picker — reuse the catalog's tag list, shown by full hierarchical path so the
      // user can tell apart same-named leaves. The value is the tag id (bound param).
      const sorted = [...tags].sort((a, b) => a.fullPath.localeCompare(b.fullPath));
      return (
        <select
          className="sa-input"
          value={`${c.value || ""}`}
          onChange={(e) => setValue(i, e.target.value)}
        >
          <option value="">Pick a tag…</option>
          {sorted.map((t) => (
            <option key={t.id} value={t.id}>
              {t.fullPath} ({t.photoCount})
            </option>
          ))}
        </select>
      );
    }

    if (def.kind === "batch") {
      return (
        <select
          className="sa-input"
          value={`${c.value || ""}`}
          onChange={(e) => setValue(i, e.target.value)}
        >
          <option value="">Pick a batch…</option>
          {batches.map((b) => (
            <option key={b.id} value={b.id}>
              {b.sourceLabel} ({b.photoCount})
            </option>
          ))}
        </select>
      );
    }

    if (def.kind === "enum") {
      return (
        <select
          className="sa-input"
          value={`${c.value || ""}`}
          onChange={(e) => setValue(i, e.target.value)}
        >
          {def.enumValues!.map((ev) => (
            <option key={ev.value} value={ev.value}>
              {ev.label}
            </option>
          ))}
        </select>
      );
    }

    if (def.kind === "date") {
      return (
        <input
          className="sa-input"
          type="date"
          value={`${c.value || ""}`}
          onChange={(e) => setValue(i, e.target.value)}
        />
      );
    }

    // int / real / text
    const type = def.kind === "text" ? "text" : "number";
    const step = def.kind === "real" ? "0.1" : undefined;
    return (
      <input
        className="sa-input"
        type={type}
        step={step}
        value={`${c.value ?? ""}`}
        onChange={(e) => setValue(i, e.target.value)}
        placeholder={def.kind === "text" ? "value" : undefined}
      />
    );
  }
}
