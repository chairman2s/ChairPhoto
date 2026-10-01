//! [`AlbumsState`]: the write path of the albums and smart-albums sections and their
//! dialogs. The lists themselves are the shell's (`Lists::albums`, `Lists::smart_albums`,
//! re-read whenever the catalog is); this entity runs the writes keyed by their ids.
//!
//! **Catalog identity.** Album, smart-album, photo, tag and batch ids are per catalog. Every
//! write runs under the identity of the catalog the ids were read from (`with_catalog_as`:
//! `CATALOG_CHANGED` once another catalog is open, whether or not `catalog:switched` has
//! arrived) — the lists' ([`ShellState::lists_from`]), and for "add the selection" also the
//! rows' ([`ShellState::rows_from`]), which must be the same catalog. The work runs on GPUI's
//! background executor, never the UI thread, and its result lands only while no
//! `catalog:switched` arrived since it started ([`AlbumsState::epoch`]). Dialogs bind when
//! they open ([`bind_dialog`]) and close on a switch or once the lists come from another
//! catalog handle.

use crate::model::{AppModel, AppModelEvent};
use crate::shell::ShellState;
use crate::storage::CloseDialog;
use chairphoto_core::app::{with_catalog_as, AppState, CatalogIdentity, CoreEvent, CATALOG_CHANGED};
use chairphoto_core::catalog::Catalog;
use gpui_kit::{App, Context, Entity, EventEmitter, Subscription, WeakEntity};
use std::cell::Cell;
use std::rc::Rc;

/// The dialog an albums view opened last — what a test drives.
#[derive(Clone)]
pub enum AlbumDialog {
    Prompt(WeakEntity<super::prompt::NamePrompt>),
    SmartEditor(WeakEntity<super::smart_editor::SmartAlbumEditor>),
}

pub struct AlbumsState {
    app: AppState,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    /// Bumped by every `catalog:switched`.
    epoch: u64,
    /// The dialog opened last (tests drive it through this).
    pub last_dialog: Option<AlbumDialog>,
    _model_events: Subscription,
}

impl AlbumsState {
    pub fn new(model: &Entity<AppModel>, shell: &Entity<ShellState>, cx: &mut Context<Self>) -> Self {
        let app = model.read(cx).state().clone();
        let _model_events = cx.subscribe(model, |this, _, event: &AppModelEvent, cx| {
            if let AppModelEvent::Core(CoreEvent::CatalogSwitched(_)) = event {
                this.epoch += 1;
                cx.notify();
            }
        });
        AlbumsState { app, model: model.clone(), shell: shell.clone(), epoch: 0, last_dialog: None, _model_events }
    }

    pub fn app_state(&self) -> &AppState {
        &self.app
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn shell(&self) -> &Entity<ShellState> {
        &self.shell
    }

    fn status(&self, line: String, cx: &mut App) {
        self.model.update(cx, |m, cx| m.set_status(line, cx));
    }

    /// A write landed: the model re-reads, and its `CatalogRead` makes the shell re-read the
    /// lists, the counts and the rows (an album filter's rows change with its membership).
    pub fn changed(&self, cx: &mut App) {
        self.model.update(cx, |m, cx| m.refresh(cx));
    }

    /// Run a write keyed by the shell's list ids, bound to the catalog they were read from;
    /// a failure is the status line `"{what} failed: …"`.
    fn write(
        &mut self,
        what: &'static str,
        work: impl FnOnce(&Catalog) -> chairphoto_core::catalog::Result<()> + Send + 'static,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let from = self.shell.read(cx).lists_from();
        self.write_as(from, what, work, then, cx);
    }

    /// [`Self::write`] bound to `from`, the catalog the ids were read from when the user
    /// acted — a delete confirm captures it as it opens, so an OK after a switch fails closed.
    fn write_as(
        &mut self,
        from: Option<CatalogIdentity>,
        what: &'static str,
        work: impl FnOnce(&Catalog) -> chairphoto_core::catalog::Result<()> + Send + 'static,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = from else {
            self.status("The albums are still loading; try again.".into(), cx);
            return;
        };
        let (albums, state, epoch) = (cx.entity(), self.app.clone(), self.epoch);
        run_on(albums, state, epoch, from, cx, true, work, move |this: &mut Self, result, cx| match result {
            Ok(()) => then(this, cx),
            Err(e) => this.status(format!("{what} failed: {e}"), cx),
        });
    }

