//! [`EditRenders`]: still renders of edit records — the loupe's active version (its fit and
//! hi-res renders), the Duel's two variants and the Proof sheet's dozen — through the decode
//! pool as `JobKey::Edit`, handed to GPUI as BGRA textures (`media::render_edit_image`, no
//! encode). The React views loaded `edit://` URLs for the same jobs.
//!
//! - **One wanted set.** The owner says which renders it wants now ([`EditRenders::want`]),
//!   most urgent first; anything else is no longer wanted: a queued job is cancelled in the
//!   pool, a finished one's texture is released, and whatever a running one produces later is
//!   dropped by generation — never shown.
//! - **Catalog-safe by key.** Every [`EditJob`] carries the catalog epoch it was asked under,
//!   so a render for one catalog's photo id can never be merged into, or handed to, a request
//!   for another catalog's.

use crate::image_store::{Loaded, Submit};
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
    /// Not wanted.
    Absent,
}

enum Entry {
    Pending(u64),
    Ready(Arc<RenderImage>),
    Failed(SharedString),
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

/// The render of `edit_json` for `photo_id` from the camera preview: `max_edge` 0 with
/// `hi_res` is the native-size render the loupe zooms into.
pub fn preview_job(photo_id: i64, edit_json: &str, max_edge: u32, hi_res: bool, catalog_epoch: u64) -> EditJob {
    EditJob {
        photo_id,
        edit_json: edit_json.to_string(),
        max_edge,
        hi_res,
        base_only: false,
        source: chairphoto_core::plugins::edit::SourceToken::Preview,
        clip: false,
        catalog_epoch,
    }
}
