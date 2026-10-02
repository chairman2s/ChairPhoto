//! [`People`]: the state of the Faces module's main view, "People" (`PeopleView` in
//! faces.tsx, #130) — the named people, the unnamed clusters, the suggestion queue, and every
//! write those make. [`super::people_view::PeopleView`] draws it.
//!
//! **When it reads.** When the People view comes on stage, on Refresh, after each of its
//! writes, after any catalog-wide refresh while it shows (a confirm in the inspector, an index
//! run's end), and when any matching run ends. Each read runs on the storage [`Runner`] and
//! is identified (`with_catalog_identified`); only the newest read lands, and none from
//! before a catalog switch.
//!
//! **Catalog identity** (map #92). Every id the view shows — person tags, cluster ids, face
//! ids — is the catalog's it was read from ([`PeopleData::from`]). Every write runs under
//! `with_catalog_as(from)`, so it fails closed once another catalog is open, even before
//! `catalog:switched` arrives; filter-by-person checks the same identity before it scopes
//! the Library to the tag. A switch closes the naming dialog and the cluster sheet and drops
//! the data.
//!
//! **Merge and split** (the GPUI view adds them; React only named a whole cluster): naming
//! several picked clusters at once makes them one person (`name_clusters`); naming some faces
//! of a cluster's sheet splits them off (`name_faces`), and the sheet can ignore faces. Only
//! faces still pending a decision change. A running match regroups the clusters, so the
//! People view's writes wait while one runs (the core's matcher writes suggestions without
//! re-checking a face's state, so a naming racing it could be overwritten).

use super::state::FacesState;
use crate::model::{AppModel, AppModelEvent};
use crate::shell::state::Surface;
use crate::shell::ShellState;
use crate::storage::Runner;
use chairphoto_core::app::faces::{
    self as core_faces, ClusterFace, ClusterSummary, NameOutcome, PersonSummary, Review, ReviewOutcome,
    SuggestionEntry, Verdict,
};
use chairphoto_core::app::{with_catalog_as, with_catalog_identified, AppState, CatalogIdentity, CoreEvent};
use chairphoto_core::catalog::{Catalog, Tag};
use gpui_kit::{App, Context, Entity, SharedString, Subscription};
use std::collections::BTreeSet;

/// The main view's id: what `Surface::Module` names and the rail orders by.
pub const PEOPLE_VIEW_ID: &str = "people";

/// The queue's default "Confirm all ≥" threshold (React: 0.8).
pub const DEFAULT_REVIEW_THRESHOLD: f64 = 0.8;

/// What the People view refuses while a matching run regroups the faces.
pub const WAIT_FOR_MATCHING: &str = "Face matching is running — wait for it to finish, then try again.";

/// One read of everything the view shows, from one catalog.
#[derive(Debug, Clone)]
pub struct PeopleData {
    pub from: CatalogIdentity,
    pub people: Vec<PersonSummary>,
    pub clusters: Vec<ClusterSummary>,
    pub suggestions: Vec<SuggestionEntry>,
    /// The people root a typed name goes under (the matcher's: `People` when unset).
    pub root: String,
    /// The tags under the root: the naming dialog's type-ahead.
    pub people_tags: Vec<Tag>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    People,
    Clusters,
    Suggestions,
}

/// The open cluster's face sheet.
#[derive(Debug, Clone)]
pub struct ClusterSheet {
    pub cluster: i64,
    pub from: CatalogIdentity,
    /// `None` while loading.
    pub faces: Option<Vec<ClusterFace>>,
    /// The faces picked for "Name selected…" / "Ignore selected" (a split).
    pub picked: BTreeSet<i64>,
}

/// What the naming dialog names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameTarget {
    /// One cluster, or several picked ones (a merge).
    Clusters(Vec<i64>),
    /// Some faces of a cluster's sheet (a split).
    Faces(Vec<i64>),
}

#[derive(Debug, Clone)]
pub struct Naming {
    pub target: NameTarget,
    /// Faces it names, for the dialog's line.
    pub faces: usize,
    pub from: CatalogIdentity,
    pub error: Option<String>,
    pub busy: bool,
}

