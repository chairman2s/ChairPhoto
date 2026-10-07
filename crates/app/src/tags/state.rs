//! [`TagsState`]: the one owner of the tag tree with its photo counts (`list_tags`), the
//! in-app tag clipboard, and the write path every tag view goes through.
//!
//! **Reads.** The tree is re-read off the UI thread whenever the [`AppModel`] has read the
//! catalog ([`AppModelEvent::CatalogRead`]: startup, a switch, a scan's end, and every
//! mutation below, which calls [`AppModel::refresh`] — React's `refresh`, which re-ran the
//! library query *and* `listTags`). Only the newest read lands.
//!
//! **Writes** (and the dialogs' reads) go through [`run`] / [`run_as`]: the work runs on
//! GPUI's background executor, never the UI thread, and its result lands in the view that
//! asked only while the catalog it was asked against is still open. Two fences, because a
//! catalog switch can come between any two steps:
//!
//! - **Under the catalog lock**, a job runs only while the open catalog is the one the tree
//!   was read from ([`CatalogGuard`], whose `CatalogIdentity` is captured with the tree:
//!   `with_catalog_as`); otherwise it fails closed with `CATALOG_CHANGED`. Tag and photo ids
//!   are per-catalog, so an id from the closed catalog would name some other row. The
//!   identity is the open handle's, not its file: a switch away and back, or a re-root's
//!   reopen, also counts as a change.
//! - **On landing**, a result is dropped when `catalog:switched` arrived since the job
//!   started (the [`TagsState::epoch`]).
//!
//! **Dialogs** (merge, split, editor, groups, create, move) take their guard when they open
//! ([`bind_dialog`]) — the tree they show ids from — not when a button is clicked, and close
//! themselves once that tree is superseded (a switch, or a re-read of another catalog).
//!
//! Tag writes are catalog-only: in-library tag assignments write no sidecar
//! (`xmp::write_keywords` has one caller, the export destination — docs/taxonomy.md
//! § Tag maintenance), so nothing here touches XMP.

use crate::model::{AppModel, AppModelEvent};
use crate::storage::CloseDialog;
use chairphoto_core::app::{with_catalog, with_catalog_as, with_catalog_identified, AppState, CatalogIdentity, CoreEvent, CATALOG_CHANGED};
use chairphoto_core::catalog::{Catalog, TagBatchOutcome, TagWithCount};
use chairphoto_model::tag_tree::TagIndex;
use gpui_kit::{App, Context, Entity, EventEmitter, Subscription, WeakEntity};
use std::cell::Cell;
use std::rc::Rc;

/// The open catalog a job was started against: the switch epoch, and the identity of the
/// catalog the tree was read from (`None` until a tree has been read).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatalogGuard {
    pub epoch: u64,
    pub identity: Option<CatalogIdentity>,
}

impl CatalogGuard {
    /// Whether `c` is the catalog this guard was taken on. Checked under the catalog lock.
    pub fn holds(&self, c: &Catalog) -> bool {
        self.identity.is_some_and(|i| i.is(c))
    }

    /// Whether `now` (the tag state's guard) names another catalog than this one: a switch
    /// since, or a tree read from another catalog handle.
    pub fn superseded_by(&self, now: &CatalogGuard) -> bool {
        now.epoch != self.epoch || (now.identity.is_some() && now.identity != self.identity)
    }
}

/// A dialog's binding to the tree it opened over: the guard its jobs run under ([`run_as`]),
/// and a subscription that closes it (one [`CloseDialog`]) once that tree is superseded —
/// its ids would name another catalog's tags.
pub fn bind_dialog<V: EventEmitter<CloseDialog> + 'static>(
    tags: &Entity<TagsState>,
    cx: &mut Context<V>,
) -> (CatalogGuard, Subscription) {
    let guard = tags.read(cx).guard();
    let closed = Rc::new(Cell::new(false));
    let sub = cx.observe(tags, move |_, tags, cx| {
        if !closed.get() && guard.superseded_by(&tags.read(cx).guard()) {
            closed.set(true);
            cx.emit(CloseDialog);
        }
    });
    (guard, sub)
}

