import { useEffect, useState } from "react";
import {
  addTagToGroup,
  createTagGroup,
  deleteTagGroup,
  getGroupMembers,
  listTagGroups,
  removeTagFromGroup,
  renameTagGroup,
  TagGroup,
} from "../modules/api";
import type { Tag } from "../modules/registry";

// Manage custom tag groups: create/rename/delete groups and edit their member tags.
// Closing notifies the parent so the quick-tag bar refetches.
export function TagGroupsManager({ onClose }: { onClose: () => void }) {
  const [groups, setGroups] = useState<TagGroup[]>([]);
  const [activeId, setActiveId] = useState<number | null>(null);
  const [members, setMembers] = useState<Tag[]>([]);
  const [newGroup, setNewGroup] = useState("");
  const [newMember, setNewMember] = useState("");

  const reloadGroups = async (keep?: number | null) => {
    const g = await listTagGroups();
    setGroups(g);
    setActiveId((cur) => {
      const want = keep !== undefined ? keep : cur;
      return g.some((x) => x.id === want) ? want : g[0]?.id ?? null;
    });
  };

  useEffect(() => {
    reloadGroups();
  }, []);

  useEffect(() => {
    if (activeId == null) return setMembers([]);
    getGroupMembers(activeId).then(setMembers).catch(() => setMembers([]));
  }, [activeId]);

  const reloadMembers = () => {
    if (activeId != null) getGroupMembers(activeId).then(setMembers).catch(() => {});
  };

  const addGroup = async () => {
    const name = newGroup.trim();
    if (!name) return;
    const id = await createTagGroup(name);
    setNewGroup("");
    await reloadGroups(id);
  };

  const addMember = async () => {
    const path = newMember.trim();
    if (!path || activeId == null) return;
    await addTagToGroup(activeId, path);
    setNewMember("");
    await reloadMembers();
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title">Tag groups</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>
        <div className="modal-body">
          <section className="editor-section">
            <h3>Groups</h3>
            <div className="nearby-list">
              {groups.map((g) => (
                <button
                  key={g.id}
                  className={`chip ${g.id === activeId ? "chip-on" : ""}`}
                  onClick={() => setActiveId(g.id)}
                >
                  {g.name}
                </button>
              ))}
              {groups.length === 0 && <span className="panel-empty">No groups yet</span>}
            </div>
            <div className="term-add">
              <input
                className="tag-input"
                placeholder="New group name (e.g. Street photo)"
                value={newGroup}
                onChange={(e) => setNewGroup(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && addGroup()}
              />
              <button className="chip" onClick={addGroup}>
                Add group
              </button>
            </div>
          </section>

          {activeId != null && (
            <section className="editor-section">
              <h3>
                Tags in “{groups.find((g) => g.id === activeId)?.name}”
                <button
                  className="tag-remove"
                  title="Delete this group"
                  onClick={() =>
                    deleteTagGroup(activeId).then(() => reloadGroups(null))
                  }
                >
                  delete group
                </button>
              </h3>
              <div className="tag-list">
                {members.map((t) => (
                  <span key={t.id} className="assigned-tag">
                    {t.fullPath}
                    <button
                      className="tag-remove"
                      onClick={() =>
                        removeTagFromGroup(activeId, t.id).then(reloadMembers)
                      }
                    >
                      ×
                    </button>
                  </span>
                ))}
                {members.length === 0 && <span className="panel-empty">No tags in this group</span>}
              </div>
              <div className="term-add">
                <input
                  className="tag-input"
                  placeholder="Add tag (path, created if new — e.g. Street/Candid)"
                  value={newMember}
                  onChange={(e) => setNewMember(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && addMember()}
                />
                <button className="chip" onClick={addMember}>
                  +
                </button>
              </div>
              <div className="term-note">
                Rename: <input
                  className="tag-input rename-input"
                  defaultValue={groups.find((g) => g.id === activeId)?.name}
                  onBlur={(e) => renameTagGroup(activeId, e.target.value).then(() => reloadGroups())}
                />
              </div>
            </section>
          )}
        </div>
      </div>
    </div>
  );
}
