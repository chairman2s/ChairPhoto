//! The tag tree's logic, as the React tag components kept it inline: the parent/child index
//! and collapse visibility (`TagPanel.tsx`), the "deepest match first" tag search shared by
//! the panel's search box and the inspector's add-tag box (`TagPanel.tsx`,
//! `PhotoInspector.tsx`), the Move and Merge target lists (`TagPanel.tsx`,
//! `TagMergeModal.tsx`), and the status-line and preview wording of a merge or split
//! (`TagPanel.tsx`'s `mergeSummary`, `TagMergeModal.tsx`'s `MergePreview`,
//! `TagSplitModal.tsx`'s `splitSummary`).
//!
//! Semantic choices against the TypeScript:
//! - The last search tie-break was `a.fullPath.localeCompare(b.fullPath)`. Rust has no ICU
//!   collator, so [`locale_cmp`] approximates it: case-insensitive first (so `bergen` and
//!   `Bergen` sort together, as ICU's primary strength does), then by code point. Paths that
//!   differ only by accents may order differently from WebKit's collator; the order is only
//!   ever a tie-break between equally deep, equally prefixed matches.
//! - `toLowerCase()` is [`str::to_lowercase`]; both use Unicode's default case mapping.
//! - `toLocaleString()` on a count is [`grouped`]: en-US grouping with commas, which is what
//!   the React app rendered on this machine.

use chairphoto_core::catalog::tag_maintenance::{TagMergeReport, TagSplitReport};
use chairphoto_core::catalog::TagWithCount;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// How many matches the search boxes offer.
pub const SEARCH_LIMIT: usize = 8;
/// How many targets the Merge dialog lists.
pub const MERGE_CANDIDATE_LIMIT: usize = 200;

/// `fullPath.split("/").length`: 1 for a top-level tag.
pub fn depth(path: &str) -> usize {
    path.split('/').count()
}

/// The leaf's ancestors as the suggestion rows print them: `fullPath` minus `name`
/// (`"Place/Norway/"` for `Place/Norway/Bergen`).
pub fn ancestor_prefix(full_path: &str, name: &str) -> String {
    full_path[..full_path.len().saturating_sub(name.len())].to_string()
}

/// `a.localeCompare(b)`, approximately — see the module docs.
pub fn locale_cmp(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b))
}

/// The tag list's parent/child index (`TagPanel.tsx`'s `byId`/`parentIds`/`childrenOf`).
#[derive(Debug, Clone, Default)]
pub struct TagIndex {
    parent_of: HashMap<i64, Option<i64>>,
    children_of: HashMap<i64, Vec<i64>>,
}

impl TagIndex {
    pub fn new(tags: &[TagWithCount]) -> Self {
        let mut index = TagIndex::default();
        for t in tags {
            index.parent_of.insert(t.tag.id, t.tag.parent_id);
            if let Some(p) = t.tag.parent_id {
                index.children_of.entry(p).or_default().push(t.tag.id);
            }
        }
        index
    }

    /// Whether any tag names `id` as its parent (the row gets a twisty).
    pub fn has_children(&self, id: i64) -> bool {
        self.children_of.contains_key(&id)
    }

    /// Every tag that has children — what "Collapse all" collapses.
    pub fn parent_ids(&self) -> HashSet<i64> {
        self.children_of.keys().copied().collect()
    }

    /// `id`'s ancestors, nearest first. A corrupt parent cycle stops rather than spins.
    pub fn ancestors(&self, id: i64) -> Vec<i64> {
        let mut out = Vec::new();
        let mut cur = self.parent_of.get(&id).copied().flatten();
        while let Some(p) = cur {
            if out.contains(&p) || out.len() > self.parent_of.len() {
                break;
            }
            out.push(p);
            cur = self.parent_of.get(&p).copied().flatten();
        }
        out
    }

    /// A row is hidden when any ancestor is collapsed.
    pub fn is_hidden(&self, id: i64, collapsed: &HashSet<i64>) -> bool {
        self.ancestors(id).iter().any(|a| collapsed.contains(a))
    }

