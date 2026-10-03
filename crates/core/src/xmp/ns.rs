//! The XML namespaces the sidecar readers and writers match elements and attributes by.

pub(super) const NS_X: &str = "adobe:ns:meta/";
pub(super) const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
pub(super) const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
pub(super) const NS_PHOTOSHOP: &str = "http://ns.adobe.com/photoshop/1.0/";
pub(super) const NS_IPTC: &str = "http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/";
pub(super) const NS_LR: &str = "http://ns.adobe.com/lightroom/1.0/";
pub(super) const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
// XMP Basic's qualifier namespace for a qualified `xmp:Identifier` Bag item
// (`rdf:value` + `xmpidq:Scheme`, say) — the qualifier, never a second identifier value.
pub(super) const NS_XMPIDQ: &str = "http://ns.adobe.com/xmp/Identifier/qual/1.0/";
pub(super) const NS_CHAIRPHOTO: &str = "https://chairphoto.local/ns/1.0/";
pub(super) const NS_EXIF: &str = "http://ns.adobe.com/exif/1.0/";
// Metadata Working Group region schema (mwg-rs) + the shared structure namespaces it uses.
// digiKam, Lightroom and Picasa all read/write faces through these.
pub(super) const NS_MWG_RS: &str = "http://www.metadataworkinggroup.com/schemas/regions/";
pub(super) const NS_STAREA: &str = "http://ns.adobe.com/xmp/sType/Area#";
pub(super) const NS_STDIM: &str = "http://ns.adobe.com/xap/1.0/sType/Dimensions#";
