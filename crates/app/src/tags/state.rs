//! [`TagsState`]: the one owner of the tag tree with its photo counts (`list_tags`), the
//! in-app tag clipboard, and the write path every tag view goes through.
//!
//! **Reads.** The tree is re-read off the UI thread whenever the [`AppModel`] has read the
//! catalog ([`AppModelEvent::CatalogRead`]: startup, a switch, a scan's end, and every
//! mutation below, which calls [`AppModel::refresh`] — React's `refresh`, which re-ran the
//! library query *and* `listTags`). Only the newest read lands.
//!
//! **Writes** (and the dialogs' reads) go through [`run`]: the work runs on GPUI's background
//! executor, never the UI thread, and its result lands in the view that asked only while the
//! catalog it was asked against is still open. Two fences, because a catalog switch can come
//! between any two steps:
//!
//! - **Under the catalog lock**, a mutating job first checks the open catalog's file is the
//!   one whose tree the view showed ([`CatalogGuard`]); if not, it writes nothing. Tag and
//!   photo ids are per-catalog, so an id from the closed catalog would name some other row.
//! - **On landing**, a result is dropped when `catalog:switched` arrived since the job
//!   started (the [`TagsState::epoch`]).
//!
//! Tag writes are catalog-only: in-library tag assignments write no sidecar
//! (`xmp::write_keywords` has one caller, the export destination — docs/taxonomy.md
//! § Tag maintenance), so nothing here touches XMP.

use crate::model::{AppModel, AppModelEvent};
use chairphoto_core::app::{with_catalog, AppState, CoreEvent};
use chairphoto_core::catalog::{Catalog, CatalogError, TagWithCount};
use chairphoto_model::tag_tree::TagIndex;
use gpui_kit::{App, Context, Entity, Subscription, WeakEntity};
use std::rc::Rc;
use std::path::PathBuf;

/// The open catalog a job was started against: the switch epoch, and the catalog file the
/// tree was read from (`None` until a tree has been read).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogGuard {
    pub epoch: u64,
    pub path: Option<PathBuf>,
}

impl CatalogGuard {
    /// Whether `c` is the catalog this guard was taken on. Checked under the catalog lock.
    pub fn holds(&self, c: &Catalog) -> bool {
        self.path.as_deref() == Some(c.db_path())
    }
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
    /// The catalog file the tree was read from.
    catalog_path: Option<PathBuf>,
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
            catalog_path: None,
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
        CatalogGuard { epoch: self.epoch, path: self.catalog_path.clone() }
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
        self.catalog_path = None;
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
            with_catalog(&state, |c| Ok((c.db_path().to_path_buf(), c.list_tags_with_counts()?)))
        });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |s, cx| {
                if s.generation != generation || s.epoch != epoch {
                    return;
                }
                match result {
                    Ok((path, tags)) => {
                        s.index = Rc::new(TagIndex::new(&tags));
                        s.tags = tags;
                        s.catalog_path = Some(path);
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
        self.run(cx, move |c| c.move_tag(tag_id, parent), |s, result, cx| {
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

    /// Assign a tag to every target photo (the selection, else the active photo).
    pub fn assign(&mut self, targets: Vec<i64>, tag_id: i64, cx: &mut Context<Self>) {
        if targets.is_empty() {
            return;
        }
        self.run(cx, move |c| targets.iter().try_for_each(|&p| c.assign_tag(p, tag_id)), |s, r, cx| {
            if let Err(e) = r {
                s.set_status(format!("Could not tag: {e}"), cx);
            }
        });
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
                targets.iter().try_for_each(|&p| c.assign_tag(p, tag_id))
            },
            |s, r, cx| {
                if let Err(e) = r {
                    s.set_status(format!("Could not tag: {e}"), cx);
                }
            },
        );
    }

    /// Remove a tag from every target photo — the whole selection, not only the active
    /// photo the chips show (App.tsx's `removeFromSelection`).
    pub fn remove(&mut self, targets: Vec<i64>, tag_id: i64, cx: &mut Context<Self>) {
        if targets.is_empty() {
            return;
        }
        self.run(cx, move |c| targets.iter().try_for_each(|&p| c.remove_tag(p, tag_id)), |s, r, cx| {
            if let Err(e) = r {
                s.set_status(format!("Could not remove the tag: {e}"), cx);
            }
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
        let line = format!("Pasted {} tag(s) onto {} photo(s)", clipboard.len(), targets.len());
        self.run(
            cx,
            move |c| {
                for &p in &targets {
                    for &t in &clipboard {
                        c.assign_tag(p, t)?;
                    }
                }
                Ok(())
            },
            move |s, r, cx| match r {
                Ok(()) => s.set_status(line, cx),
                Err(e) => s.set_status(format!("Could not paste tags: {e}"), cx),
            },
        );
    }
}

/// Run `work` against the open catalog on GPUI's background executor and hand its result to
/// `then` on the view `cx` belongs to — only while the catalog it was started against is
/// still open (see the module docs). `mutates`: the work writes, so it runs only if the open
/// catalog is still the guard's (checked under the lock, before `work`), and a successful
/// write calls [`TagsState::changed`] — also when the view that asked has since closed.
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
    let fence = guard.clone();
    let job = cx.background_executor().spawn(async move {
        with_catalog(&state, |c| {
            if mutates && !fence.holds(c) {
                return Err(CatalogError::Tag("the catalog changed; nothing was written".into()));
            }
            work(c)
        })
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
