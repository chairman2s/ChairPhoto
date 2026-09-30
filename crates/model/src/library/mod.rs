//! The Library surface's model: the query result ([`query`]) and the session around it —
//! scope, selection, catalog-switch reset. Ports of `src/modules/libraryQuery.ts` and
//! `src/modules/librarySession.ts`.
//!
//! The row, query and badge types are chairphoto-core's own (`Photo`, `PhotoQuery`,
//! `PhotoPage`, `ImportBatch`, `StorageStatus`, …): the TypeScript types were those backend
//! DTOs mirrored field for field.

pub mod query;

#[cfg(test)]
pub(crate) mod test_support {
    use chairphoto_core::catalog::{Photo, PhotoPage, PickState};

    /// A bare photo row, as the vitest `photo(id)` fixture built it.
    pub fn photo(id: i64) -> Photo {
        Photo {
            id,
            uuid: format!("uuid-{id}"),
            path: format!("photo{id}.jpg"),
            rating: 0,
            label: String::new(),
            pick_state: PickState::None,
            capture_time: None,
            width: None,
            height: None,
            camera_model: None,
            lens: None,
            aperture: None,
            shutter_speed: None,
            iso: None,
            external_editors: String::new(),
            thumbnail_path: None,
            stack_count: 0,
            stack_parent_id: None,
            metadata_ready: 1,
            sharpness: None,
            sharpness_method: None,
            burst_flag: None,
            version_count: 0,
            cover_token: None,
        }
    }

    /// One page holding `ids`, with `total` matches.
    pub fn page(ids: &[i64], total: usize) -> PhotoPage {
        PhotoPage { photos: ids.iter().map(|&id| photo(id)).collect(), offset: 0, total }
    }
}
