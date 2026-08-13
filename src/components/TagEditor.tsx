import { useCallback, useEffect, useState } from "react";
import {
  addTagTerm,
  deleteTag,
  getTagExportable,
  listLanguages,
  listTagTerms,
  removeTagTerm,
  renameTag,
  setTagDescription,
  setTagExportable,
  setTermExport,
  tagExportPreview,
  TagTerm,
} from "../modules/api";
import { panelsForSlot, useHostContributions } from "../modules/host";
import { ModuleContent } from "../modules/ModuleContent";

// Modal for managing a tag's taxonomy: per-language translations and synonyms,
// each synonym with its own export checkbox, plus a live preview of what would be
// exported for a chosen set of languages. The canonical tag name is the neutral
// default; translations and synonyms layer on top.
export function TagEditor({
  tagId,
  tagName,
  tagPath,
  tagDescription,
  onClose,
  onChanged,
}: {
  tagId: number;
  tagName: string;
  tagPath: string;
  tagDescription: string;
  onClose: () => void;
  onChanged: () => void;
}) {
  const [terms, setTerms] = useState<TagTerm[]>([]);
  const [languages, setLanguages] = useState<string[]>([]);
  // "" represents the default (canonical) name; start with it selected.
  const [previewLangs, setPreviewLangs] = useState<string[]>([""]);
  const [preview, setPreview] = useState<string[]>([]);
  const [description, setDescription] = useState(tagDescription);
  const [name, setName] = useState(tagName);
  const [exportable, setExportable] = useState(true);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [error, setError] = useState("");

  useHostContributions(); // re-render when modules contribute/remove tag-editor panels

  const [trLang, setTrLang] = useState("");
  const [trText, setTrText] = useState("");
  const [synText, setSynText] = useState("");
  const [synLang, setSynLang] = useState("");
  const [synExport, setSynExport] = useState(true);

  const reload = useCallback(async () => {
    setTerms(await listTagTerms(tagId));
    setLanguages(await listLanguages());
  }, [tagId]);

  useEffect(() => {
    reload();
  }, [reload]);

  // Load the per-tag export flag when the tag changes.
  useEffect(() => {
    getTagExportable(tagId).then(setExportable).catch(() => setExportable(true));
  }, [tagId]);

  const toggleExportable = async (next: boolean) => {
    setExportable(next);
    await setTagExportable(tagId, next).catch((e) => setError(String(e)));
    onChanged(); // refresh the grid/keywords; the preview re-runs via the dep below
  };

  // Refresh the export preview whenever terms, the language selection, or the tag's
  // exportable flag change.
  useEffect(() => {
    tagExportPreview(tagId, previewLangs).then(setPreview).catch(() => setPreview([]));
  }, [tagId, previewLangs, terms, exportable]);

  const after = async () => {
    await reload();
    onChanged();
  };

  const addTranslation = async () => {
    if (!trText.trim() || !trLang.trim()) return;
    await addTagTerm(tagId, trText.trim(), trLang.trim(), true, true);
    setTrText("");
    setTrLang("");
    after();
  };

  const addSynonym = async () => {
    if (!synText.trim()) return;
    await addTagTerm(tagId, synText.trim(), synLang.trim() || null, false, synExport);
    setSynText("");
    setSynLang("");
    setSynExport(true);
    after();
  };

  const rename = async () => {
    const next = name.trim();
    if (!next || next === tagName) return;
    try {
      await renameTag(tagId, next);
      setError("");
      onChanged();
    } catch (e) {
      setError(String(e));
      setName(tagName);
    }
  };

  const doDelete = async () => {
    await deleteTag(tagId);
    onChanged();
    onClose();
  };

  const toggleLang = (lang: string) =>
    setPreviewLangs((cur) =>
      cur.includes(lang) ? cur.filter((l) => l !== lang) : [...cur, lang],
    );

  const translations = terms.filter((t) => t.isPrimary);
  const synonyms = terms.filter((t) => !t.isPrimary);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-headinfo">
            <input
              className="tag-input rename-input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              onBlur={rename}
              onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
              title="Rename tag"
            />
            <div className="modal-sub">{tagPath}</div>
            {error && <div className="modal-error">{error}</div>}
          </div>
          <div className="row">
            {confirmDelete ? (
              <button className="chip chip-danger" onClick={doDelete}>
                Confirm delete
              </button>
            ) : (
              <button className="chip" onClick={() => setConfirmDelete(true)}>
                Delete
              </button>
            )}
            <button className="chip" onClick={onClose}>
              Close
            </button>
          </div>
        </div>

        <div className="modal-body">
          {/* Description */}
          <section className="editor-section">
            <h3>Description</h3>
            <textarea
              className="tag-input description-input"
              placeholder="What this tag means — helps disambiguate shared taxonomies and gives AI tagging context. Not exported to image files."
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              onBlur={() => {
                if (description !== tagDescription) {
                  setTagDescription(tagId, description).then(onChanged);
                }
              }}
              rows={2}
            />
          </section>

          {/* Export gate */}
          <section className="editor-section">
            <h3>Export</h3>
            <label className="term-export">
              <input
                type="checkbox"
                checked={!exportable}
                onChange={(e) => toggleExportable(!e.target.checked)}
              />
              Organizational — don’t export this tag as a keyword
            </label>
            <div className="term-note">
              The tag still groups photos in your library, but it’s never written to exported
              files or hashtags. Tags nested under it still export.
            </div>
          </section>

          {/* Translations */}
          <section className="editor-section">
            <h3>Translations</h3>
            <div className="term-row term-row-static">
              <span className="term-lang">default</span>
              <span className="term-text">{tagName}</span>
              <span className="term-note">canonical name</span>
            </div>
            {translations.map((t) => (
              <div key={t.id} className="term-row">
                <span className="term-lang">{t.language}</span>
                <span className="term-text">{t.text}</span>
                <button className="tag-remove" onClick={() => removeTagTerm(t.id).then(after)}>
                  ×
                </button>
              </div>
            ))}
            <div className="term-add">
              <input
                className="tag-input lang-input"
                placeholder="lang (e.g. nb)"
                value={trLang}
                onChange={(e) => setTrLang(e.target.value)}
              />
              <input
                className="tag-input"
                placeholder="Translated name"
                value={trText}
                onChange={(e) => setTrText(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && addTranslation()}
              />
              <button className="chip" onClick={addTranslation}>
                Add
              </button>
            </div>
          </section>

          {/* Synonyms */}
          <section className="editor-section">
            <h3>Synonyms</h3>
            {synonyms.length === 0 && <div className="panel-empty">No synonyms</div>}
            {synonyms.map((t) => (
              <div key={t.id} className="term-row">
                <span className="term-lang">{t.language ?? "—"}</span>
                <span className="term-text">{t.text}</span>
                <label className="term-export" title="Include this synonym on export">
                  <input
                    type="checkbox"
                    checked={t.export}
                    onChange={(e) => setTermExport(t.id, e.target.checked).then(after)}
                  />
                  export
                </label>
                <button className="tag-remove" onClick={() => removeTagTerm(t.id).then(after)}>
                  ×
                </button>
              </div>
            ))}
            <div className="term-add">
              <input
                className="tag-input lang-input"
                placeholder="lang (opt)"
                value={synLang}
                onChange={(e) => setSynLang(e.target.value)}
              />
              <input
                className="tag-input"
                placeholder="Synonym"
                value={synText}
                onChange={(e) => setSynText(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && addSynonym()}
              />
              <label className="term-export">
                <input
                  type="checkbox"
                  checked={synExport}
                  onChange={(e) => setSynExport(e.target.checked)}
                />
                export
              </label>
              <button className="chip" onClick={addSynonym}>
                Add
              </button>
            </div>
          </section>

          {/* Export preview */}
          <section className="editor-section">
            <h3>Export preview</h3>
            <div className="row">
              <span className="term-note">Languages:</span>
              <label className="term-export">
                <input
                  type="checkbox"
                  checked={previewLangs.includes("")}
                  onChange={() => toggleLang("")}
                />
                default
              </label>
              {languages.map((lang) => (
                <label key={lang} className="term-export">
                  <input
                    type="checkbox"
                    checked={previewLangs.includes(lang)}
                    onChange={() => toggleLang(lang)}
                  />
                  {lang}
                </label>
              ))}
            </div>
            <div className="preview-labels">
              {preview.length === 0 ? (
                <span className="panel-empty">nothing to export</span>
              ) : (
                preview.map((label) => (
                  <span key={label} className="assigned-tag">
                    {label}
                  </span>
                ))
              )}
            </div>
          </section>

          {/* Sections contributed by enabled modules (e.g. the Obsidian tag note).
              The panel reads the tag being edited via getEditingTag(). */}
          {panelsForSlot("tag-editor").map((panel) => (
            <section className="editor-section" key={panel.id}>
              <h3>{panel.label}</h3>
              <ModuleContent view={panel} />
            </section>
          ))}
        </div>
      </div>
    </div>
  );
}