    /// ＋ New album (AlbumsPanel `onNew`).
    pub fn create_album(&mut self, name: String, cx: &mut Context<Self>) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        self.write("Creating the album", move |c| c.create_album(&name).map(drop), |_, _| {}, cx);
    }

    /// ⚙ Rename.
    pub fn rename_album(&mut self, id: i64, name: String, cx: &mut Context<Self>) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        self.write("Renaming the album", move |c| c.rename_album(id, &name), |_, _| {}, cx);
    }

    /// ✕ Delete (after its confirm): photos are not deleted; an active filter on it clears.
    /// `from`: the lists' catalog when ✕ was clicked ([`ShellState::lists_from`]).
    pub fn delete_album(&mut self, id: i64, from: Option<CatalogIdentity>, cx: &mut Context<Self>) {
        self.write_as(
            from,
            "Deleting the album",
            move |c| c.delete_album(id),
            move |this, cx| {
                this.shell.update(cx, |s, cx| {
                    if s.library.scope().album_id == Some(id) {
                        s.update_scope(cx, |l| l.select_album(None));
                    }
                })
            },
            cx,
        );
    }

    /// "+N": add the selection to the album (App.tsx `addPhotosToAlbum(selectedIds)`). The
    /// selection's ids and the album's must come from the same catalog.
    pub fn add_selection(&mut self, album_id: i64, cx: &mut Context<Self>) {
        let (ids, rows_from, lists_from) = {
            let s = self.shell.read(cx);
            (s.library.selection().ids.to_vec(), s.rows_from(), s.lists_from())
        };
        if ids.is_empty() {
            return;
        }
        if rows_from.is_none() || rows_from != lists_from {
            self.status(format!("Adding to the album failed: {CATALOG_CHANGED}"), cx);
            return;
        }
        let n = ids.len();
        self.write(
            "Adding to the album",
            move |c| c.add_photos_to_album(album_id, &ids),
            move |this, cx| this.status(format!("Added {n} photo(s) to the album."), cx),
            cx,
        );
    }

    /// ✎ Rename a smart album.
    pub fn rename_smart_album(&mut self, id: i64, name: String, cx: &mut Context<Self>) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        self.write("Renaming the smart album", move |c| c.rename_smart_album(id, &name), |_, _| {}, cx);
    }

    /// ✕ Delete a smart album (after its confirm); an active filter on it clears.
    /// `from`: the lists' catalog when ✕ was clicked.
    pub fn delete_smart_album(&mut self, id: i64, from: Option<CatalogIdentity>, cx: &mut Context<Self>) {
        self.write_as(
            from,
            "Deleting the smart album",
            move |c| c.delete_smart_album(id),
            move |this, cx| {
                this.shell.update(cx, |s, cx| {
                    if s.library.scope().smart_album_id == Some(id) {
                        s.update_scope(cx, |l| l.select_smart_album(None));
                    }
                })
            },
            cx,
        );
    }
}

/// Run `work` against catalog `from` on GPUI's background executor (`with_catalog_as`) and
/// hand its result to `then` on the view `cx` belongs to — only while no `catalog:switched`
/// arrived since it started. `mutates`: a successful write re-reads the catalog
/// ([`AlbumsState::changed`]), also when the view that asked has since closed.
pub fn run_bound<V: 'static, R: Send + 'static>(
    albums: &Entity<AlbumsState>,
    from: CatalogIdentity,
    cx: &mut Context<V>,
    mutates: bool,
    work: impl FnOnce(&Catalog) -> chairphoto_core::catalog::Result<R> + Send + 'static,
    then: impl FnOnce(&mut V, Result<R, String>, &mut Context<V>) + 'static,
) {
    let (state, epoch) = {
        let a = albums.read(cx);
        (a.app.clone(), a.epoch)
    };
    run_on(albums.clone(), state, epoch, from, cx, mutates, work, then);
}

/// [`run_bound`] with the state and epoch already read — what [`AlbumsState`]'s own writes
/// use, since it cannot read itself while it is being updated.
#[allow(clippy::too_many_arguments)]
fn run_on<V: 'static, R: Send + 'static>(
    albums: Entity<AlbumsState>,
    state: AppState,
    epoch: u64,
    from: CatalogIdentity,
    cx: &mut Context<V>,
    mutates: bool,
    work: impl FnOnce(&Catalog) -> chairphoto_core::catalog::Result<R> + Send + 'static,
    then: impl FnOnce(&mut V, Result<R, String>, &mut Context<V>) + 'static,
) {
    let job = cx.background_executor().spawn(async move { with_catalog_as(&state, from, work) });
    cx.spawn(async move |this, cx| {
        let result = job.await;
        if albums.read_with(cx, |a, _| a.epoch) != epoch {
            return; // another catalog's answer
        }
        let wrote = mutates && result.is_ok();
        this.update(cx, |view, cx| then(view, result, cx)).ok();
        if wrote {
            albums.update(cx, |a, cx| a.changed(cx));
        }
    })
    .detach();
}

/// Close a dialog (one [`CloseDialog`]) once the catalog its ids came from (`from`) is no
/// longer the one shown: a `catalog:switched`, or lists read from another catalog handle.
/// `which` picks the identity to compare: the lists' or the rows'.
pub fn bind_dialog<V: EventEmitter<CloseDialog> + 'static>(
    albums: &Entity<AlbumsState>,
    from: Option<CatalogIdentity>,
    which: fn(&ShellState) -> Option<CatalogIdentity>,
    cx: &mut Context<V>,
) -> Vec<Subscription> {
    let epoch = albums.read(cx).epoch;
    let shell = albums.read(cx).shell.clone();
    let closed = Rc::new(Cell::new(false));
    let on_albums = {
        let closed = closed.clone();
        cx.observe(albums, move |_, albums, cx| {
            if !closed.get() && albums.read(cx).epoch != epoch {
                closed.set(true);
                cx.emit(CloseDialog);
            }
        })
    };
    let on_shell = cx.observe(&shell, move |_, shell, cx| {
        let now = which(shell.read(cx));
        if !closed.get() && now.is_some() && now != from {
            closed.set(true);
            cx.emit(CloseDialog);
        }
    });
    vec![on_albums, on_shell]
}
