//! Privacy invariants for padlocked (private) tags: the padlock inherits at creation,
//! and cloud prompts withhold whole private subtrees — including the rejected-tags
//! clause. Regression tests for the leak where person tags created after a recursive
//! padlock sweep defaulted to cloud-visible.

use chairphoto_lib::catalog::Catalog;

mod common;

fn open_catalog(tag: &str) -> (Catalog, common::TestTmpDir) {
    let dir = common::TestTmpDir::new(tag);
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
    (catalog, dir)
}

#[test]
fn tag_created_under_padlocked_parent_is_private_from_birth() {
    let (c, _dir) = open_catalog("tag-privacy-inherit");
    let people = c.create_tag("People").unwrap();
    c.set_tag_private(people, true, true).unwrap();

    // The padlock sweep ran BEFORE this person existed — the historical leak.
    let late = c.create_tag("People/Added Later").unwrap();
    assert!(c.tag_private(late).unwrap());

    // Deep creation: every new intermediate inherits too.
    let deep = c.create_tag("People/Group/Someone").unwrap();
    assert!(c.tag_private(deep).unwrap());
    let group = c.find_tag_id_by_path("People/Group").unwrap().unwrap();
    assert!(c.tag_private(group).unwrap());

    // A tag under a non-padlocked parent stays public.
    let plain = c.create_tag("Animals/Birds").unwrap();
    assert!(!c.tag_private(plain).unwrap());
}

#[cfg(feature = "ai")]
#[test]
fn cloud_taxonomy_withholds_the_whole_private_subtree() {
    use chairphoto_lib::plugins::ai;
    let (c, _dir) = open_catalog("tag-privacy-taxonomy");
    let name = c.create_tag("People/Nina Example").unwrap();
    c.create_tag("Animals/Birds").unwrap();
    // Padlock only the PARENT, non-recursively: the child's own flag stays clear —
    // exactly the drifted state found in real catalogs.
    let people = c.find_tag_id_by_path("People").unwrap().unwrap();
    c.set_tag_private(people, true, false).unwrap();
    assert!(!c.tag_private(name).unwrap());

    let cloud = ai::taxonomy_text(&c, false).unwrap();
    assert!(
        !cloud.contains("Nina Example"),
        "cloud prompt leaked a name:\n{cloud}"
    );
    assert!(!cloud.contains("People"), "{cloud}");
    assert!(cloud.contains("Animals/Birds"), "{cloud}");

    // The local model still sees everything.
    let local = ai::taxonomy_text(&c, true).unwrap();
    assert!(local.contains("People/Nina Example"), "{local}");
}

#[cfg(feature = "ai")]
#[test]
fn rejected_clause_paths_get_the_same_privacy_filter() {
    use chairphoto_lib::plugins::ai;
    let (c, _dir) = open_catalog("tag-privacy-rejected");
    c.create_tag("People/Nina Example").unwrap();
    c.create_tag("Animals/Birds").unwrap();
    let people = c.find_tag_id_by_path("People").unwrap().unwrap();
    c.set_tag_private(people, true, false).unwrap();

    let rejected = vec![
        "Animals/Birds".to_string(),         // public and existing: kept
        "People/Nina Example".to_string(),   // inside a private subtree: withheld
        "People/Deleted Person".to_string(), // no longer resolves: withheld — still a name
    ];
    let safe = ai::cloud_safe_paths(&c, &rejected).unwrap();
    assert_eq!(safe, vec!["Animals/Birds".to_string()]);
}