pub struct People {
    app: AppState,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    faces: Entity<FacesState>,
    pub data: Option<PeopleData>,
    pub loading: bool,
    pub error: Option<String>,
    pub tab: Tab,
    pub sheet: Option<ClusterSheet>,
    /// Clusters picked on the Clusters tab (for "Name together…").
    pub picked_clusters: Vec<i64>,
    pub naming: Option<Naming>,
    /// The suggestion queue's "Confirm all ≥" threshold.
    pub threshold: f64,
    /// A queue or sheet write in flight: its buttons wait.
    pub busy: bool,
    visible: bool,
    read_seq: u64,
    generation: u64,
    live: bool,
    _subscriptions: Vec<Subscription>,
}

impl People {
    pub fn new(
        app: AppState,
        model: Entity<AppModel>,
        shell: Entity<ShellState>,
        faces: Entity<FacesState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&shell, |this, _, cx| this.sync(cx)),
            cx.subscribe(&model, |this, _, event: &AppModelEvent, cx| match event {
                AppModelEvent::CatalogRead if this.visible => this.refresh(cx),
                AppModelEvent::Core(CoreEvent::CatalogSwitched(_)) => this.catalog_switched(cx),
                AppModelEvent::Core(CoreEvent::FacesMatchDone(_)) if this.visible => this.refresh(cx),
                _ => {}
            }),
            // "Matching is running" gates the writes: redraw when it changes.
            cx.observe(&faces, |_, _, cx| cx.notify()),
        ];
        let mut this = People {
            app,
            model,
            shell,
            faces,
            data: None,
            loading: false,
            error: None,
            tab: Tab::People,
            sheet: None,
            picked_clusters: Vec::new(),
            naming: None,
            threshold: DEFAULT_REVIEW_THRESHOLD,
            busy: false,
            visible: false,
            read_seq: 0,
            generation: 0,
            live: true,
            _subscriptions: subscriptions,
        };
        this.sync(cx);
        this
    }

    pub fn shell(&self) -> &Entity<ShellState> {
        &self.shell
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Whether a matching run is going (the writes wait for it).
    pub fn matching(&self, cx: &App) -> bool {
        self.faces.read(cx).matching.busy()
    }

    /// Run `work` off the UI thread and hand its result to `land`, unless the catalog was
    /// switched (or the module unloaded) meanwhile.
    fn run<R: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce(&AppState) -> R + Send + 'static,
        land: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) {
        let generation = self.generation;
        let app = self.app.clone();
        let rx = Runner::get(cx).run(move || work(&app));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if s.generation != generation || !s.live {
                    return;
                }
                land(s, result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn status(&self, line: impl Into<SharedString>, cx: &mut Context<Self>) {
        let line = line.into();
        self.model.update(cx, |m, cx| m.set_status(line, cx));
    }

    /// Follow the stage: read when the People view comes on.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let visible = matches!(&self.shell.read(cx).surface, Surface::Module(id) if id == PEOPLE_VIEW_ID);
        let appeared = visible && !self.visible;
        let changed = visible != self.visible;
        self.visible = visible;
        if appeared {
            self.refresh(cx);
        }
        if changed {
            cx.notify(); // the view lets go of its thumbnails off stage
        }
    }

    fn catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.data = None;
        self.loading = false;
        self.error = None;
        self.sheet = None;
        self.picked_clusters.clear();
        self.naming = None;
        self.busy = false;
        // The shell sends the stage back to the Library; the next showing reads afresh.
        self.visible = false;
        cx.notify();
    }

    pub fn unload(&mut self, cx: &mut Context<Self>) {
        self.live = false;
        self.generation += 1;
        cx.notify();
    }

    // --- reads ----------------------------------------------------------------------------

    /// "Refresh": everything the view shows, from one catalog; and the open sheet's faces.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.live {
            return;
        }
        self.loading = true;
        self.error = None;
        self.read_seq += 1;
        let seq = self.read_seq;
        cx.notify();
        self.run(
            cx,
            |app| with_catalog_identified(app, read_all),
            move |s, result, cx| {
                if s.read_seq != seq {
                    return; // a newer read is on its way
                }
                s.loading = false;
                match result {
                    Ok((from, (people, clusters, suggestions, root, people_tags))) => {
                        if s.data.as_ref().is_some_and(|d| d.from != from) {
                            // Another catalog (switched without the event yet, or re-rooted):
                            // nothing picked from the old one survives.
                            s.sheet = None;
                            s.picked_clusters.clear();
                            s.naming = None;
                        }
                        let live: BTreeSet<i64> = clusters.iter().map(|c| c.cluster_id).collect();
                        s.picked_clusters.retain(|c| live.contains(c));
                        s.data = Some(PeopleData { from, people, clusters, suggestions, root, people_tags });
                        s.reload_sheet(cx);
                    }
                    Err(e) => s.error = Some(e),
                }
            },
        );
    }

    /// Open cluster `cluster`'s face sheet (from the Clusters tab).
    pub fn open_cluster(&mut self, cluster: i64, cx: &mut Context<Self>) {
        let Some(from) = self.data.as_ref().map(|d| d.from) else { return };
        self.sheet = Some(ClusterSheet { cluster, from, faces: None, picked: BTreeSet::new() });
        self.reload_sheet(cx);
        cx.notify();
    }

    pub fn close_cluster(&mut self, cx: &mut Context<Self>) {
        self.sheet = None;
        cx.notify();
    }

    fn reload_sheet(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = &self.sheet else { return };
        let (cluster, from) = (sheet.cluster, sheet.from);
        self.run(
            cx,
            move |app| with_catalog_as(app, from, |c| core_faces::cluster_faces(c, cluster)),
            move |s, result, _| {
                let Some(sheet) = s.sheet.as_mut().filter(|sh| sh.cluster == cluster && sh.from == from) else { return };
                match result {
                    Ok(faces) => {
                        let live: BTreeSet<i64> = faces.iter().map(|f| f.face_id).collect();
                        sheet.picked.retain(|f| live.contains(f));
                        sheet.faces = Some(faces);
                    }
                    Err(e) => {
                        s.sheet = None;
                        s.error = Some(e);
                    }
                }
            },
        );
    }

    // --- picking --------------------------------------------------------------------------

    pub fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.sheet = None;
        cx.notify();
    }

    pub fn toggle_cluster(&mut self, cluster: i64, cx: &mut Context<Self>) {
        if let Some(i) = self.picked_clusters.iter().position(|&c| c == cluster) {
            self.picked_clusters.remove(i);
        } else {
            self.picked_clusters.push(cluster);
        }
        cx.notify();
    }

    pub fn toggle_face(&mut self, face: i64, cx: &mut Context<Self>) {
        if let Some(sheet) = &mut self.sheet {
            if !sheet.picked.remove(&face) {
                sheet.picked.insert(face);
            }
            cx.notify();
        }
    }

    pub fn set_threshold(&mut self, threshold: f64, cx: &mut Context<Self>) {
        self.threshold = threshold.clamp(0., 1.);
        cx.notify();
    }

    // --- naming ---------------------------------------------------------------------------

    /// Open the naming dialog for one cluster (a card's click).
    pub fn name_cluster(&mut self, cluster: i64, cx: &mut Context<Self>) {
        self.open_naming(NameTarget::Clusters(vec![cluster]), cx);
    }

    /// "Name together…": the picked clusters as one person (a merge).
    pub fn name_picked_clusters(&mut self, cx: &mut Context<Self>) {
        if self.picked_clusters.is_empty() {
            return;
        }
        let picked = self.picked_clusters.clone();
        self.open_naming(NameTarget::Clusters(picked), cx);
    }

    /// The sheet's "Name selected…" (a split) — or, with nothing picked, the whole cluster.
    pub fn name_sheet(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = &self.sheet else { return };
        let target = if sheet.picked.is_empty() {
            NameTarget::Clusters(vec![sheet.cluster])
        } else {
            NameTarget::Faces(sheet.picked.iter().copied().collect())
        };
        self.open_naming(target, cx);
    }

    fn open_naming(&mut self, target: NameTarget, cx: &mut Context<Self>) {
        let Some(data) = &self.data else { return };
        if self.matching(cx) {
            self.status(WAIT_FOR_MATCHING, cx);
            return;
        }
        let faces = match &target {
            NameTarget::Clusters(ids) => {
                data.clusters.iter().filter(|c| ids.contains(&c.cluster_id)).map(|c| c.member_count as usize).sum()
            }
            NameTarget::Faces(ids) => ids.len(),
        };
        self.naming = Some(Naming { target, faces, from: data.from, error: None, busy: false });
        cx.notify();
    }

    pub fn cancel_naming(&mut self, cx: &mut Context<Self>) {
        self.naming = None;
        cx.notify();
    }

    /// The dialog's Confirm: name the target as `typed` (under the people root unless it is
    /// already a path below it), bound to the catalog the dialog was opened on.
    pub fn confirm_naming(&mut self, typed: String, cx: &mut Context<Self>) {
        let Some(naming) = &mut self.naming else { return };
        if naming.busy {
            return;
        }
        if typed.trim().is_empty() {
            naming.error = Some("Enter a name or tag path.".into());
            cx.notify();
            return;
        }
        if self.faces.read(cx).matching.busy() {
            naming.error = Some(WAIT_FOR_MATCHING.into());
            cx.notify();
            return;
        }
        let root = self.data.as_ref().map(|d| d.root.clone()).unwrap_or_default();
        let path = super::logic::named_path(&root, &typed);
        let (target, from) = (naming.target.clone(), naming.from);
        naming.busy = true;
        naming.error = None;
        cx.notify();
        self.run(
            cx,
            move |app| {
                with_catalog_as(app, from, |c| match &target {
                    NameTarget::Clusters(ids) => core_faces::name_clusters(c, ids, &path),
                    NameTarget::Faces(ids) => core_faces::name_faces(c, ids, &path),
                })
                .map(|out| (out, path))
            },
            |s, result, cx| match result {
                Ok((out, path)) => {
                    s.naming = None;
                    s.picked_clusters.clear();
                    if let Some(sheet) = &mut s.sheet {
                        sheet.picked.clear();
                    }
                    s.status(name_line(&out, &path), cx);
                    s.changed(cx);
                }
                Err(e) => {
                    if let Some(n) = &mut s.naming {
                        n.busy = false;
                        n.error = Some(format!("Failed: {e}"));
                    }
                }
            },
        );
    }

    /// The sheet's "Ignore selected": strangers, a crowd.
    pub fn ignore_picked(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = &self.sheet else { return };
        if sheet.picked.is_empty() || self.busy {
            return;
        }
        if self.matching(cx) {
            self.status(WAIT_FOR_MATCHING, cx);
            return;
        }
        let (ids, from): (Vec<i64>, _) = (sheet.picked.iter().copied().collect(), sheet.from);
        self.busy = true;
        cx.notify();
        self.run(cx, move |app| with_catalog_as(app, from, |c| core_faces::ignore_faces(c, &ids)), |s, result, cx| {
            s.busy = false;
            match result {
                Ok(n) => {
                    if let Some(sheet) = &mut s.sheet {
                        sheet.picked.clear();
                    }
                    s.status(format!("Ignored {n} face{}.", if n == 1 { "" } else { "s" }), cx);
                }
                Err(e) => s.status(format!("Faces: could not ignore the faces: {e}"), cx),
            }
            s.changed(cx);
        });
    }

    // --- the suggestion queue -------------------------------------------------------------

    /// ✓ / ✕ on one row, or "Confirm all ≥ X%": each applies only while the face is still
    /// suggested as the person the row shows (`review_suggestions`).
    pub fn review(&mut self, reviews: Vec<Review>, cx: &mut Context<Self>) {
        let Some(from) = self.data.as_ref().map(|d| d.from) else { return };
        if reviews.is_empty() || self.busy {
            return;
        }
        if self.matching(cx) {
            self.status(WAIT_FOR_MATCHING, cx);
            return;
        }
        self.busy = true;
        cx.notify();
        self.run(
            cx,
            move |app| with_catalog_as(app, from, |c| core_faces::review_suggestions(c, &reviews)),
            |s, result, cx| {
                s.busy = false;
                match result {
                    Ok(out) => s.status(review_line(&out), cx),
                    Err(e) => s.status(format!("Faces: could not apply the review: {e}"), cx),
                }
                s.changed(cx);
            },
        );
    }

    /// One row's ✓ or ✕.
    pub fn review_one(&mut self, face: i64, verdict: Verdict, cx: &mut Context<Self>) {
        let Some(entry) = self.data.as_ref().and_then(|d| d.suggestions.iter().find(|e| e.face_id == face)) else {
            return;
        };
        let review = Review { face_id: face, tag_id: entry.person_tag_id, verdict };
        self.review(vec![review], cx);
    }

    /// The suggestions "Confirm all ≥ X%" would confirm.
    pub fn above_threshold(&self) -> Vec<&SuggestionEntry> {
        let Some(d) = &self.data else { return Vec::new() };
        super::logic::at_or_above(d.suggestions.iter().map(|e| e.confidence), self.threshold)
            .into_iter()
            .map(|i| &d.suggestions[i])
            .collect()
    }

    /// "Confirm all ≥ X% (n)".
    pub fn confirm_all(&mut self, cx: &mut Context<Self>) {
        let reviews: Vec<Review> = self
            .above_threshold()
            .into_iter()
            .map(|e| Review { face_id: e.face_id, tag_id: e.person_tag_id, verdict: Verdict::Confirm })
            .collect();
        self.review(reviews, cx);
    }

    // --- filter by person -----------------------------------------------------------------

    /// A person card: the Library, filtered by that person's tag (`api.filterByTag`) — only
    /// while the catalog the tag id was read from is still open.
    pub fn filter_by_person(&mut self, tag: i64, cx: &mut Context<Self>) {
        let Some(from) = self.data.as_ref().map(|d| d.from) else { return };
        self.run(cx, move |app| with_catalog_as(app, from, |_| Ok(())), move |s, result, cx| match result {
            Ok(()) => s.shell.update(cx, |sh, cx| {
                sh.update_scope(cx, |l| l.select_tag(Some(tag)));
                sh.show_library(cx);
            }),
            Err(e) => s.status(format!("Faces: {e}"), cx),
        });
    }

    /// A write landed: every catalog-derived view re-reads — this one through the model's
    /// `CatalogRead`, which it follows while it shows.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.refresh(cx));
        self.faces.update(cx, |f, cx| f.follow_photo(true, cx));
    }
}

