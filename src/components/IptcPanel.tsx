import { useEffect, useState } from "react";
import { getIptc, IptcFields, IptcSaveOutcome, setIptc } from "../modules/api";

const EMPTY: IptcFields = {
  description: "",
  headline: "",
  title: "",
  creator: "",
  copyright: "",
  credit: "",
  source: "",
  city: "",
  state: "",
  country: "",
  countryCode: "",
};

// Text fields rendered as single-line inputs (description is a textarea).
const FIELDS: { key: keyof IptcFields; label: string }[] = [
  { key: "headline", label: "Headline" },
  { key: "title", label: "Title" },
  { key: "creator", label: "Creator" },
  { key: "copyright", label: "Copyright" },
  { key: "credit", label: "Credit" },
  { key: "source", label: "Source" },
  { key: "city", label: "City" },
  { key: "state", label: "State/Province" },
  { key: "country", label: "Country" },
  { key: "countryCode", label: "Country code" },
];

/** The status line for a save, matching the GPUI inspector's (`IptcSaveOutcome::status`).
 *  Never claims a sidecar write that did not happen (#148). Pure, exported for tests. */
export function iptcSaveStatus(outcome: IptcSaveOutcome | null | undefined): string {
  switch (outcome?.sidecar) {
    case "written":
      return "Saved to sidecar";
    case "unchanged":
      return "Saved — the sidecar already had these values";
    case "pending":
      return outcome.reason
        ? `Saved to catalog; sidecar pending (${outcome.reason})`
        : "Saved to catalog; sidecar pending";
    default:
      // A backend that answers nothing (older than #148) said nothing about the sidecar.
      return "Saved to catalog";
  }
}

// Authored IPTC Core fields for a photo. Saving writes to the catalog AND the XMP
// sidecar (merge-safe), so other apps and exports see the values.
export function IptcPanel({ photoId }: { photoId: number }) {
  const [fields, setFields] = useState<IptcFields>(EMPTY);
  const [saved, setSaved] = useState<IptcFields>(EMPTY);
  const [status, setStatus] = useState("");

  useEffect(() => {
    getIptc(photoId)
      .then((f) => {
        setFields(f);
        setSaved(f);
        setStatus("");
      })
      .catch(() => {
        setFields(EMPTY);
        setSaved(EMPTY);
      });
  }, [photoId]);

  const dirty = JSON.stringify(fields) !== JSON.stringify(saved);

  const update = (key: keyof IptcFields, value: string) =>
    setFields((f) => ({ ...f, [key]: value }));

  const save = async () => {
    setStatus("Saving…");
    try {
      const outcome = await setIptc(photoId, fields);
      // The catalog has the values whatever became of the sidecar: they are the new baseline.
      setSaved(fields);
      setStatus(iptcSaveStatus(outcome));
    } catch (e) {
      setStatus(`Failed: ${e}`);
    }
  };

  return (
    <div className="iptc">
      <label className="iptc-label">Caption / Description</label>
      <textarea
        className="tag-input iptc-caption"
        rows={2}
        value={fields.description}
        onChange={(e) => update("description", e.target.value)}
      />
      {FIELDS.map(({ key, label }) => (
        <div key={key} className="iptc-row">
          <label className="iptc-label">{label}</label>
          <input
            className="tag-input"
            value={fields[key]}
            onChange={(e) => update(key, e.target.value)}
          />
        </div>
      ))}
      <div className="iptc-actions">
        <button className="chip chip-on" onClick={save} disabled={!dirty}>
          Save IPTC
        </button>
        <span className="iptc-status">{status}</span>
      </div>
    </div>
  );
}
