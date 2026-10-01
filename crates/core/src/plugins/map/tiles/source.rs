//! A raster tile source: the URL template the user set (`map.tileUrl`), checked and
//! brought in line with the OSM tile policy.
//!
//! - The default is the policy's exact URL, `https://tile.openstreetmap.org/{z}/{x}/{y}.png`.
//!   The React app's old default, `https://{s}.tile.openstreetmap.org/…`, uses the
//!   subdomains the policy says "may be slower or withdrawn without notice": a stored copy of
//!   it reads as the new default.
//! - `{s}` on OSM's own host is dropped with its dot; on any other host it becomes `a`, the
//!   first subdomain every `{s}` provider serves. Requests never spread over subdomains.
//! - Only `http`/`https` URLs with `{z}`, `{x}` and `{y}` are accepted, and no other
//!   placeholder (`{r}`, `{apikey}` …): a template we would fill wrongly is refused, not
//!   guessed at.
//! - [`TileSource::host`] is what the user's per-host consent names (decision #118: ask
//!   before the first request to each tile host).

use super::math::TileKey;

/// The OSM tile policy's exact URL.
pub const DEFAULT_TILE_URL: &str = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
/// The React app's default, which uses OSM's deprecated subdomains.
pub const LEGACY_DEFAULT_TILE_URL: &str = "https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png";
/// OSM's attribution, shown on the map whenever tiles are (ODbL; the policy's
/// "Attribution" requirement). Every OSM-derived source needs it too.
pub const OSM_ATTRIBUTION: &str = "© OpenStreetMap contributors";
/// The deepest zoom tiles are fetched at (Leaflet's `maxZoom: 19`, OSM's own maximum).
pub const MAX_TILE_ZOOM: u8 = 19;

const OSM_HOST: &str = "tile.openstreetmap.org";

/// A checked tile URL template.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TileSource {
    template: String,
    host: String,
}

impl Default for TileSource {
    fn default() -> Self {
        TileSource::parse(DEFAULT_TILE_URL).expect("the default tile URL is valid")
    }
}

impl TileSource {
    /// Check a template. Empty (or the legacy default) means the default.
    pub fn parse(url: &str) -> Result<TileSource, String> {
        let url = url.trim();
        if url.is_empty() || url == LEGACY_DEFAULT_TILE_URL {
            return TileSource::parse(DEFAULT_TILE_URL);
        }
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .ok_or_else(|| "a tile URL must start with https:// or http://".to_string())?;
        let authority = rest.split('/').next().unwrap_or_default();
        let authority = authority.rsplit('@').next().unwrap_or_default();
        if authority.is_empty() || !rest.contains('/') {
            return Err("a tile URL needs a host and a path".into());
        }
        let mut template = url.to_string();
        if authority.contains("{s}") {
            let without = authority.replace("{s}.", "");
            if without.eq_ignore_ascii_case(OSM_HOST) {
                template = template.replacen("{s}.", "", 1);
            }
        }
        template = template.replace("{s}", "a");
        for needed in ["{z}", "{x}", "{y}"] {
            if !template.contains(needed) {
                return Err(format!("a tile URL needs the {{z}}, {{x}} and {{y}} placeholders (missing {needed})"));
            }
        }
        let leftover = ["{z}", "{x}", "{y}"].iter().fold(template.clone(), |t, p| t.replace(p, ""));
        if let Some(start) = leftover.find('{') {
            let name: String = leftover[start..].chars().take_while(|c| *c != '}').chain(['}']).collect();
            return Err(format!("unsupported placeholder {name} in the tile URL"));
        }
        let host_of = |t: &str| {
            let rest = t.split_once("://").map(|(_, r)| r).unwrap_or(t);
            let authority = rest.split('/').next().unwrap_or_default();
            authority.rsplit('@').next().unwrap_or_default().to_ascii_lowercase()
        };
        let host = host_of(&template);
        Ok(TileSource { template, host })
    }

    /// The template, after [`parse`](Self::parse)'s rewrites.
    pub fn template(&self) -> &str {
        &self.template
    }

    /// The host (with any port) requests go to: what consent is asked and remembered for.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The default OpenStreetMap source.
    pub fn is_default(&self) -> bool {
        self.template == DEFAULT_TILE_URL
    }

    /// The URL of one tile.
    pub fn url(&self, key: TileKey) -> String {
        self.template.replace("{z}", &key.z.to_string()).replace("{x}", &key.x.to_string()).replace("{y}", &key.y.to_string())
    }

    /// A directory name for this source's disk cache: the host, made file-name safe, and a
    /// hash of the whole template (two styles on one host are different tiles).
    pub fn cache_id(&self) -> String {
        use sha2::Digest as _;
        let safe: String =
            self.host.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
        let digest = sha2::Sha256::digest(self.template.as_bytes());
        let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        format!("{safe}-{hex}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_policys_exact_url_and_the_legacy_default_migrates() {
        let d = TileSource::default();
        assert_eq!(d.template(), "https://tile.openstreetmap.org/{z}/{x}/{y}.png");
        assert_eq!(d.host(), "tile.openstreetmap.org");
        assert!(d.is_default());
        assert_eq!(TileSource::parse(LEGACY_DEFAULT_TILE_URL).unwrap(), d);
        assert_eq!(TileSource::parse("  ").unwrap(), d);
        assert_eq!(d.url(TileKey { z: 3, x: 4, y: 5 }), "https://tile.openstreetmap.org/3/4/5.png");
    }

    /// No request ever goes to a `{s}` subdomain: OSM's own host loses it, others get `a`.
    #[test]
    fn subdomains_are_never_used() {
        let osm = TileSource::parse("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png?x=1").unwrap();
        assert_eq!(osm.template(), "https://tile.openstreetmap.org/{z}/{x}/{y}.png?x=1");
        let other = TileSource::parse("https://{s}.tiles.example.org/{z}/{x}/{y}.png").unwrap();
        assert_eq!(other.template(), "https://a.tiles.example.org/{z}/{x}/{y}.png");
        assert_eq!(other.host(), "a.tiles.example.org");
        for s in [osm, other] {
            assert!(!s.url(TileKey { z: 1, x: 0, y: 1 }).contains("{s}"));
        }
    }

    #[test]
    fn hosts_keep_ports_and_drop_credentials() {
        let s = TileSource::parse("http://user:pw@LocalHost:8080/tiles/{z}/{x}/{y}.png").unwrap();
        assert_eq!(s.host(), "localhost:8080");
    }

    #[test]
    fn bad_templates_are_refused_with_the_reason() {
        let err = |u: &str| TileSource::parse(u).unwrap_err();
        assert!(err("ftp://x.org/{z}/{x}/{y}.png").contains("https://"));
        assert!(err("https://x.org/{z}/{x}.png").contains("missing {y}"));
        assert!(err("https://x.org/{z}/{x}/{y}{r}.png").contains("{r}"));
        assert!(err("https://{z}{x}{y}").contains("host and a path"));
    }

    #[test]
    fn cache_ids_are_file_safe_and_differ_per_template() {
        let a = TileSource::parse("https://h.org:81/a/{z}/{x}/{y}.png").unwrap();
        let b = TileSource::parse("https://h.org:81/b/{z}/{x}/{y}.png").unwrap();
        assert!(a.cache_id().starts_with("h.org_81-"), "{}", a.cache_id());
        assert_ne!(a.cache_id(), b.cache_id());
        assert!(a.cache_id().chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)));
    }
}
