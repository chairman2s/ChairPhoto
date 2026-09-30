//! Working images for renders outside the Develop session (docs/plans/raw-foundation,
//! slice 5): an engine-2 version on the Library loupe, a cover thumbnail, a render the
//! session did not ask for. An engine-2 record means "this RAW through this pipeline", so
//! without the session's image it is rendered from one loaded here — never from the
//! camera preview.
//!
//! Bounded: the session's own image is used when it holds the photo; otherwise one image
//! is loaded at a time (the slot's lock serializes loads) from the `.rawf` cache or the
//! decoder, and only the most recent is kept — for [`KEEP`], so the loupe's zoom render
//! right after its fit render does not load again. Its token carries a generation with the
//! top bit set, so it never names a session image, and a fresh one per load, so the
//! framed-base cache can never serve a previous file's pixels.
//!
//! Lock order: the session lookup (`RESIDENT`, a leaf) happens before this slot is taken,
//! never under it.

use super::{with_resident, working_image_from};
use crate::plugins::edit::{SourceToken, WorkingImage};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the last offline image stays after its load.
pub const KEEP: Duration = Duration::from_secs(60);
const OFFLINE_GENERATION: u64 = 1 << 63;

struct Offline {
    key: super::cache::CacheKey,
    photo_id: i64,
    token: SourceToken,
    image: Arc<WorkingImage>,
}

static SLOT: Mutex<Option<Offline>> = Mutex::new(None);
static NEXT: AtomicU64 = AtomicU64::new(0);

/// The working image to render `photo_id` (at `path`) from outside the session: the
/// session's when resident, else the kept offline image when it is this file as it is on
/// disk now, else a fresh load (which first drops the previous offline image).
pub fn working_image_for(
    photo_id: i64,
    path: &Path,
    cache_budget_bytes: u64,
) -> Result<(SourceToken, Arc<WorkingImage>), String> {
    if let Some(found) = with_resident(|r| r.find_photo(photo_id)) {
        return Ok(found);
    }
    let key = super::cache::CacheKey::for_file(path).ok_or_else(|| format!("cannot read {}", path.display()))?;
    let mut slot = SLOT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(o) = slot.as_ref() {
        if o.photo_id == photo_id && o.key == key {
            return Ok((o.token.clone(), o.image.clone()));
        }
    }
    *slot = None; // one offline image in memory at a time, even while the next loads
    let t = std::time::Instant::now();
    let (decoded, from) = super::session::load_linear(path, &AtomicBool::new(false), cache_budget_bytes)?;
    let image = Arc::new(working_image_from(decoded));
    let token = SourceToken::Working { photo_id, generation: OFFLINE_GENERATION | NEXT.fetch_add(1, Ordering::Relaxed) };
    *slot = Some(Offline { key, photo_id, token: token.clone(), image: image.clone() });
    drop(slot);
    eprintln!("develop: offline working image for photo {photo_id} from {from} in {:.2?}", t.elapsed());
    let mine = token.clone();
    let _ = std::thread::Builder::new().name("develop-offline-expiry".into()).spawn(move || {
        std::thread::sleep(KEEP);
        expire(&mine);
    });
    Ok((token, image))
}

/// Drop the offline image if it is still the one `token` names.
fn expire(token: &SourceToken) {
    let mut slot = SLOT.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|o| &o.token == token) {
        *slot = None;
    }
}

/// Drop the offline image (an ownership change in Develop, a catalog switch).
pub fn clear() {
    *SLOT.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether an offline image is held (tests).
#[cfg(test)]
pub fn held() -> Option<SourceToken> {
    SLOT.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|o| o.token.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::develop::{serial, test_image};

    #[test]
    fn the_sessions_image_is_used_and_nothing_is_loaded() {
        let _serial = serial();
        crate::develop::release_all();
        let token = SourceToken::Working { photo_id: 7, generation: 3 };
        assert!(with_resident(|r| r.insert(token.clone(), test_image(4, 4))));
        // A path that does not exist: had it tried to load, this would be an error.
        let (t, _) = working_image_for(7, Path::new("/nonexistent/x.arw"), 0).unwrap();
        assert_eq!(t, token);
        assert!(held().is_none());
        crate::develop::release_all();
    }

    #[test]
    fn a_missing_file_is_an_error_and_holds_nothing() {
        let _serial = serial();
        crate::develop::release_all();
        assert!(working_image_for(8, Path::new("/nonexistent/y.arw"), 0).is_err());
        assert!(held().is_none());
    }

    #[test]
    fn expiry_drops_only_the_image_it_names() {
        let _serial = serial();
        clear();
        let key = super::super::cache::CacheKey::for_file(Path::new("/")).unwrap();
        let a = SourceToken::Working { photo_id: 1, generation: OFFLINE_GENERATION | 900 };
        let b = SourceToken::Working { photo_id: 1, generation: OFFLINE_GENERATION | 901 };
        *SLOT.lock().unwrap() = Some(Offline { key, photo_id: 1, token: b.clone(), image: test_image(2, 2) });
        expire(&a); // an older load's timer
        assert_eq!(held(), Some(b.clone()));
        expire(&b);
        assert!(held().is_none());
    }

    /// With a real RAW (`CHAIRPHOTO_RAW_FIXTURE`): the second request reuses the first load.
    #[test]
    fn a_second_render_reuses_the_loaded_image() {
        let Ok(fixture) = std::env::var("CHAIRPHOTO_RAW_FIXTURE") else {
            println!("SKIPPED: a_second_render_reuses_the_loaded_image — set CHAIRPHOTO_RAW_FIXTURE");
            return;
        };
        let _serial = serial();
        crate::develop::release_all();
        let p = Path::new(&fixture);
        let (t1, i1) = working_image_for(9, p, 0).unwrap();
        let (t2, i2) = working_image_for(9, p, 0).unwrap();
        assert_eq!(t1, t2);
        assert!(Arc::ptr_eq(&i1, &i2));
        clear();
    }
}
