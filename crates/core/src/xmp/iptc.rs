//! The authored IPTC Core fields: the [`MANAGED`] property table, their writers and the
//! reader of which ones a sidecar already holds.

use std::path::Path;
use xmltree::XMLNode;
use crate::catalog::{IptcFields, IptcMask};
use super::document::SidecarDocument;
use super::dom::{child, element_children, first_text, is_rdf, lang_alt, plain, seq_creator};
use super::ns::{NS_DC, NS_IPTC, NS_PHOTOSHOP, NS_RDF};
use super::parse::ns_attr;
use super::repair::parse_for_read;
use super::sidecar_path;

/// One property [`write_iptc`] manages: its (namespace, local name) and the catalog field
/// that holds its value, side by side so the mapping cannot drift between two lists.
pub(super) struct Managed {
    pub(super) ns: &'static str,
    pub(super) name: &'static str,
    field: IptcMask,
}

/// Properties chairphoto manages via [`write_iptc`]. A write touches only the ones whose
/// catalog value changed (issue #144): it removes every existing instance of a changed
/// property — element or compact form, in every Description — and re-adds it when the new
/// value is non-empty. Every other element in the sidecar, an unchanged managed property
/// included, is preserved.
/// Note: `chairphoto:ImportBatch` is managed separately by [`write_import_batch`](super::write_import_batch)
/// and is intentionally NOT listed here so IPTC writes don't clobber it. Likewise
/// `chairphoto:LastWrite` is not listed: every writer's completion stamp is applied
/// uniformly by [`SidecarDocument::commit`], not per-writer.
pub(super) const MANAGED: [Managed; 11] = [
    Managed { ns: NS_DC, name: "description", field: IptcMask::DESCRIPTION },
    Managed { ns: NS_DC, name: "title", field: IptcMask::TITLE },
    Managed { ns: NS_DC, name: "rights", field: IptcMask::COPYRIGHT },
    Managed { ns: NS_DC, name: "creator", field: IptcMask::CREATOR },
    Managed { ns: NS_PHOTOSHOP, name: "Headline", field: IptcMask::HEADLINE },
    Managed { ns: NS_PHOTOSHOP, name: "Credit", field: IptcMask::CREDIT },
    Managed { ns: NS_PHOTOSHOP, name: "Source", field: IptcMask::SOURCE },
    Managed { ns: NS_PHOTOSHOP, name: "City", field: IptcMask::CITY },
    Managed { ns: NS_PHOTOSHOP, name: "State", field: IptcMask::STATE },
    Managed { ns: NS_PHOTOSHOP, name: "Country", field: IptcMask::COUNTRY },
    Managed { ns: NS_IPTC, name: "CountryCode", field: IptcMask::COUNTRY_CODE },
];

/// The sidecar node for one managed property's non-empty value.
fn managed_node(ns: &str, name: &str, value: &str) -> XMLNode {
    match (ns, name) {
        (NS_DC, "creator") => seq_creator(value),
        (NS_DC, _) => lang_alt(name, value),
        (NS_PHOTOSHOP, _) => plain("photoshop", NS_PHOTOSHOP, name, value),
        _ => plain("Iptc4xmpCore", NS_IPTC, name, value),
    }
}

/// Write a change of the authored IPTC fields into the photo's XMP sidecar, merging with
/// any existing content. `before` is the catalog's value before this change and `after`
/// the value now stored; callers read both in the lock hold that stores `after`, so the
/// diff is the change the catalog actually made.
///
/// Only a field whose value changed is written (issue #144): a new non-empty value replaces
/// every existing instance of the property, and a value the user cleared removes them. A
/// field that did not change — empty before and after included — is left exactly as the
/// sidecar has it, so a creator, rights or caption another tool wrote (ChairPhoto never
/// imports them into the catalog) survive a city-only geocode or a title-only save. With
/// nothing changed the sidecar is not opened at all.
///
/// Creates the sidecar if absent; backs up a pre-existing non-chairphoto sidecar once
/// before the first write.
pub fn write_iptc(photo_path: &Path, before: &IptcFields, after: &IptcFields) -> Result<(), String> {
    write_iptc_fields(photo_path, IptcMask::changed(before, after), after)
}