/// The dialog a tag view opened last — what a test drives. Weak: holding it must not keep a
/// closed dialog's view alive (the tag editor clears the shell's editing tag on release).
#[derive(Clone)]
pub enum TagDialog {
    Editor(WeakEntity<super::editor::TagEditor>),
    Create(WeakEntity<super::create::TagCreate>),
    Move(WeakEntity<super::move_tag::TagMove>),
    Merge(WeakEntity<super::merge::TagMerge>),
    Split(WeakEntity<super::split::TagSplit>),
    Groups(WeakEntity<super::groups::TagGroupsManager>),
}

pub struct TagsState {
    app: AppState,
    model: Entity<AppModel>,
    /// The tree, in `full_path` order (`list_tags_with_counts`).
    pub tags: Vec<TagWithCount>,
    pub index: Rc<TagIndex>,
    /// A tree has been read for the open catalog.
    pub loaded: bool,
    /// The catalog the tree was read from.
    identity: Option<CatalogIdentity>,
    /// Bumped by every `catalog:switched`.
    epoch: u64,
    /// Bumped by every tree read; an older read's result is dropped.
    generation: u64,
    /// Bumped whenever a tag write landed: views that show per-photo tags, quick-tag groups
    /// or "Recently used" re-read on a change (React's `groupsKey`).
    pub revision: u64,
    /// Copied tag ids (in-app, not the OS clipboard), cleared by a catalog switch.
    pub clipboard: Vec<i64>,
    /// The dialog opened last (tests drive it through this).
    pub last_dialog: Option<TagDialog>,
    _model_events: Subscription,
}

impl TagsState {
    pub fn new(model: &Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        let app = model.read(cx).state().clone();
        let _model_events = cx.subscribe(model, |this, _, event: &AppModelEvent, cx| match event {
            AppModelEvent::CatalogRead => this.refresh(cx),
            AppModelEvent::Core(CoreEvent::CatalogSwitched(_)) => this.on_catalog_switched(cx),
            _ => {}
        });
        TagsState {
            app,
            model: model.clone(),
            tags: Vec::new(),
            index: Rc::default(),
            loaded: false,
            identity: None,
            epoch: 0,
            generation: 0,
            revision: 0,
            clipboard: Vec::new(),
            last_dialog: None,
            _model_events,
        }
    }

    pub fn app_state(&self) -> &AppState {
        &self.app
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The guard a job started now carries.
    pub fn guard(&self) -> CatalogGuard {
        CatalogGuard { epoch: self.epoch, identity: self.identity }
    }

    pub fn tag(&self, id: i64) -> Option<&TagWithCount> {
        self.tags.iter().find(|t| t.tag.id == id)
    }

    /// Every id in the tree, the clipboard and the open dialogs names the closed catalog.
    pub(crate) fn on_catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.epoch += 1;
        self.generation += 1;
        self.tags.clear();
        self.index = Rc::default();
        self.loaded = false;
        self.identity = None;
        self.clipboard.clear();
        self.revision += 1;
        cx.notify();
    }

