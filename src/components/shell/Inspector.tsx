// The Darkroom redesign's docked inspector column (see agent-notes mockup Main.dc.html
// `.insp`): a serif filename header with the photo's colour-label swatch and a hide
// button, a lowercase tab row — details / tags / versions / publish — and a scrolling
// body. The shell owns only the chrome; the tab *content* is handed in as `children`
// (App renders PhotoInspector with the same `tab`), so this component stays thin and
// mountable in tests without any Tauri surface.
//
// `QuickTagGroups` (below) is the tags tab's extra zone — the old bottom QuickTagBar's
// group tabs + tag buttons, now rendered under the tab content via the `quickTags` prop.
import { useEffect, useState, type ReactNode } from "react";
import type { Photo, Tag } from "../../modules/registry";
import { COLOR_LABELS } from "../../modules/labels";
import { getGroupMembers, listTagGroups, recentlyUsedTags, TagGroup } from "../../modules/api";

export type InspectorTab = "details" | "tags" | "versions" | "publish";

/** The tab row, in order — also the validation whitelist for the persisted tab. */
export const INSPECTOR_TABS: readonly InspectorTab[] = [
  "details",
  "tags",
  "versions",
  "publish",
] as const;

export interface InspectorProps {
  tab: InspectorTab;
  onTab: (t: InspectorTab) => void;
  /** Hide the whole column (App sets rightHidden; the TitleBar checkbox brings it back). */
  onHide: () => void;
  /** The photo the header names — its basename and colour-label swatch. */
  photo: Photo | null;
  /** The tab's content — App renders PhotoInspector with the same `tab`. */
  children: ReactNode;
  /** Tags-tab extra zone (the quick-tag groups); rendered under `children` only while
   *  the tags tab is active, so its fetches never run for the other tabs. */
  quickTags: ReactNode | null;
}

export function Inspector({ tab, onTab, onHide, photo, children, quickTags }: InspectorProps) {
  const filename = photo ? photo.path.split("/").pop() : null;
  // Labels are stored with canonical casing but compared case-insensitively in the
  // backend (see modules/labels.ts) — match the swatch the same lenient way.
  const label = photo?.label
    ? COLOR_LABELS.find((l) => l.name.toLowerCase() === photo.label.toLowerCase())
    : undefined;
  return (
    <aside className="insp">
      <div className="insp-head">
        <h3 title={photo?.path}>{filename ?? ""}</h3>
        {label && (
          <span
            className="insp-swatch"
            style={{ background: label.color }}
            title={`${label.name} label`}
          />
        )}
        <button
          className="insp-hide"
          onClick={onHide}
          title="Hide the inspector"
          aria-label="Hide the inspector"
        >
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <polyline points="9 6 15 12 9 18"></polyline>
          </svg>
        </button>
      </div>
      <div className="insp-tabs" role="tablist">
        {INSPECTOR_TABS.map((t) => (
          <button
            key={t}
            role="tab"
            aria-selected={t === tab}
            className={`insp-tab ${t === tab ? "on" : ""}`}
            onClick={() => onTab(t)}
          >
            {t}
          </button>
        ))}
      </div>
      <div className="insp-body">
        {children}
        {tab === "tags" && quickTags}
      </div>
    </aside>
  );
}

// ── Quick-tag groups ─────────────────────────────────────────────────────────
// The old bottom QuickTagBar, rehomed into the tags tab: pick a tag group, then click
// its tag buttons to apply them to the current selection. Groups are user-defined (see
// TagGroupsManager), plus the always-present "Recently used" auto group.

// Sentinel id for the virtual "Recently used" group — an auto group whose members are
// the tags you most recently applied by hand (no stored membership; always current).
const RECENT_ID = -1;
const RECENT_GROUP: TagGroup = { id: RECENT_ID, name: "Recently used" };

export interface QuickTagGroupsProps {
  /** Bump to refetch after the manager changes groups or a tag is applied. */
  reloadKey: number;
  selectionCount: number;
  onAssign: (tagId: number) => void;
  onManage: () => void;
}

export function QuickTagGroups({
  reloadKey,
  selectionCount,
  onAssign,
  onManage,
}: QuickTagGroupsProps) {
  const [groups, setGroups] = useState<TagGroup[]>([RECENT_GROUP]);
  const [activeId, setActiveId] = useState<number | null>(RECENT_ID);
  const [members, setMembers] = useState<Tag[]>([]);

  useEffect(() => {
    listTagGroups()
      .then((g) => {
        const all = [RECENT_GROUP, ...g];
        setGroups(all);
        setActiveId((cur) => (all.some((x) => x.id === cur) ? cur : RECENT_ID));
      })
      .catch(() => setGroups([RECENT_GROUP]));
  }, [reloadKey]);

  useEffect(() => {
    if (activeId == null) return setMembers([]);
    const load = activeId === RECENT_ID ? recentlyUsedTags(10) : getGroupMembers(activeId);
    // A group can hold an auto-tag; its button would only be refused (#181).
    load.then((m) => setMembers(m.filter((t) => !t.autoRule))).catch(() => setMembers([]));
  }, [activeId, reloadKey]);

  const targetLabel = selectionCount > 1 ? ` → ${selectionCount} selected` : "";
  const isRecent = activeId === RECENT_ID;

  return (
    <div className="qtg">
      <div className="ins-label">Quick tags</div>
      <div className="qtg-groups">
        {groups.map((g) => (
          <button
            key={g.id}
            className={`chip ${g.id === activeId ? "chip-on" : ""}`}
            onClick={() => setActiveId(g.id)}
          >
            {g.name}
          </button>
        ))}
        <button className="chip qtg-manage" onClick={onManage} title="Create/edit tag groups">
          ⚙ groups
        </button>
      </div>
      <div className="qtg-tags">
        {members.length === 0 && (
          <span className="panel-empty">
            {isRecent
              ? "No recently used tags yet — tag a photo and they’ll show here."
              : "Empty group — add tags via “⚙ groups”."}
          </span>
        )}
        {members.map((t) => (
          <button
            key={t.id}
            className="chip qtg-tag"
            title={`${t.fullPath}${targetLabel}`}
            onClick={() => onAssign(t.id)}
          >
            {t.name}
          </button>
        ))}
      </div>
    </div>
  );
}
