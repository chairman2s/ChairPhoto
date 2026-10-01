//! Saving a photo's authored IPTC fields: the catalog, then the photo's XMP sidecar.
//!
//! Shared by the Tauri `set_iptc` command and the GPUI inspector (gpui #108).

use super::{with_catalog, AppState};
use crate::catalog::IptcFields;

/// Store `fields` in the catalog, then write them to the photo's XMP sidecar (merge-safe:
/// `xmp::write_iptc` touches only the IPTC elements it manages). The sidecar sits next to
/// the original the location resolver finds; with no reachable copy the catalog keeps the
/// values and the error says why the sidecar was not written.
///
/// Blocking: the catalog lock is held only for the store and the path lookup, and the
/// sidecar's read-modify-write runs after it is released. Call it off the UI thread.
pub fn save_iptc(state: &AppState, photo_id: i64, fields: &IptcFields) -> Result<(), String> {
    let original = with_catalog(state, |c| store(c, photo_id, fields))?;
    crate::xmp::write_iptc(&original, fields)
}

/// [`save_iptc`] of a photo read from the catalog `expected` names: the store and the path
/// lookup fail closed with `CATALOG_CHANGED` once another catalog is open, so neither the
/// other catalog's row with the same id nor its photo's sidecar is written.
pub fn save_iptc_as(
    state: &AppState,
    expected: super::CatalogIdentity,
    photo_id: i64,
    fields: &IptcFields,
) -> Result<(), String> {
    let original = super::with_catalog_as(state, expected, |c| store(c, photo_id, fields))?;
    crate::xmp::write_iptc(&original, fields)
}

fn store(c: &crate::catalog::Catalog, photo_id: i64, fields: &IptcFields) -> crate::catalog::Result<std::path::PathBuf> {
    c.set_iptc(photo_id, fields)?;
    c.require_photo_path(photo_id)
}