    /// `id` and every descendant — the targets a move of `id` must refuse (`move_tag` rejects
    /// moving a tag into itself or below itself).
    pub fn subtree(&self, id: i64) -> HashSet<i64> {
        let mut out = HashSet::new();
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            if out.insert(cur) {
                stack.extend(self.children_of.get(&cur).into_iter().flatten().copied());
            }
        }
        out
    }

    /// Whether a drag of `dragged` may drop onto `target` (`None` = "All photos", the top
    /// level): not onto itself or its own subtree, and not where it already is.
    pub fn can_drop(&self, dragged: i64, target: Option<i64>) -> bool {
        let current = self.parent_of.get(&dragged).copied().flatten();
        match target {
            Some(t) => !self.subtree(dragged).contains(&t) && current != Some(t),
            None => current.is_some(),
        }
    }
}

/// The search boxes' matches: tags whose path or name contains `query` (trimmed,
/// case-insensitive), minus `exclude`, ranked **deepest first** (the most specific tag
/// before its ancestors), then names that start with the query, then path order; at most
/// [`SEARCH_LIMIT`]. An empty query matches nothing.
pub fn search<'a>(tags: &'a [TagWithCount], query: &str, exclude: &HashSet<i64>) -> Vec<&'a TagWithCount> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<&TagWithCount> = tags
        .iter()
        .filter(|t| {
            !exclude.contains(&t.tag.id)
                && (t.tag.full_path.to_lowercase().contains(&q) || t.tag.name.to_lowercase().contains(&q))
        })
        .collect();
    hits.sort_by(|a, b| {
        depth(&b.tag.full_path)
            .cmp(&depth(&a.tag.full_path))
            .then_with(|| {
                let starts = |t: &TagWithCount| !t.tag.name.to_lowercase().starts_with(&q);
                starts(a).cmp(&starts(b))
            })
            .then_with(|| locale_cmp(&a.tag.full_path, &b.tag.full_path))
    });
    hits.truncate(SEARCH_LIMIT);
    hits
}

/// The Move dialog's parents for `moving`: every tag outside its subtree, except its
/// current parent, whose path contains `filter` (trimmed, case-insensitive).
pub fn move_candidates<'a>(tags: &'a [TagWithCount], index: &TagIndex, moving: &TagWithCount, filter: &str) -> Vec<&'a TagWithCount> {
    let blocked = index.subtree(moving.tag.id);
    let q = filter.trim().to_lowercase();
    tags.iter()
        .filter(|t| {
            !blocked.contains(&t.tag.id)
                && Some(t.tag.id) != moving.tag.parent_id
                && (q.is_empty() || t.tag.full_path.to_lowercase().contains(&q))
        })
        .collect()
}

/// The Merge dialog's targets for `source`: every tag but itself and its subtree (by path
/// prefix, as the dialog did), filtered like [`move_candidates`], at most
/// [`MERGE_CANDIDATE_LIMIT`].
pub fn merge_candidates<'a>(tags: &'a [TagWithCount], source: &TagWithCount, filter: &str) -> Vec<&'a TagWithCount> {
    let prefix = format!("{}/", source.tag.full_path);
    let q = filter.trim().to_lowercase();
    tags.iter()
        .filter(|t| t.tag.id != source.tag.id && !t.tag.full_path.starts_with(&prefix))
        .filter(|t| q.is_empty() || t.tag.full_path.to_lowercase().contains(&q))
        .take(MERGE_CANDIDATE_LIMIT)
        .collect()
}