    /// Re-read the tree off the UI thread.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let (generation, epoch) = (self.generation, self.epoch);
        let state = self.app.clone();
        let read = cx.background_executor().spawn(async move {
            with_catalog_identified(&state, |c| c.list_tags_with_counts())
        });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |s, cx| {
                if s.generation != generation || s.epoch != epoch {
                    return;
                }
                match result {
                    Ok((identity, tags)) => {
                        s.index = Rc::new(TagIndex::new(&tags));
                        s.tags = tags;
                        s.identity = Some(identity);
                        s.loaded = true;
                    }
                    Err(e) => eprintln!("tags: tree unavailable: {e}"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A write landed: everything catalog-derived re-reads (the rows, counts, lists and this
    /// tree, through the model's `CatalogRead`), and the per-photo views re-read.
    pub fn changed(&mut self, cx: &mut Context<Self>) {
        self.revision += 1;
        self.model.update(cx, |m, cx| m.refresh(cx));
        cx.notify();
    }

    pub fn set_status(&self, line: impl Into<gpui_kit::SharedString>, cx: &mut App) {
        let line = line.into();
        self.model.update(cx, |m, cx| m.set_status(line, cx));
    }

    // --- the write verbs the panel and the inspector share ----------------------------------

    /// [`run`] for this entity's own verbs.
    fn run<R: Send + 'static>(
        &self,
        cx: &mut Context<Self>,
        work: impl FnOnce(&Catalog) -> chairphoto_core::catalog::Result<R> + Send + 'static,
        then: impl FnOnce(&mut Self, Result<R, String>, &mut Context<Self>) + 'static,
    ) {
        let tags = cx.entity();
        run_on(tags, self.app.clone(), self.guard(), cx, true, work, then);
    }

    /// Reparent a tag (drag-and-drop, Move to…, Move to top level). Failures go to the
    /// status line as React's `Move failed: …`.
    pub fn move_tag(&mut self, tag_id: i64, parent: Option<i64>, cx: &mut Context<Self>) {
        self.move_tag_as(self.guard(), tag_id, parent, cx);
    }

    /// [`move_tag`](Self::move_tag) under `guard` — the Move dialog's, taken when it opened.
    pub fn move_tag_as(&mut self, guard: CatalogGuard, tag_id: i64, parent: Option<i64>, cx: &mut Context<Self>) {
        let tags = cx.entity();
        run_on(tags, self.app.clone(), guard, cx, true, move |c| c.move_tag(tag_id, parent), |s, result, cx| {
            if let Err(e) = result {
                s.set_status(format!("Move failed: {e}"), cx);
            }
        });
    }

    /// Make a tag (and with `recursive` its subtree) private or public.
    pub fn set_private(&mut self, tag_id: i64, private: bool, recursive: bool, cx: &mut Context<Self>) {
        self.run(cx, move |c| c.set_tag_private(tag_id, private, recursive), move |s, result, cx| {
            let line = match result {
                Ok(n) => chairphoto_model::tag_tree::privacy_summary(n, private),
                Err(e) => format!("Privacy change failed: {e}"),
            };
            s.set_status(line, cx);
        });
    }

    /// Assign a tag to every target photo (the selection, else the active photo). An auto-tag
    /// is refused by the core (#181); its message goes to the status line.
    pub fn assign(&mut self, targets: Vec<i64>, tag_id: i64, cx: &mut Context<Self>) {
        if targets.is_empty() {
            return;
        }
        self.run(cx, move |c| c.assign_tags(&targets, &[tag_id]), |s, r, cx| match r {
            Ok(out) => s.report_skipped(&out, cx),
            Err(e) => s.set_status(format!("Could not tag: {e}"), cx),
        });
    }

    /// The status for a batch write that skipped auto-tags (#181): the core's refusal for
    /// one, a count for several. Nothing when none was skipped.
    fn report_skipped(&mut self, out: &TagBatchOutcome, cx: &mut Context<Self>) {
        if let Some(line) = skipped_line(out) {
            self.set_status(line, cx);
        }
    }

    /// Create the tag at `path` (`create_tag` returns the existing id when it already exists)
    /// and assign it to every target — the inspector's add-tag box when nothing matched.
    pub fn create_and_assign(&mut self, targets: Vec<i64>, path: String, cx: &mut Context<Self>) {
        let path = path.trim().to_string();
        if targets.is_empty() || path.is_empty() {
            return;
        }
        self.run(
            cx,
            move |c| {
                let tag_id = c.create_tag(&path)?;
                c.assign_tags(&targets, &[tag_id])
            },
            |s, r, cx| match r {
                Ok(out) => s.report_skipped(&out, cx),
                Err(e) => s.set_status(format!("Could not tag: {e}"), cx),
            },
        );
    }

    /// Remove a tag from every target photo — the whole selection, not only the active
    /// photo the chips show (App.tsx's `removeFromSelection`).
    pub fn remove(&mut self, targets: Vec<i64>, tag_id: i64, cx: &mut Context<Self>) {
        if targets.is_empty() {
            return;
        }
        self.run(cx, move |c| c.remove_tags(&targets, &[tag_id]), |s, r, cx| match r {
            Ok(out) => s.report_skipped(&out, cx),
            Err(e) => s.set_status(format!("Could not remove the tag: {e}"), cx),
        });
    }

    /// Copy tag ids to the in-app clipboard (App.tsx's `copyTags`).
    pub fn copy(&mut self, tag_ids: Vec<i64>, cx: &mut Context<Self>) {
        let line = if tag_ids.is_empty() {
            "That photo has no tags to copy".to_string()
        } else {
            format!("Copied {} tag(s)", tag_ids.len())
        };
        self.clipboard = tag_ids;
        self.set_status(line, cx);
        cx.notify();
    }

    /// Assign every copied tag to every target (App.tsx's `pasteTagsToSelection`).
    pub fn paste(&mut self, targets: Vec<i64>, cx: &mut Context<Self>) {
        let clipboard = self.clipboard.clone();
        if clipboard.is_empty() || targets.is_empty() {
            return;
        }
        let n_targets = targets.len();
        // Auto-tags copied from a photo are skipped, not pasted (#181), and counted.
        self.run(
            cx,
            move |c| c.assign_tags(&targets, &clipboard),
            move |s, r, cx| match r {
                Ok(out) => s.set_status(paste_line(&out, n_targets), cx),
                Err(e) => s.set_status(format!("Could not paste tags: {e}"), cx),
            },
        );
    }
}

/// The status for a batch tag write's skipped auto-tags: the core's refusal when there is
/// one, a count when there are several, `None` when nothing was skipped.
pub fn skipped_line(out: &TagBatchOutcome) -> Option<String> {
    match out.skipped.as_slice() {
        [] => None,
        [one] => Some(one.to_string()),
        many => Some(format!("Skipped {} auto-tags: the catalog assigns them, not by hand", many.len())),
    }
}

/// Paste's status: what was pasted, and the auto-tags it skipped.
pub fn paste_line(out: &TagBatchOutcome, n_targets: usize) -> String {
    let mut line = format!("Pasted {} tag(s) onto {} photo(s)", out.tags_written, n_targets);
    match out.skipped.len() {
        0 => {}
        1 => line.push_str(&format!("; skipped auto-tag {}", out.skipped[0].path)),
        n => line.push_str(&format!("; skipped {n} auto-tags")),
    }
    line
}

/// [`run`] under a guard the caller took earlier — a dialog's, from when it opened over the
/// tree its ids came from ([`bind_dialog`]).
pub fn run_as<V: 'static, R: Send + 'static>(
    tags: &Entity<TagsState>,
    guard: &CatalogGuard,
    cx: &mut Context<V>,
    mutates: bool,
    work: impl FnOnce(&Catalog) -> chairphoto_core::catalog::Result<R> + Send + 'static,
    then: impl FnOnce(&mut V, Result<R, String>, &mut Context<V>) + 'static,
) {
    let state = tags.read(cx).app.clone();
    run_on(tags.clone(), state, *guard, cx, mutates, work, then);
}

/// Run `work` against the catalog the tree was read from, on GPUI's background executor, and
/// hand its result to `then` on the view `cx` belongs to — only while the catalog it was
/// started against is still open (see the module docs). The work runs under the guard's
/// catalog identity (`with_catalog_as`: `CATALOG_CHANGED` once another catalog is open); a
/// read before any tree was read runs against the open catalog, a write then fails closed.
/// `mutates`: a successful write calls [`TagsState::changed`] — also when the view that asked
/// has since closed.
pub fn run<V: 'static, R: Send + 'static>(
    tags: &Entity<TagsState>,
    cx: &mut Context<V>,
    mutates: bool,
    work: impl FnOnce(&Catalog) -> chairphoto_core::catalog::Result<R> + Send + 'static,
    then: impl FnOnce(&mut V, Result<R, String>, &mut Context<V>) + 'static,
) {
    let (state, guard) = {
        let t = tags.read(cx);
        (t.app.clone(), t.guard())
    };
    run_on(tags.clone(), state, guard, cx, mutates, work, then);
}

fn run_on<V: 'static, R: Send + 'static>(
    tags: Entity<TagsState>,
    state: AppState,
    guard: CatalogGuard,
    cx: &mut Context<V>,
    mutates: bool,
    work: impl FnOnce(&Catalog) -> chairphoto_core::catalog::Result<R> + Send + 'static,
    then: impl FnOnce(&mut V, Result<R, String>, &mut Context<V>) + 'static,
) {
    let job = cx.background_executor().spawn(async move {
        match guard.identity {
            Some(identity) => with_catalog_as(&state, identity, work),
            None if !mutates => with_catalog(&state, work),
            None => Err(CATALOG_CHANGED.to_string()),
        }
    });
    cx.spawn(async move |this, cx| {
        let result = job.await;
        let current = tags.read_with(cx, |t, _| t.epoch == guard.epoch);
        if !current {
            return; // another catalog's answer
        }
        let wrote = mutates && result.is_ok();
        this.update(cx, |view, cx| then(view, result, cx)).ok();
        if wrote {
            tags.update(cx, |t, cx| t.changed(cx));
        }
    })
    .detach();
}
