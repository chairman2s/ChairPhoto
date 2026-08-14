// Obsidian module: per-photo AND per-tag companion notes in an Obsidian vault,
// linked both ways — obsidian://new / obsidian://open out, and a chairphoto://<uuid>
// (photo) or chairphoto://tag/<uuid> (tag) deep link back. Pure frontend: the note is
// created by Obsidian itself via its URI scheme
// (https://obsidian.md/help/Extending+Obsidian/Obsidian+URI); no image is exported.
// The note mappings live in module-scoped settings, keyed by the subject's stable
// uuid ("obsidian.note.<uuid>" / "obsidian.tagnote.<uuid>" after host namespacing).
// See docs/obsidian.md.

import { useEffect, useState } from "react";
import type { ChairPhotoAPI, ChairPhotoModule, Photo, Tag } from "../registry";
import { listTagTerms, openExternal } from "../api";
import { useHostEditingTag, useHostSelection } from "../host";

interface NoteRecord {
  vault: string;
  /** Vault-relative note path, without the .md extension (what obsidian://open wants). */
  file: string;
  createdAt: number;
}

const noteKey = (p: Photo) => `note.${p.uuid}`;
// Tag notes are keyed by the tag's stable uuid (not name/path), so the link
// survives renames and moves — and matches how taxonomies merge across machines.
const tagNoteKey = (t: Tag) => `tagnote.${t.uuid}`;

/** Obsidian tags can't contain spaces — CamelCase each word within a path segment,
 *  keeping the `/` hierarchy (Obsidian nests tags on `/`): "Street Photography/Old
 *  Town" → "StreetPhotography/OldTown". */
function obsidianTag(fullPath: string): string {
  return fullPath
    .split("/")
    .map((segment) =>
      segment
        .split(/\s+/)
        .filter(Boolean)
        .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
        .join(""),
    )
    .join("/");
}

/** Deterministic note name: capture date + filename stem + uuid prefix — readable,
 *  and collision-proof across cards that reuse DSC-numbers. */
function noteName(p: Photo): string {
  const date = (p.captureTime ?? "").slice(0, 10) || "undated";
  const stem = (p.path.split("/").pop() ?? String(p.id)).replace(/\.[^.]+$/, "");
  return `${date} ${stem} ${p.uuid.slice(0, 8)}`;
}

/** The initial note body. Kept compact — the whole thing is URL-encoded into the
 *  obsidian://new URI — and null fields are omitted rather than written empty. */