/// Everything the view shows, in one catalog lock.
#[allow(clippy::type_complexity)]
fn read_all(
    c: &Catalog,
) -> chairphoto_core::catalog::Result<(Vec<PersonSummary>, Vec<ClusterSummary>, Vec<SuggestionEntry>, String, Vec<Tag>)> {
    let root = core_faces::effective_people_root(c)?;
    let prefix = format!("{root}/");
    let people_tags = c
        .list_tags_with_counts()?
        .into_iter()
        .map(|t| t.tag)
        .filter(|t| t.full_path == root || t.full_path.starts_with(&prefix))
        .collect();
    Ok((core_faces::people_summary(c)?, core_faces::cluster_summary(c)?, core_faces::suggestion_list(c)?, root, people_tags))
}

/// The status line a naming leaves.
pub fn name_line(out: &NameOutcome, path: &str) -> String {
    let mut line = format!(
        "Named {} face{} on {} photo{} as {path}.",
        out.faces,
        if out.faces == 1 { "" } else { "s" },
        out.photos,
        if out.photos == 1 { "" } else { "s" }
    );
    if out.skipped > 0 {
        line.push_str(&format!(" {} had changed since the list was read and were left alone.", out.skipped));
    }
    line
}

/// The status line a review leaves.
pub fn review_line(out: &ReviewOutcome) -> String {
    let mut parts = Vec::new();
    if out.confirmed > 0 {
        parts.push(format!("{} confirmed", out.confirmed));
    }
    if out.rejected > 0 {
        parts.push(format!("{} rejected", out.rejected));
    }
    if out.stale > 0 {
        parts.push(format!("{} changed since the list was read and were left alone", out.stale));
    }
    if parts.is_empty() {
        "Nothing to review.".into()
    } else {
        format!("Suggestions: {}.", parts.join(", "))
    }
}
