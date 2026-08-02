import { useEffect, useMemo, useRef, useState } from "react";
import type { TagWithCount } from "../modules/api";
import { TagCreateModal } from "./TagCreateModal";

// The id of the tag currently being dragged. A module-level variable (not React
// state) so drag handlers read it synchronously — state updates from onDragStart
// aren't guaranteed to be visible to the onDragOver that must call preventDefault().
let draggedTagId: number | null = null;

// Left sidebar: the hierarchical tag tree with photo counts. Clicking a tag filters
// the grid to photos under it (incl. descendants). Tags are draggable: drop one onto
// another tag to reparent it there, or onto "All photos" to move it to the top level.
export function TagPanel({
  tags,
  activeTagId,
  onSelectTag,
  onEditTag,
  onMoveTag,
  onSetPrivate,
  onTagsChanged,
}: {
  tags: TagWithCount[];
  activeTagId: number | null;
  onSelectTag: (tagId: number | null) => void;
  onEditTag: (tag: TagWithCount) => void;
  onMoveTag: (tagId: number, newParentId: number | null) => void;
  onSetPrivate: (tagId: number, isPrivate: boolean, recursive: boolean) => void;
  onTagsChanged?: () => void;
}) {
  const [overId, setOverId] = useState<number | "root" | null>(null);
  // ids of collapsed parents — their descendants are hidden from the list
  const [collapsed, setCollapsed] = useState<Set<number>>(new Set());
  // right-click context menu: which tag, and where to anchor it
  const [menu, setMenu] = useState<{ tag: TagWithCount; x: number; y: number } | null>(null);
  // the tag whose new parent is being picked (null = picker closed)
  const [moving, setMoving] = useState<TagWithCount | null>(null);
  const [moveFilter, setMoveFilter] = useState("");
  // "New tags" create modal: open flag + optional parentPath for child creation
  const [createModal, setCreateModal] = useState<{ open: boolean; parentPath: string | null }>({
    open: false,
    parentPath: null,
  });
  // Quick-search box above the tree: query, highlighted suggestion, and the tag row
  // to scroll into view after a pick (deferred so expanded ancestors render first).
  const [search, setSearch] = useState("");
  const [searchHl, setSearchHl] = useState(0);
  const [scrollToId, setScrollToId] = useState<number | null>(null);
  const rootRef = useRef<HTMLElement | null>(null);

  // id -> tag, the set of ids that have children, and a parent -> children index
  const { byId, parentIds, childrenOf } = useMemo(() => {
    const byId = new Map<number, TagWithCount>();
    const parentIds = new Set<number>();
    const childrenOf = new Map<number, number[]>();
    for (const t of tags) {
      byId.set(t.id, t);
      if (t.parentId != null) {
        parentIds.add(t.parentId);
        const arr = childrenOf.get(t.parentId);
        if (arr) arr.push(t.id);
        else childrenOf.set(t.parentId, [t.id]);
      }
    }
    return { byId, parentIds, childrenOf };
  }, [tags]);

  // Autocomplete for the search box. Same ranking as the inspector's add-tag box:
  // deepest (most specific) match first, then name-prefix matches, then path order.
  const searchQuery = search.trim().toLowerCase();
  const searchMatches = useMemo(() => {
    if (!searchQuery) return [];
    const depth = (t: TagWithCount) => t.fullPath.split("/").length;
    return tags
      .filter(
        (t) =>
          t.fullPath.toLowerCase().includes(searchQuery) ||
          t.name.toLowerCase().includes(searchQuery),
      )
      .sort((a, b) => {
        const byDepth = depth(b) - depth(a);
        if (byDepth) return byDepth;
        const aStarts = a.name.toLowerCase().startsWith(searchQuery) ? 0 : 1;
        const bStarts = b.name.toLowerCase().startsWith(searchQuery) ? 0 : 1;
        if (aStarts !== bStarts) return aStarts - bStarts;
        return a.fullPath.localeCompare(b.fullPath);
      })
      .slice(0, 8);
  }, [searchQuery, tags]);
  const clampedSearchHl = Math.min(searchHl, Math.max(0, searchMatches.length - 1));

  // Picking a search result selects the tag (filters the grid), expands any collapsed
  // ancestors so its row is visible in the tree, and scrolls it into view.
  const pickSearch = (tag: TagWithCount) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      let p = tag.parentId;
      while (p != null) {
        next.delete(p);
        p = byId.get(p)?.parentId ?? null;
      }
      return next;
    });
    onSelectTag(tag.id);
    setSearch("");
    setSearchHl(0);
    setScrollToId(tag.id);
  };

  useEffect(() => {
    if (scrollToId == null) return;
    rootRef.current
      ?.querySelector(`[data-tag-row="${scrollToId}"]`)
      ?.scrollIntoView({ block: "center" });
    setScrollToId(null);
  }, [scrollToId]);

  // A tag can't be moved into itself or any of its descendants (move_tag rejects it).
  const invalidTargets = useMemo(() => {
    const blocked = new Set<number>();
    if (!moving) return blocked;
    const stack = [moving.id];
    while (stack.length) {
      const id = stack.pop()!;
      blocked.add(id);
      for (const c of childrenOf.get(id) ?? []) stack.push(c);
    }
    return blocked;
  }, [moving, childrenOf]);

  // Close the context menu on any outside click or Escape.
  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setMenu(null);
    window.addEventListener("click", close);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("keydown", onKey);
    };
  }, [menu]);

  const moveCandidates = useMemo(() => {
    if (!moving) return [];
    const q = moveFilter.trim().toLowerCase();
    return tags.filter(
      (t) =>
        !invalidTargets.has(t.id) &&
        t.id !== moving.parentId &&
        (q === "" || t.fullPath.toLowerCase().includes(q)),
    );
  }, [moving, moveFilter, tags, invalidTargets]);

  const doMove = (newParentId: number | null) => {
    if (moving && newParentId !== moving.parentId) onMoveTag(moving.id, newParentId);
    setMoving(null);
    setMoveFilter("");
  };

  // Hidden if any ancestor is collapsed.
  const isHidden = (tag: TagWithCount) => {
    let p = tag.parentId;
    while (p != null) {
      if (collapsed.has(p)) return true;
      p = byId.get(p)?.parentId ?? null;
    }
    return false;
  };

  const toggleCollapse = (id: number) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const allCollapsed = collapsed.size > 0;
  const setAll = (collapse: boolean) =>
    setCollapsed(collapse ? new Set(parentIds) : new Set());

  const allowDrop = (e: React.DragEvent, over: number | "root") => {
    if (draggedTagId == null) return;
    e.preventDefault(); // required for onDrop to fire
    e.dataTransfer.dropEffect = "move";
    if (over !== draggedTagId) setOverId(over);
  };

  const handleDrop = (e: React.DragEvent, parentId: number | null) => {
    e.preventDefault();
    const src = draggedTagId;
    draggedTagId = null;
    setOverId(null);
    if (src != null && src !== parentId) onMoveTag(src, parentId);
  };

  return (
    <aside className="panel tag-panel" ref={rootRef}>
      <div className="panel-header">
        <span className="panel-head">Tags</span>
        <button
          className="panel-add"
          title="New tags…"
          onClick={() => setCreateModal({ open: true, parentPath: null })}
        >
          ＋
        </button>
        {parentIds.size > 0 && (
          <button
            className="panel-add"
            title={allCollapsed ? "Expand all" : "Collapse all"}
            onClick={() => setAll(!allCollapsed)}
          >
            {allCollapsed ? "⊕" : "⊖"}
          </button>
        )}
      </div>
      <div className="tag-add tag-panel-search">
        <input
          className="tag-input"
          type="search"
          placeholder="Search tags…"
          value={search}
          onChange={(e) => {
            setSearch(e.target.value);
            setSearchHl(0);
          }}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown" && searchMatches.length) {
              e.preventDefault();
              setSearchHl((h) => Math.min(h + 1, searchMatches.length - 1));
            } else if (e.key === "ArrowUp" && searchMatches.length) {
              e.preventDefault();
              setSearchHl((h) => Math.max(h - 1, 0));
            } else if (e.key === "Enter" && searchMatches.length) {
              e.preventDefault();
              pickSearch(searchMatches[clampedSearchHl]);
            } else if (e.key === "Escape") {
              setSearch("");
              setSearchHl(0);
            }
          }}
        />
        {searchMatches.length > 0 && (
          <ul className="tag-suggest">
            {searchMatches.map((s, i) => {
              const cut = s.fullPath.length - s.name.length;
              return (
                <li key={s.id}>
                  <button
                    className={`tag-suggest-item ${i === clampedSearchHl ? "tag-suggest-on" : ""}`}
                    onMouseEnter={() => setSearchHl(i)}
                    onClick={() => pickSearch(s)}
                  >
                    <span className="tag-suggest-anc">{s.fullPath.slice(0, cut)}</span>
                    <span className="tag-suggest-leaf">{s.name}</span>
                    <span className="tag-suggest-count">{s.photoCount}</span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
        {searchQuery && searchMatches.length === 0 && (
          <div className="panel-empty">No matching tag</div>
        )}
      </div>
      <button
        className={`tag-row tag-filterrow ${activeTagId === null ? "tag-active" : ""} ${
          overId === "root" ? "tag-drop" : ""
        }`}
        onClick={() => onSelectTag(null)}
        onDragOver={(e) => allowDrop(e, "root")}
        onDragLeave={() => setOverId((o) => (o === "root" ? null : o))}
        onDrop={(e) => handleDrop(e, null)}
      >
        <span className="tag-name">All photos</span>
      </button>
      {tags.map((tag) => {
        if (isHidden(tag)) return null;
        const depth = tag.fullPath.split("/").length - 1;
        const hasChildren = parentIds.has(tag.id);
        const isCollapsed = collapsed.has(tag.id);
        return (
          <div
            key={tag.id}
            data-tag-row={tag.id}
            className={`tag-row ${tag.id === activeTagId ? "tag-active" : ""} ${
              overId === tag.id ? "tag-drop" : ""
            }`}
            style={{ paddingLeft: 12 + depth * 14 }}
            draggable
            onDragStart={(e) => {
              draggedTagId = tag.id;
              e.dataTransfer.effectAllowed = "move";
              // Some WebKitGTK versions require drag data for `drop` to fire.
              e.dataTransfer.setData("text/plain", String(tag.id));
            }}
            onDragEnd={() => {
              draggedTagId = null;
              setOverId(null);
            }}
            onDragOver={(e) => allowDrop(e, tag.id)}
            onDragLeave={() => setOverId((o) => (o === tag.id ? null : o))}
            onDrop={(e) => handleDrop(e, tag.id)}
            onContextMenu={(e) => {
              e.preventDefault();
              setMenu({ tag, x: e.clientX, y: e.clientY });
            }}
          >
            {hasChildren ? (
              <button
                className="tag-twisty"
                title={isCollapsed ? "Expand" : "Collapse"}
                onClick={(e) => {
                  e.stopPropagation();
                  toggleCollapse(tag.id);
                }}
              >
                {isCollapsed ? "▸" : "▾"}
              </button>
            ) : (
              <span className="tag-twisty tag-twisty-empty" />
            )}
            <button className="tag-filter" onClick={() => onSelectTag(tag.id)}>
              <span className="tag-name">{tag.name}</span>
              {tag.private && (
                <span
                  className="tag-private"
                  title="Private — hidden from external/cloud AI (local AI still uses it)"
                >
                  🔒
                </span>
              )}
              {tag.autoRule && (
                <span className="tag-auto" title={`auto-tag (${tag.autoRule})`}>
                  auto
                </span>
              )}
            </button>
            <span className="tag-count">{tag.photoCount}</span>
            <button
              className="tag-edit"
              title="Edit translations & synonyms"
              onClick={() => onEditTag(tag)}
            >
              ⚙
            </button>
          </div>
        );
      })}
      {tags.length === 0 && <div className="panel-empty">No tags yet</div>}

      {menu && (
        <div
          className="tag-menu"
          style={{ left: menu.x, top: menu.y }}
          // keep clicks inside from bubbling to the window close-handler
          onClick={(e) => e.stopPropagation()}
        >
          <div className="tag-menu-head">{menu.tag.name}</div>
          <button
            className="tag-menu-item"
            onClick={() => {
              setMoving(menu.tag);
              setMoveFilter("");
              setMenu(null);
            }}
          >
            Move to…
          </button>
          <button
            className="tag-menu-item"
            disabled={menu.tag.parentId == null}
            onClick={() => {
              onMoveTag(menu.tag.id, null);
              setMenu(null);
            }}
          >
            Move to top level
          </button>
          <div className="tag-menu-sep" />
          <button
            className="tag-menu-item"
            onClick={() => {
              onSetPrivate(menu.tag.id, !menu.tag.private, false);
              setMenu(null);
            }}
            title="Private tags are withheld from external/cloud AI; the local model still uses them."
          >
            {menu.tag.private ? "Make public (cloud AI)" : "Make private (hide from cloud AI)"}
          </button>
          {parentIds.has(menu.tag.id) && (
            <>
              <button
                className="tag-menu-item"
                onClick={() => {
                  onSetPrivate(menu.tag.id, true, true);
                  setMenu(null);
                }}
              >
                Make private incl. sub-tags
              </button>
              <button
                className="tag-menu-item"
                onClick={() => {
                  onSetPrivate(menu.tag.id, false, true);
                  setMenu(null);
                }}
              >
                Make public incl. sub-tags
              </button>
            </>
          )}
          <div className="tag-menu-sep" />
          <button
            className="tag-menu-item"
            onClick={() => {
              onEditTag(menu.tag);
              setMenu(null);
            }}
          >
            Edit…
          </button>
          <button
            className="tag-menu-item"
            onClick={() => {
              setCreateModal({ open: true, parentPath: menu.tag.fullPath });
              setMenu(null);
            }}
          >
            New child tags…
          </button>
        </div>
      )}

      {createModal.open && (
        <TagCreateModal
          parentPath={createModal.parentPath}
          onClose={() => setCreateModal({ open: false, parentPath: null })}
          onChanged={() => {
            setCreateModal({ open: false, parentPath: null });
            onTagsChanged?.();
          }}
        />
      )}

      {moving && (
        <div className="modal-backdrop" onClick={() => doMove(moving.parentId ?? null)}>
          <div
            className="modal tag-move-modal"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="modal-header">
              <div className="modal-headinfo">
                <div className="modal-title">Move “{moving.name}”</div>
                <div className="modal-sub">
                  Currently: {moving.fullPath}
                </div>
              </div>
              <button className="chip" onClick={() => setMoving(null)}>
                Cancel
              </button>
            </div>
            <div className="modal-body">
              <input
                className="tag-input"
                autoFocus
                placeholder="Filter parents…"
                value={moveFilter}
                onChange={(e) => setMoveFilter(e.target.value)}
              />
              <div className="tag-move-list">
                <button
                  className="tag-move-option"
                  disabled={moving.parentId == null}
                  onClick={() => doMove(null)}
                >
                  ↑ Top level
                </button>
                {moveCandidates.map((t) => (
                  <button
                    key={t.id}
                    className="tag-move-option"
                    onClick={() => doMove(t.id)}
                  >
                    {t.fullPath}
                  </button>
                ))}
                {moveCandidates.length === 0 && (
                  <div className="panel-empty">No matching parent</div>
                )}
              </div>
            </div>
          </div>
        </div>
      )}
    </aside>
  );
}
