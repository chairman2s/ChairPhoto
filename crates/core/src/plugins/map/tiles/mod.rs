//! The headless half of the map widget (docs/plans/gpui/map.md, ticket #119): everything a
//! slippy map needs that is not drawing.
//!
//! - [`math`] — Web Mercator with fractional zoom: the viewport, pan, zoom about a point,
//!   the visible tile grid, fit-to-points.
//! - [`source`] — the tile URL template, checked and kept to the OSM tile policy (exact
//!   default URL, no `{s}` subdomains), and the host consent is asked for.
//! - [`cache`] — the disk cache (body + validators + expiry), size-capped.
//! - [`fetch`] — cache first, then a conditional GET with ChairPhoto's User-Agent, at most
//!   four requests in flight.
//!
//! Nothing here decides *whether* to fetch: the user's per-host consent (decision #118) is
//! checked by the app before it asks for a tile.

pub mod cache;
pub mod fetch;
pub mod math;
pub mod source;

pub use math::{TileKey, TilePlacement, Viewport};
pub use source::TileSource;
