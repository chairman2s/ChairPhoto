import { useEffect, useState } from "react";
import { getGroupMembers, listTagGroups, recentlyUsedTags, TagGroup } from "../modules/api";
import type { Tag } from "../modules/registry";

// Sentinel id for the virtual "Recently used" group — an auto group whose members are
// the tags you most recently applied by hand (no stored membership; always current).
const RECENT_ID = -1;
const RECENT_GROUP: TagGroup = { id: RECENT_ID, name: "Recently used" };

// Bottom bar for fast tagging: pick a tag group, then click its tag buttons to
// apply them to the current selection. Groups are user-defined (see TagGroupsManager),
// plus the always-present "Recently used" auto group.
export function QuickTagBar({
  reloadKey,
  selectionCount,
  onAssign,
  onManage,
}: {
  reloadKey: number; // bump to refetch after the manager changes groups or a tag is applied
  selectionCount: number;
  onAssign: (tagId: number) => void;
  onManage: () => void;
}) {
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
    load.then(setMembers).catch(() => setMembers([]));
  }, [activeId, reloadKey]);

  const targetLabel = selectionCount > 1 ? ` → ${selectionCount} selected` : "";
  const isRecent = activeId === RECENT_ID;

  return (
    <div className="quicktag">
      <div className="quicktag-groups">
        {groups.map((g) => (
          <button
            key={g.id}
            className={`chip ${g.id === activeId ? "chip-on" : ""}`}
            onClick={() => setActiveId(g.id)}
          >
            {g.name}
          </button>
        ))}
        <button className="chip quicktag-manage" onClick={onManage} title="Create/edit tag groups">
          ⚙ groups
        </button>
      </div>
      <div className="quicktag-tags">
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
            className="chip quicktag-tag"
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