function noteContent(p: Photo, tags: Tag[]): string {
  const exposure = [
    p.aperture != null ? `f/${p.aperture}` : null,
    p.shutterSpeed ? `${p.shutterSpeed}s` : null,
    p.iso != null ? `ISO ${p.iso}` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  const lines = [
    "---",
    "type: ChairPhoto",
    `chairphoto: ${p.uuid}`,
    `photo: ${p.path.split("/").pop()}`,
    p.captureTime ? `captured: ${p.captureTime}` : null,
    p.cameraModel ? `camera: ${p.cameraModel}` : null,
    p.lens ? `lens: ${p.lens}` : null,
    exposure ? `exposure: ${exposure}` : null,
    p.rating > 0 ? `rating: ${p.rating}` : null,
  ].filter(Boolean) as string[];
  if (tags.length) {
    lines.push("tags:");
    // URI-length insurance; a photo rarely carries anywhere near this many.
    for (const t of tags.slice(0, 20)) lines.push(`  - ${obsidianTag(t.fullPath)}`);
  }
  lines.push(
    "---",
    "",
    `[Open in ChairPhoto](chairphoto://${p.uuid}) · [Open in loupe](chairphoto://${p.uuid}/loupe)`,
    "",
  );
  return lines.join("\n");
}

/** Tag-note file name: the tag's leaf name (filesystem-safe) + uuid prefix, so
 *  same-named tags under different parents never collide. */
function tagNoteName(t: Tag): string {
  return `${t.name.replace(/[\\/:*?"<>|]/g, "-")} ${t.uuid.slice(0, 8)}`;
}

/** The initial tag-note body. The frontmatter carries the tag's stable uuid and
 *  full path plus every taxonomy term as an alias — machine-readable groundwork
 *  for the future taxonomy service; the body below is free-form explanation. */
function tagNoteContent(t: Tag, aliases: string[]): string {
  const lines = [
    "---",
    "type: ChairPhotoTag",
    `chairphoto-tag: ${t.uuid}`,
    `tag: ${t.fullPath}`,
  ];
  if (aliases.length) {
    lines.push("aliases:");
    for (const a of aliases.slice(0, 20)) lines.push(`  - ${a}`);
  }
  lines.push("tags:", `  - ${obsidianTag(t.fullPath)}`);
  lines.push(
    "---",
    "",
    `[Show photos in ChairPhoto](chairphoto://tag/${t.uuid})`,
    "",
  );
  if (t.description.trim()) lines.push(t.description.trim(), "");
  return lines.join("\n");
}

/** "Note" section inside the tag editor modal: a companion note for the TAG
 *  (a detailed explanation of what it means), linked both ways like photo notes. */
function ObsidianTagPanel({ api }: { api: ChairPhotoAPI }) {
  useHostEditingTag(); // re-render when the editing tag changes
  const tag = api.getEditingTag();
  const [note, setNote] = useState<NoteRecord | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!tag) {
      setNote(null);
      return;
    }
    let alive = true;
    api
      .getSetting(tagNoteKey(tag))
      .then((raw) => alive && setNote(raw ? (JSON.parse(raw) as NoteRecord) : null))
      .catch(() => alive && setNote(null));
    return () => {
      alive = false;
    };
  }, [tag?.uuid]);

  if (!tag) return null;

  const createNote = async () => {
    const vault = ((await api.getSetting("vault")) ?? "").trim();
    if (!vault) {
      api.showToast("Set the Obsidian vault name in Preferences → Modules → Obsidian");
      return;
    }
    const folder = ((await api.getSetting("folder")) ?? "").trim() || "ChairPhoto";
    setBusy(true);
    try {
      // Translations + synonyms become note aliases, so the note is findable in
      // Obsidian by any term the taxonomy knows.
      const terms = await listTagTerms(tag.id).catch(() => []);
      const aliases = [...new Set(terms.map((t) => t.text.trim()).filter(Boolean))];
      const file = `${folder}/Tags/${tagNoteName(tag)}`;
      const uri =
        `obsidian://new?vault=${encodeURIComponent(vault)}` +
        `&file=${encodeURIComponent(file)}` +
        `&content=${encodeURIComponent(tagNoteContent(tag, aliases))}`;
      await openExternal(uri);
      const rec: NoteRecord = { vault, file, createdAt: Date.now() };
      await api.setSetting(tagNoteKey(tag), JSON.stringify(rec));
      setNote(rec);
    } catch (e) {
      api.showToast(`Couldn't create note: ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const openNote = () => {
    if (!note) return;
    openExternal(
      `obsidian://open?vault=${encodeURIComponent(note.vault)}&file=${encodeURIComponent(note.file)}`,
    ).catch((e) => api.showToast(`Couldn't open note: ${e}`));
  };

  const forgetNote = async () => {
    await api.setSetting(tagNoteKey(tag), "");
    setNote(null);
  };

  if (note) {
    return (
      <div className="obsidian-panel">
        <div className="ai-sug-reason" title={note.file}>
          {note.file.split("/").pop()}
        </div>
        <div className="iptc-actions">
          <button className="chip chip-on" onClick={openNote}>
            Open note
          </button>
          <button
            className="chip"
            onClick={forgetNote}
            title="Forget the link only — the note stays in your vault"
          >
            Forget
          </button>
        </div>
      </div>
    );
  }
  return (
    <div className="obsidian-panel">
      <div className="term-note">
        A companion note for this tag — a place for a detailed explanation of what it
        means, linked both ways.
      </div>
      <div className="iptc-actions">
        <button className="chip chip-on" onClick={createNote} disabled={busy}>
          {busy ? "Creating…" : "Create note in Obsidian"}
        </button>
      </div>
    </div>
  );
}