/// [`write_iptc`] of an explicit set of fields: each field in `fields` is written from
/// `values` — replaced when non-empty, removed when empty — and every other property is
/// left as the sidecar has it. The catalog's owed-IPTC record (#148) names the fields a
/// sidecar has not received yet; a save or a retry writes those together with its own
/// change. With `fields` empty the sidecar is not opened at all.
pub fn write_iptc_fields(photo_path: &Path, fields: IptcMask, values: &IptcFields) -> Result<(), String> {
    let mut owned = Vec::new();
    let mut replacements = Vec::new();
    for m in &MANAGED {
        if !fields.contains(m.field) {
            continue;
        }
        owned.push((m.ns, m.name));
        let new = m.field.value(values);
        if !new.is_empty() {
            replacements.push(managed_node(m.ns, m.name, new));
        }
    }
    if owned.is_empty() {
        return Ok(());
    }

    let mut doc = SidecarDocument::open(photo_path)?;
    doc.replace_owned(&owned, replacements);
    doc.commit()
}

/// The managed IPTC fields the photo's sidecar holds a non-empty value for, in element or
/// compact attribute form, in any `rdf:Description`. No sidecar is [`IptcMask::NONE`]; a
/// sidecar that does not parse is an error (nothing is known about it).
pub fn read_iptc_present(photo_path: &Path) -> Result<IptcMask, String> {
    let path = sidecar_path(photo_path);
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(IptcMask::NONE),
        Err(e) => return Err(e.to_string()),
    };
    let root = parse_for_read(file)?;
    let Some(rdf) = root.get_child(("RDF", NS_RDF)) else {
        return Ok(IptcMask::NONE);
    };
    let mut present = IptcMask::NONE;
    for (_, desc) in element_children(rdf) {
        if !is_rdf(desc, "Description") {
            continue;
        }
        for m in &MANAGED {
            let attr = ns_attr(desc, m.ns, m.name).is_some_and(|v| !v.trim().is_empty());
            let element = child(desc, m.ns, m.name).is_some_and(|e| first_text(e).is_some());
            if attr || element {
                present = present | m.field;
            }
        }
    }
    Ok(present)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmp::test_xml::read;
    use crate::xmp::{sidecar_backup_path, test_fixtures};

    #[test]
    fn creates_sidecar_with_iptc() {
        let dir = crate::test_support::TestTmpDir::new("xmp-create");
        let photo = dir.join("DSC1.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let fields = IptcFields {
            description: "A ferry".into(),
            creator: "Andreas".into(),
            copyright: "(c) 2026".into(),
            city: "Trondheim".into(),
            ..Default::default()
        };
        write_iptc(&photo, &IptcFields::default(), &fields).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("A ferry"));
        assert!(xmp.contains("Andreas"));
        assert!(xmp.contains("Trondheim"));
        assert!(xmp.contains("photoshop:City"));
        assert!(xmp.contains("dc:description"));
    }

    #[test]
    fn merge_preserves_foreign_elements() {
        let dir = crate::test_support::TestTmpDir::new("xmp-merge");
        let photo = dir.join("DSC2.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // Simulate an existing darktable sidecar with its own namespace + element.
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/">
   <darktable:history_end>7</darktable:history_end>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        let fields = IptcFields {
            description: "Edited photo".into(),
            ..Default::default()
        };
        write_iptc(&photo, &IptcFields::default(), &fields).unwrap();

        let xmp = read(&sidecar_path(&photo));
        // Our field is present AND darktable's element survived.
        assert!(xmp.contains("Edited photo"), "IPTC not written");
        assert!(xmp.contains("history_end"), "darktable data clobbered!");
        assert!(xmp.contains("darktable"), "darktable namespace lost!");
        // A backup of the original was made on first write.
        assert!(sidecar_backup_path(&sidecar_path(&photo)).exists());
    }

    #[test]
    fn rewrites_not_duplicates_on_second_write() {
        let dir = crate::test_support::TestTmpDir::new("xmp-rewrite");
        let photo = dir.join("DSC3.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let first = IptcFields { headline: "First".into(), ..Default::default() };
        write_iptc(&photo, &IptcFields::default(), &first).unwrap();
        write_iptc(&photo, &first, &IptcFields { headline: "Second".into(), ..Default::default() }).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("Second"));
        assert!(!xmp.contains("First"), "old value should be replaced, not duplicated");
        assert_eq!(xmp.matches("photoshop:Headline").count(), 2); // open + close tag, once
    }

    /// Issue #144: the geocoder fills an empty city, and writes nothing else. The creator,
    /// rights, caption, title, headline and country code another tool wrote — values the
    /// catalog never imported, so they are empty there before and after — must survive, in
    /// a Lightroom single-Description sidecar and in exiftool's one-Description-per-namespace
    /// layout alike. The city itself changed, so the foreign one is replaced.
    #[test]
    fn a_city_only_write_keeps_every_other_foreign_iptc_field() {
        use test_fixtures::{assert_non_iptc_intact, foreign_iptc, iptc, with, FOREIGN};
        for (layout, sidecar) in FOREIGN {
            let dir = crate::test_support::TestTmpDir::new("xmp-144-city-only");
            let photo = dir.join("DSC144.ARW");
            std::fs::write(&photo, b"raw").unwrap();
            std::fs::write(sidecar_path(&photo), sidecar).unwrap();

            let filled = IptcFields { city: "Trondheim".into(), ..Default::default() };
            write_iptc(&photo, &IptcFields::default(), &filled).unwrap();

            let xml = read(&sidecar_path(&photo));
            assert_eq!(iptc(&xml), with(foreign_iptc(), "photoshop:City", &["Trondheim"]),
                "{layout}:\n{xml}");
            assert_non_iptc_intact(&xml, layout);
        }
    }

    /// A write that changes no field does not open the sidecar: no stamp, no backup, the
    /// file byte-identical.
    #[test]
    fn an_unchanged_write_leaves_the_sidecar_alone() {
        let dir = crate::test_support::TestTmpDir::new("xmp-144-unchanged");
        let photo = dir.join("DSC144.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        std::fs::write(sidecar_path(&photo), test_fixtures::LIGHTROOM).unwrap();

        let same = IptcFields { title: "Mine".into(), ..Default::default() };
        write_iptc(&photo, &same, &same).unwrap();

        assert_eq!(read(&sidecar_path(&photo)), test_fixtures::LIGHTROOM);
        assert!(!sidecar_backup_path(&sidecar_path(&photo)).exists());
    }

    /// Every catalog field lands in its own IPTC property — checked against a table written
    /// out here, not derived from `MANAGED`, so a swapped mapping (Credit written as Source,
    /// say) fails. Each field carries a distinct value.
    #[test]
    fn every_iptc_field_maps_to_its_own_property() {
        let dir = crate::test_support::TestTmpDir::new("xmp-144-mapping");
        let photo = dir.join("DSC144.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        let fields = IptcFields {
            description: "v-description".into(),
            headline: "v-headline".into(),
            title: "v-title".into(),
            creator: "v-creator".into(),
            copyright: "v-copyright".into(),
            credit: "v-credit".into(),
            source: "v-source".into(),
            city: "v-city".into(),
            state: "v-state".into(),
            country: "v-country".into(),
            country_code: "v-country_code".into(),
        };
        write_iptc(&photo, &IptcFields::default(), &fields).unwrap();

        let xml = read(&sidecar_path(&photo));
        let expected = [
            (NS_DC, "description", "v-description"),
            (NS_DC, "title", "v-title"),
            (NS_DC, "rights", "v-copyright"),
            (NS_DC, "creator", "v-creator"),
            (NS_PHOTOSHOP, "Headline", "v-headline"),
            (NS_PHOTOSHOP, "Credit", "v-credit"),
            (NS_PHOTOSHOP, "Source", "v-source"),
            (NS_PHOTOSHOP, "City", "v-city"),
            (NS_PHOTOSHOP, "State", "v-state"),
            (NS_PHOTOSHOP, "Country", "v-country"),
            (NS_IPTC, "CountryCode", "v-country_code"),
        ];
        for (ns, name, value) in expected {
            assert_eq!(test_fixtures::property_values(&xml, ns, name), [value], "{name}:\n{xml}");
        }
    }
}
