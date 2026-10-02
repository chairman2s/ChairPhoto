//! Recording publications for a front end's publish flow (docs/publications.md): the one
//! implementation the GPUI publish targets (Snapchat, and the #124 OAuth services) call.
//! Feature-independent — a publication is catalog state, not part of any publishing backend.

use super::{AppState, CatalogIdentity};

/// Record that each `(photo id, version id)` of `published` went to `marker` (the module's
/// publication marker), with `url` when the service returned one — in one catalog lock hold
/// and one transaction (a failure on any row records none), and only while `catalog` (the one
/// the ids were read from) is still open: otherwise nothing is written and it answers
/// [`super::CATALOG_CHANGED`]. Blocking: call it on a worker.
pub fn record_publications_as(
    state: &AppState,
    catalog: CatalogIdentity,
    published: &[(i64, Option<i64>)],
    marker: &str,
    url: Option<&str>,
) -> Result<(), String> {
    super::with_catalog_as(state, catalog, |c| c.record_publications(published, marker, url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::CATALOG_CHANGED;
    use std::path::Path;

    fn catalog_with_photos(dir: &Path, n: usize) -> (AppState, Vec<i64>) {
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let state = AppState::default();
        let c = crate::catalog::Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let ids = (0..n)
            .map(|i| {
                let p = root.join(format!("IMG_{i}.jpg"));
                std::fs::write(&p, b"jpeg").unwrap();
                c.upsert_photo(&p, None, 0, 6).unwrap().id
            })
            .collect();
        *state.catalog.lock().unwrap() = Some(c);
        (state, ids)
    }

    fn platforms(state: &AppState, id: i64) -> Vec<String> {
        crate::app::with_catalog(state, |c| c.list_publications(id)).unwrap().into_iter().map(|p| p.platform).collect()
    }

    /// Publications are recorded under the module's marker, for exactly the given
    /// (photo, version) pairs — and not at all once another catalog is open.
    #[test]
    fn publications_are_recorded_only_into_the_catalog_the_ids_came_from() {
        let dir = crate::test_support::TestTmpDir::new("publishing-record");
        let (state, ids) = catalog_with_photos(&dir, 1);
        let read = crate::app::catalog_identity(&state).unwrap();
        record_publications_as(&state, read, &[(ids[0], None)], "snapchat", None).unwrap();
        assert_eq!(platforms(&state, ids[0]), ["snapchat"]);

        // Another catalog with a colliding photo id.
        let other = crate::test_support::TestTmpDir::new("publishing-record-b");
        let (b, ids_b) = catalog_with_photos(&other, 1);
        assert_eq!(ids_b, ids, "the ids collide");
        let catalog_b = b.catalog.lock().unwrap().take();
        *state.catalog.lock().unwrap() = catalog_b;
        let err = record_publications_as(&state, read, &[(ids[0], None)], "snapchat", None).unwrap_err();
        assert_eq!(err, CATALOG_CHANGED);
        assert!(platforms(&state, ids[0]).is_empty(), "nothing lands in the catalog that opened since");
    }

    /// One transaction: when a later row fails (a photo deleted since it was read), the
    /// earlier rows are not left recorded either.
    #[test]
    fn a_failing_row_records_none() {
        let dir = crate::test_support::TestTmpDir::new("publishing-record-tx");
        let (state, ids) = catalog_with_photos(&dir, 3);
        let read = crate::app::catalog_identity(&state).unwrap();
        crate::app::with_catalog(&state, |c| c.remove_photo(ids[2])).unwrap();
        let published = [(ids[0], None), (ids[1], None), (ids[2], None)];
        let err = record_publications_as(&state, read, &published, "snapchat", None);
        assert!(err.is_err(), "the deleted photo's row fails");
        for &id in &ids[..2] {
            assert!(platforms(&state, id).is_empty(), "photo {id} was recorded although the batch failed");
        }
    }
}
