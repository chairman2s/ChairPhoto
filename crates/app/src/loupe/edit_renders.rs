//! [`EditRenders`]: still renders of edit records — the loupe's active version (its fit and
//! hi-res renders), the Duel's two variants and the Proof sheet's dozen — through the decode
//! pool as `JobKey::Edit`, handed to GPUI as BGRA textures (`media::render_edit_image`, no
//! encode). The React views loaded `edit://` URLs for the same jobs.
//!
//! - **One wanted set.** The owner says which renders it wants now ([`EditRenders::want`]),
//!   most urgent first; anything else is no longer wanted: a queued job is cancelled in the
//!   pool, a finished one's texture is released, and whatever a running one produces later is
//!   dropped by generation — never shown.
//! - **Catalog-bound.** Every [`EditJob`] carries the [`CatalogIdentity`] its photo id was read
//!   from (`EditJob::catalog`, #251), so a render for one catalog's photo id can never be
//!   merged into, or handed to, a request for another catalog's. The worker renders it only
//!   while that catalog is still open, checked under the catalog lock: in a switch's window
//!   (the new catalog published, `catalog:switched` not yet here) it answers
//!   [`CATALOG_CHANGED`] instead of another photo's pixels, and that answer is kept as
//!   [`RenderState::Stale`] — nothing drawn, no failure shown, not asked again for that job.

use crate::image_store::{Loaded, Submit};
use chairphoto_core::app::{CatalogIdentity, CATALOG_CHANGED};
use chairphoto_core::image_pool::{EditJob, JobKey};
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt as _;
use gpui_kit::{App, Context, RenderImage, SharedString, Task};
use std::collections::HashMap;
use std::sync::Arc;

/// Where one render stands.
#[derive(Clone)]
pub enum RenderState {
    Ready(Arc<RenderImage>),
    Rendering,
    Failed(SharedString),
    /// Asked for a catalog that was no longer open when it rendered (see the module docs):
    /// nothing to draw — never another catalog's photo — and nothing failed.
    Stale,
    /// Not wanted.
    Absent,
}

enum Entry {
    Pending(u64),
    Ready(Arc<RenderImage>),
    Failed(SharedString),
    Stale,
}

struct Done {
    job: EditJob,
    generation: u64,
    result: Result<Loaded, String>,
}

/// See the module docs.
pub struct EditRenders {
    pool: Arc<dyn Submit>,
    entries: HashMap<EditJob, Entry>,
    generation: u64,
    done: UnboundedSender<Done>,
    /// Results dropped because their job was no longer wanted (tests).
    stale_dropped: u64,
    _drain: Task<()>,
}

impl EditRenders {
    pub fn new(pool: Arc<dyn Submit>, cx: &mut Context<Self>) -> Self {
        let (done, mut rx) = unbounded::<Done>();
        let _drain = cx.spawn(async move |this, cx| {
            while let Some(d) = rx.next().await {
                if this.update(cx, |r, cx| r.complete(d, cx)).is_err() {
                    break;
                }
            }
        });
        EditRenders { pool, entries: HashMap::new(), generation: 0, done, stale_dropped: 0, _drain }
    }

    pub fn get(&self, job: &EditJob) -> RenderState {
        match self.entries.get(job) {
            Some(Entry::Ready(image)) => RenderState::Ready(image.clone()),
            Some(Entry::Pending(_)) => RenderState::Rendering,
            Some(Entry::Failed(e)) => RenderState::Failed(e.clone()),
            Some(Entry::Stale) => RenderState::Stale,
            None => RenderState::Absent,
        }
    }

    pub fn stale_dropped(&self) -> u64 {
        self.stale_dropped
    }

    /// Want exactly `jobs`, most urgent first: new ones go to the pool as one batch (the first
    /// on top of its stack); every other entry is dropped (see the module docs). Calling it
    /// again with the same set sends nothing.
    pub fn want(&mut self, jobs: &[EditJob], cx: &mut Context<Self>) {
        let unwanted: Vec<EditJob> = self.entries.keys().filter(|k| !jobs.contains(k)).cloned().collect();
        let mut released = Vec::new();
        for job in unwanted {
            match self.entries.remove(&job) {
                Some(Entry::Pending(_)) => {
                    self.pool.cancel(&JobKey::Edit(job));
                }
                Some(Entry::Ready(image)) => released.push(image),
                _ => {}
            }
        }
        let mut batch = Vec::new();
        for job in jobs {
            if self.entries.contains_key(job) {
                continue;
            }
            self.generation += 1;
            let generation = self.generation;
            self.entries.insert(job.clone(), Entry::Pending(generation));
            let (done, key) = (self.done.clone(), job.clone());
            batch.push((
                JobKey::Edit(job.clone()),
                Box::new(move |result| {
                    let _ = done.unbounded_send(Done { job: key, generation, result });
                }) as chairphoto_core::image_pool::Respond<Loaded>,
            ));
        }
        if !batch.is_empty() {
            self.pool.submit_batch(batch);
        }
        release(released, cx);
    }