/// `n.toLocaleString()` in en-US: `1234567` → `"1,234,567"`.
pub fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn s(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// A committed merge, in one status line, counting only what happened (`mergeSummary`).
pub fn merge_summary(r: &TagMergeReport) -> String {
    let source = r.sources.iter().map(|s| s.path.as_str()).collect::<Vec<_>>().join(", ");
    let source = if source.is_empty() { "tag".to_string() } else { source };
    let mut parts = Vec::new();
    if r.photos_retagged > 0 {
        parts.push(format!("{} photo{} moved", grouped(r.photos_retagged), s(r.photos_retagged)));
    }
    if r.assignments_collapsed > 0 {
        parts.push(format!("{} already tagged", grouped(r.assignments_collapsed)));
    }
    if r.children_reparented > 0 {
        parts.push(format!("{} child tag{} moved", r.children_reparented, s(r.children_reparented)));
    }
    if !r.smart_albums_rewritten.is_empty() {
        parts.push(format!("{} smart album rule(s) rewritten", r.smart_albums_rewritten.len()));
    }
    let detail = if parts.is_empty() { String::new() } else { format!(" — {}", parts.join(", ")) };
    format!("Merged {source} into {}{detail}.", r.target_path)
}

/// The dry run, in words (`MergePreview`): one line per thing the merge touched. Empty
/// means the tag is empty and merging only removes it. `None` plugin counters (compiled
/// out) and zeros print nothing.
pub fn merge_preview_lines(r: &TagMergeReport) -> Vec<String> {
    let mut lines = Vec::new();
    if r.photos_retagged > 0 {
        lines.push(format!("{} photos gain {}", grouped(r.photos_retagged), r.target_path));
    }
    if r.assignments_collapsed > 0 {
        lines.push(format!("{} already had it — two assignments become one", grouped(r.assignments_collapsed)));
    }
    if r.children_reparented > 0 {
        let mut line = format!("{} child tag{} move across", r.children_reparented, s(r.children_reparented));
        if r.descendants_repathed > 0 {
            line += &format!(
                ", and {} tag path{} are rewritten",
                r.descendants_repathed,
                s(r.descendants_repathed)
            );
        }
        lines.push(line);
    }
    if r.terms_moved > 0 {
        lines.push(format!("{} term(s) move", r.terms_moved));
    }
    if r.synonyms_moved > 0 {
        lines.push(format!("{} synonym(s) move", r.synonyms_moved));
    }
    if r.groups_repointed > 0 {
        lines.push(format!("{} quick-tag group membership(s) repoint", r.groups_repointed));
    }
    if !r.smart_albums_rewritten.is_empty() {
        lines.push(format!("Smart album rules rewritten: {}", r.smart_albums_rewritten.join(", ")));
    }
    let some = |n: Option<usize>| n.filter(|&n| n > 0);
    if let Some(n) = some(r.faces_repointed) {
        lines.push(format!("{n} face(s) stay named"));
    }
    if let Some(n) = some(r.face_rejections_repointed) {
        lines.push(format!("{n} face rejection(s) follow"));
    }
    if let Some(n) = some(r.classifiers_dropped) {
        lines.push(format!("{n} Smart Tagging classifier(s) dropped — retrained from the merged set"));
    }
    if let Some(n) = some(r.suggestions_repointed) {
        lines.push(format!("{n} pending suggestion(s) follow the merge"));
    }
    if r.aliases_recorded > 0 {
        lines.push(format!(
            "{} tombstone(s) recorded, so importing an old bundle cannot bring the tag back",
            r.aliases_recorded
        ));
    }
    lines
}

/// The split's dry run, one line each (`TagSplitModal.tsx`'s preview list).
pub fn split_preview_lines(r: &TagSplitReport) -> Vec<String> {
    let mut lines = vec![format!(
        "{} photo{} gain {}{}",
        grouped(r.photos_moved),
        s(r.photos_moved),
        r.new_tag_path,
        if r.created_new_tag { " (a new tag)" } else { "" }
    )];
    if r.photos_untagged > 0 {
        lines.push(format!("{} lose {}", grouped(r.photos_untagged), r.source_path));
    }
    if r.photos_already_tagged > 0 {
        lines.push(format!("{} already had the new tag", grouped(r.photos_already_tagged)));
    }
    if r.photos_without_source > 0 {
        lines.push(format!(
            "{} of the selection never carried {} — left alone",
            grouped(r.photos_without_source),
            r.source_path
        ));
    }
    lines
}

/// A committed split, in one status line (`splitSummary`).
pub fn split_summary(r: &TagSplitReport) -> String {
    let mut parts =
        vec![format!("{} photo{} moved to {}", grouped(r.photos_moved), s(r.photos_moved), r.new_tag_path)];
    if r.photos_already_tagged > 0 {
        parts.push(format!("{} already had it", r.photos_already_tagged));
    }
    if r.photos_without_source > 0 {
        parts.push(format!("{} never carried {}", r.photos_without_source, r.source_path));
    }
    format!("{}.", parts.join(" — "))
}

/// The status line after Make private/public (App.tsx's `onSetPrivate`).
pub fn privacy_summary(changed: usize, private: bool) -> String {
    format!("{changed} tag{} marked {}.", s(changed), if private { "private" } else { "public" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chairphoto_core::catalog::tag_maintenance::MergedSource;
    use chairphoto_core::catalog::Tag;

    fn tag(id: i64, path: &str, parent: Option<i64>, count: i64) -> TagWithCount {
        TagWithCount {
            tag: Tag {
                id,
                uuid: format!("u{id}"),
                name: path.rsplit('/').next().unwrap().to_string(),
                full_path: path.to_string(),
                parent_id: parent,
                description: String::new(),
                auto_rule: None,
                private: false,
            },
            photo_count: count,
        }
    }

    /// Place, Place/Norway, Place/Norway/Bergen, People, People/Bergen Berg, Bergensbanen.
    fn tree() -> Vec<TagWithCount> {
        vec![
            tag(5, "Bergensbanen", None, 1),
            tag(3, "People", None, 0),
            tag(4, "People/Bergen Berg", Some(3), 2),
            tag(1, "Place", None, 9),
            tag(2, "Place/Norway", Some(1), 9),
            tag(6, "Place/Norway/Bergen", Some(2), 7),
        ]
    }

    fn ids(v: &[&TagWithCount]) -> Vec<i64> {
        v.iter().map(|t| t.tag.id).collect()
    }

    #[test]
    fn search_ranks_the_deepest_match_first_then_name_prefix_then_path() {
        let tags = tree();
        // Bergen (depth 3) before People/Bergen Berg (depth 2) before Bergensbanen (depth 1).
        assert_eq!(ids(&search(&tags, "  BERGEN ", &HashSet::new())), [6, 4, 5]);
        // A path match counts too: "norway/" matches only through full_path.
        assert_eq!(ids(&search(&tags, "norway/", &HashSet::new())), [6]);
        // Equal depth: the name that starts with the query wins over one that contains it.
        let tags2 = vec![tag(1, "Xbike", None, 0), tag(2, "Bike", None, 0), tag(3, "Abike", None, 0)];
        assert_eq!(ids(&search(&tags2, "bike", &HashSet::new())), [2, 3, 1]);
        assert!(search(&tags, "   ", &HashSet::new()).is_empty());
    }

    #[test]
    fn search_excludes_assigned_tags_and_caps_at_eight() {
        let tags = tree();
        assert_eq!(ids(&search(&tags, "bergen", &HashSet::from([6]))), [4, 5]);
        let many: Vec<TagWithCount> = (0..20).map(|i| tag(i, &format!("Tag{i:02}"), None, 0)).collect();
        let hits = search(&many, "tag", &HashSet::new());
        assert_eq!(hits.len(), SEARCH_LIMIT);
        assert_eq!(hits[0].tag.full_path, "Tag00", "path order breaks the remaining tie");
    }

    #[test]
    fn locale_order_ignores_case_first() {
        assert_eq!(locale_cmp("bergen", "Oslo"), Ordering::Less);
        assert_eq!(locale_cmp("Bergen", "bergen"), Ordering::Less);
    }

    #[test]
    fn the_index_knows_parents_ancestors_and_subtrees() {
        let tags = tree();
        let index = TagIndex::new(&tags);
        assert!(index.has_children(1) && index.has_children(2) && !index.has_children(6));
        assert_eq!(index.parent_ids(), HashSet::from([1, 2, 3]));
        assert_eq!(index.ancestors(6), [2, 1]);
        assert_eq!(index.subtree(1), HashSet::from([1, 2, 6]));
        assert!(index.is_hidden(6, &HashSet::from([1])));
        assert!(!index.is_hidden(2, &HashSet::from([2])), "a collapsed row itself stays visible");
    }

    #[test]
    fn a_drop_is_refused_onto_itself_its_subtree_or_where_it_already_is() {
        let index = TagIndex::new(&tree());
        assert!(!index.can_drop(1, Some(1)));
        assert!(!index.can_drop(1, Some(6)), "into its own grandchild");
        assert!(!index.can_drop(6, Some(2)), "already there");
        assert!(index.can_drop(6, Some(3)));
        assert!(index.can_drop(6, None));
        assert!(!index.can_drop(1, None), "already top level");
    }

    #[test]
    fn a_corrupt_parent_cycle_terminates() {
        let tags = vec![tag(1, "A", Some(2), 0), tag(2, "B", Some(1), 0)];
        let index = TagIndex::new(&tags);
        assert_eq!(index.ancestors(1), [2, 1]);
        assert!(index.is_hidden(1, &HashSet::from([2])));
    }

    #[test]
    fn move_and_merge_candidates_leave_out_what_the_backend_refuses() {
        let tags = tree();
        let index = TagIndex::new(&tags);
        let place = &tags[3];
        // Moving Place: not itself or its subtree; no parent to exclude.
        assert_eq!(ids(&move_candidates(&tags, &index, place, "")), [5, 3, 4]);
        let bergen = &tags[5];
        // Moving Bergen: not its current parent Norway; filter by path.
        assert_eq!(ids(&move_candidates(&tags, &index, bergen, "PLACE")), [1]);
        assert_eq!(ids(&merge_candidates(&tags, place, "")), [5, 3, 4]);
        assert_eq!(ids(&merge_candidates(&tags, bergen, "berg")), [5, 4]);
    }

    fn report() -> TagMergeReport {
        TagMergeReport {
            target_id: 2,
            target_path: "Cycling".into(),
            sources: vec![MergedSource { id: 1, path: "Bike".into(), uuid: None }],
            photos_retagged: 1234,
            assignments_collapsed: 1,
            children_reparented: 2,
            descendants_repathed: 1,
            terms_moved: 0,
            terms_skipped: vec![],
            synonyms_moved: 3,
            groups_repointed: 0,
            smart_albums_rewritten: vec!["Rides".into()],
            aliases_recorded: 1,
            warnings: vec![],
            faces_repointed: None,
            face_rejections_repointed: Some(0),
            classifiers_dropped: Some(2),
            suggestions_repointed: None,
        }
    }

    #[test]
    fn a_merge_reads_back_in_reacts_words() {
        let r = report();
        assert_eq!(
            merge_summary(&r),
            "Merged Bike into Cycling — 1,234 photos moved, 1 already tagged, 2 child tags moved, 1 smart album rule(s) rewritten."
        );
        assert_eq!(
            merge_preview_lines(&r),
            [
                "1,234 photos gain Cycling",
                "1 already had it — two assignments become one",
                "2 child tags move across, and 1 tag path are rewritten",
                "3 synonym(s) move",
                "Smart album rules rewritten: Rides",
                "2 Smart Tagging classifier(s) dropped — retrained from the merged set",
                "1 tombstone(s) recorded, so importing an old bundle cannot bring the tag back",
            ]
        );
        let empty = TagMergeReport { target_path: "Cycling".into(), ..TagMergeReport::default() };
        assert!(merge_preview_lines(&empty).is_empty(), "an empty tag: nothing to move");
        assert_eq!(merge_summary(&empty), "Merged tag into Cycling.");
    }

    #[test]
    fn a_split_reads_back_in_reacts_words() {
        let r = TagSplitReport {
            source_path: "Concert".into(),
            new_tag_id: 9,
            new_tag_path: "Venue".into(),
            photos_moved: 1,
            photos_already_tagged: 2,
            photos_untagged: 1,
            photos_without_source: 3,
            created_new_tag: true,
        };
        assert_eq!(split_summary(&r), "1 photo moved to Venue — 2 already had it — 3 never carried Concert.");
        assert_eq!(
            split_preview_lines(&r),
            [
                "1 photo gain Venue (a new tag)",
                "1 lose Concert",
                "2 already had the new tag",
                "3 of the selection never carried Concert — left alone",
            ]
        );
    }

    #[test]
    fn counts_group_like_to_locale_string() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1234567), "1,234,567");
        assert_eq!(privacy_summary(1, true), "1 tag marked private.");
        assert_eq!(privacy_summary(3, false), "3 tags marked public.");
        assert_eq!(ancestor_prefix("Place/Norway/Bergen", "Bergen"), "Place/Norway/");
    }
}
