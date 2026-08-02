import { useEffect, useState } from "react";
import { getIptc, IptcFields, setIptc } from "../modules/api";

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
      await setIptc(photoId, fields);
      setSaved(fields);
      setStatus("Saved to sidecar");
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