    fn complete(&mut self, done: Done, cx: &mut Context<Self>) {
        match self.entries.get(&done.job) {
            Some(Entry::Pending(g)) if *g == done.generation => {}
            _ => {
                self.stale_dropped += 1;
                return;
            }
        }
        let entry = match done.result {
            Ok(loaded) => Entry::Ready(loaded.image),
            // The worker refused it: its catalog is no longer the open one (#251).
            Err(e) if e == CATALOG_CHANGED => Entry::Stale,
            Err(e) => {
                eprintln!("edit render: photo {}: {e}", done.job.photo_id);
                Entry::Failed(e.into())
            }
        };
        self.entries.insert(done.job, entry);
        cx.notify();
    }
}

/// Remove released textures from every window's atlas, outside any window update.
fn release(images: Vec<Arc<RenderImage>>, cx: &mut Context<EditRenders>) {
    if images.is_empty() {
        return;
    }
    cx.defer(move |cx: &mut App| {
        for image in images {
            cx.drop_image(image, None);
        }
    });
}

/// The render of `edit_json` for `photo_id` of the catalog `catalog` from the camera preview:
/// `max_edge` 0 with `hi_res` is the native-size render the loupe zooms into.
pub fn preview_job(photo_id: i64, edit_json: &str, max_edge: u32, hi_res: bool, catalog: CatalogIdentity) -> EditJob {
    EditJob {
        photo_id,
        edit_json: edit_json.to_string(),
        max_edge,
        hi_res,
        base_only: false,
        source: chairphoto_core::plugins::edit::SourceToken::Preview,
        clip: false,
        catalog,
    }
}

/// The catalog binding (#251) through the real worker body (`image_store::runner`) on the job
/// this module built, answered through a hand-driven pool: the check is the core's, the
/// handling of its answer is this module's.
#[cfg(test)]
mod tests {
    use super::{preview_job, EditRenders, RenderState};
    use crate::image_tests::{pixels, FakePool};
    use crate::tests::{colliding_catalog, core_switch, deliver_switch, open_catalog_with_photos, start_with_pool, TempDir};
    use chairphoto_core::app::{catalog_identity, CATALOG_CHANGED};
    use chairphoto_core::image_pool::JobKey;
    use gpui_kit::{AppContext as _, TestAppContext};
    use std::sync::Arc;

    /// A render asked for catalog A's photo, running when the core switches to B (whose photo
    /// has the same id): the worker renders nothing and the render is stale — not drawn, not
    /// failed, not asked again. A render asked for B's photo of that id is a different job and
    /// passes the check (it gets as far as B's original, which this test never wrote).
    fn a_render_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
        let dir = TempDir::new(if delivered { "renders-switch-ev" } else { "renders-switch" });
        let pool = Arc::new(FakePool::default());
        let app = start_with_pool(cx, pool.clone());
        let ids = open_catalog_with_photos(&app, &dir, 2, cx);
        let a = catalog_identity(&app.state).unwrap();
        let renders = cx.update(|cx| {
            let pool: Arc<dyn crate::image_store::Submit> = pool.clone();
            cx.new(|cx| EditRenders::new(pool, cx))
        });
        let in_a = preview_job(ids[0], "{\"ev\":1}", 320, false, a);
        renders.update(cx, |r, cx| r.want(&[in_a.clone()], cx));
        let key = JobKey::Edit(in_a.clone());
        pool.start(key.clone()); // on a worker: it cannot be cancelled

        let (b, b_ids) = colliding_catalog(&dir, "b", 2);
        assert_eq!(b_ids, ids, "the ids collide");
        core_switch(&app, b);
        if delivered {
            deliver_switch(&app, cx);
        }
        let b_identity = catalog_identity(&app.state).unwrap();
        assert_ne!(a, b_identity);

        let run = crate::image_store::runner(app.state.clone());
        let answer = run(key.clone()).map(|_| ());
        assert_eq!(answer, Err(CATALOG_CHANGED.to_string()), "delivered={delivered}: rendered in B");
        pool.finish(&key, run(key.clone()));
        cx.run_until_parked();
        let asked = pool.batches.lock().unwrap().len();
        renders.update(cx, |r, cx| r.want(&[in_a.clone()], cx));
        renders.read_with(cx, |r, _| {
            assert!(matches!(r.get(&in_a), RenderState::Stale), "delivered={delivered}: stale, not drawn or failed");
        });
        assert_eq!(pool.batches.lock().unwrap().len(), asked, "a stale render is not asked again");

        // Asked for B's photo of the same id: another job, rendered in B.
        let in_b = preview_job(ids[0], "{\"ev\":1}", 320, false, b_identity);
        assert_ne!(in_a, in_b);
        renders.update(cx, |r, cx| r.want(&[in_b.clone()], cx));
        let key_b = JobKey::Edit(in_b.clone());
        let e = run(key_b.clone()).map(|_| ()).unwrap_err();
        assert!(e.contains(&format!("no reachable copy of photo {}", ids[0])), "resolved in B: {e}");
        pool.finish(&key_b, Ok(pixels(4, 4)));
        cx.run_until_parked();
        renders.read_with(cx, |r, _| assert!(matches!(r.get(&in_b), RenderState::Ready(_))));
    }

    #[gpui_kit::test]
    fn a_render_asked_before_an_unannounced_switch_draws_nothing_from_the_new_catalog(cx: &mut TestAppContext) {
        a_render_across_a_switch(false, cx);
    }

    #[gpui_kit::test]
    fn a_render_asked_before_an_announced_switch_draws_nothing_from_the_new_catalog(cx: &mut TestAppContext) {
        a_render_across_a_switch(true, cx);
    }
}