function ObsidianPanel({ api }: { api: ChairPhotoAPI }) {
  useHostSelection(); // re-render when the active photo changes
  const photoId = api.getActivePhotoId();
  const [photo, setPhoto] = useState<Photo | null>(null);
  const [note, setNote] = useState<NoteRecord | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    // get_photo (not getSelectedPhotos) so off-grid stack children work too.
    if (photoId == null) {
      setPhoto(null);
      setNote(null);
      return;
    }
    let alive = true;
    api
      .invoke<Photo>("get_photo", { photoId })
      .then(async (p) => {
        if (!alive) return;
        setPhoto(p);
        const raw = await api.getSetting(noteKey(p));
        if (alive) setNote(raw ? (JSON.parse(raw) as NoteRecord) : null);
      })
      .catch(() => {
        if (alive) {
          setPhoto(null);
          setNote(null);
        }
      });
    return () => {
      alive = false;
    };
  }, [photoId]);

  const createNote = async () => {
    if (!photo) return;
    const vault = ((await api.getSetting("vault")) ?? "").trim();
    if (!vault) {
      api.showToast("Set the Obsidian vault name in Preferences → Modules → Obsidian");
      return;
    }
    const folder = ((await api.getSetting("folder")) ?? "").trim() || "ChairPhoto";
    setBusy(true);
    try {
      const tags = await api.invoke<Tag[]>("get_photo_tags", { photoId: photo.id });
      const file = `${folder}/${noteName(photo)}`;
      const uri =
        `obsidian://new?vault=${encodeURIComponent(vault)}` +
        `&file=${encodeURIComponent(file)}` +
        `&content=${encodeURIComponent(noteContent(photo, tags))}`;
      await openExternal(uri);
      const rec: NoteRecord = { vault, file, createdAt: Date.now() };
      await api.setSetting(noteKey(photo), JSON.stringify(rec));
      setNote(rec);
    } catch (e) {
      api.showToast(`Couldn't create note: ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const openNote = () => {
    if (!note) return;
    openExternal(
      `obsidian://open?vault=${encodeURIComponent(note.vault)}&file=${encodeURIComponent(note.file)}`,
    ).catch((e) => api.showToast(`Couldn't open note: ${e}`));
  };

  // Forget the mapping only — the note itself stays in the vault. There is no
  // delete_setting command; an empty value means "no note".
  const forgetNote = async () => {
    if (!photo) return;
    await api.setSetting(noteKey(photo), "");
    setNote(null);
  };

  if (!photo) return <div className="ai-sug-reason">Select a photo.</div>;
  if (note) {
    return (
      <div className="obsidian-panel">
        <div className="ai-sug-reason" title={note.file}>
          {note.file.split("/").pop()}
        </div>
        <div className="iptc-actions">
          <button className="chip chip-on" onClick={openNote}>
            Open note
          </button>
          <button
            className="chip"
            onClick={forgetNote}
            title="Forget the link only — the note stays in your vault"
          >
            Forget
          </button>
        </div>
      </div>
    );
  }
  return (
    <div className="obsidian-panel">
      <div className="iptc-actions">
        <button className="chip chip-on" onClick={createNote} disabled={busy}>
          {busy ? "Creating…" : "Create note in Obsidian"}
        </button>
      </div>
    </div>
  );
}

function ObsidianSettings({ api }: { api: ChairPhotoAPI }) {
  const [vault, setVault] = useState("");
  const [folder, setFolder] = useState("");

  useEffect(() => {
    api.getSetting("vault").then((v) => setVault(v ?? "")).catch(() => {});
    api.getSetting("folder").then((v) => setFolder(v ?? "")).catch(() => {});
  }, []);

  const save = async () => {
    await api.setSetting("vault", vault.trim());
    await api.setSetting("folder", folder.trim());
    api.showToast("Obsidian settings saved");
  };

  return (
    <div className="iptc-panel">
      <div className="field">
        <label>Vault name</label>
        <input
          className="folder-input"
          value={vault}
          placeholder="My vault"
          onChange={(e) => setVault(e.target.value)}
        />
      </div>
      <div className="field">
        <label>Notes folder (inside the vault)</label>
        <input
          className="folder-input"
          value={folder}
          placeholder="ChairPhoto"
          onChange={(e) => setFolder(e.target.value)}
        />
      </div>
      <div className="iptc-actions">
        <button className="chip chip-on" onClick={save}>
          Save
        </button>
      </div>
    </div>
  );
}

export const obsidianModule: ChairPhotoModule = {
  id: "obsidian",
  name: "Obsidian",
  version: "0.1.0",
  description:
    "Per-photo and per-tag notes in an Obsidian vault, linked both ways (obsidian:// out, chairphoto:// back).",
  // Read-only: the note's front matter needs the photo row and its tags (#48).
  permissions: { commands: ["get_photo", "get_photo_tags"] },
  onLoad(api) {
    api.registerPanel({
      id: "obsidian-note",
      label: "Note",
      slot: "inspector",
      render: () => <ObsidianPanel api={api} />,
    });
    api.registerPanel({
      id: "obsidian-tag-note",
      label: "Obsidian note",
      slot: "tag-editor",
      render: () => <ObsidianTagPanel api={api} />,
    });
    api.registerSettingsPanel(() => <ObsidianSettings api={api} />);
  },
};
