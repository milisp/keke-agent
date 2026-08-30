#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use keke_paths::AbsPath;
use keke_plugin::EntrySource;
use keke_plugin::GitRef;
use keke_plugin::Marketplace;
use keke_plugin::PluginError;
use keke_plugin::foreign_installs;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn abs(path: impl AsRef<Path>) -> AbsPath {
    AbsPath::new(path.as_ref()).expect("absolute")
}

/// A catalog whose single entry carries `source` verbatim.
fn catalog_with_source(root: &Path, source: &str) -> Marketplace {
    write(
        &root.join(".claude-plugin/marketplace.json"),
        &format!(
            r#"{{"name": "acme-tools", "owner": {{"name": "Acme", "email": "a@example"}},
                 "plugins": [{{"name": "shipper", "version": "1.0.0", "source": {source}}}]}}"#
        ),
    );
    Marketplace::load(&abs(root))
        .expect("parses")
        .expect("is a marketplace")
}

fn source_of(root: &Path, source: &str) -> EntrySource {
    catalog_with_source(root, source)
        .get("shipper")
        .expect("listed")
        .source
        .clone()
}

#[test]
fn a_bare_string_source_names_a_directory_in_the_marketplace() {
    let tmp = tempfile::tempdir().expect("tempdir");

    assert_eq!(
        source_of(tmp.path(), r#""./plugins/shipper""#),
        EntrySource::Local {
            path: "./plugins/shipper".to_string()
        }
    );
}

#[test]
fn a_typed_local_source_names_a_directory_in_the_marketplace() {
    let tmp = tempfile::tempdir().expect("tempdir");

    assert_eq!(
        source_of(
            tmp.path(),
            r#"{"type": "local", "path": "./plugins/shipper"}"#
        ),
        EntrySource::Local {
            path: "./plugins/shipper".to_string()
        }
    );
}

#[test]
fn a_url_source_with_a_ref_can_move_under_the_person() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let source = source_of(
        tmp.path(),
        r#"{"source": "url", "url": "https://github.com/x/y", "ref": "main"}"#,
    );

    assert_eq!(
        source,
        EntrySource::Git {
            url: "https://github.com/x/y".to_string(),
            reference: GitRef::Moving("main".to_string()),
        }
    );
    let EntrySource::Git { reference, .. } = source else {
        unreachable!()
    };
    assert!(reference.can_move());
}

#[test]
fn a_url_source_with_a_sha_cannot_move_under_the_person() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let source = source_of(
        tmp.path(),
        r#"{"source": "url", "url": "https://github.com/x/y", "sha": "61f1903b"}"#,
    );

    assert_eq!(
        source,
        EntrySource::Git {
            url: "https://github.com/x/y".to_string(),
            reference: GitRef::Pinned("61f1903b".to_string()),
        }
    );
    let EntrySource::Git { reference, .. } = source else {
        unreachable!()
    };
    assert!(!reference.can_move());
}

#[test]
fn a_url_source_that_states_no_revision_is_a_moving_default() {
    let tmp = tempfile::tempdir().expect("tempdir");

    assert_eq!(
        source_of(
            tmp.path(),
            r#"{"source": "url", "url": "https://github.com/x/y"}"#
        ),
        EntrySource::Git {
            url: "https://github.com/x/y".to_string(),
            reference: GitRef::Default,
        }
    );
}

#[test]
fn a_pin_outranks_a_branch_when_an_entry_states_both() {
    let tmp = tempfile::tempdir().expect("tempdir");

    assert_eq!(
        source_of(
            tmp.path(),
            r#"{"source": "url", "url": "https://github.com/x/y", "ref": "main", "sha": "61f1903b"}"#
        ),
        EntrySource::Git {
            url: "https://github.com/x/y".to_string(),
            reference: GitRef::Pinned("61f1903b".to_string()),
        }
    );
}

#[test]
fn the_owner_of_a_catalog_is_the_name_they_gave() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let catalog = catalog_with_source(tmp.path(), r#""./plugins/shipper""#);

    assert_eq!(catalog.name, "acme-tools");
    assert_eq!(catalog.owner.as_deref(), Some("Acme"));
}

#[test]
fn a_catalog_written_for_a_newer_host_still_loads() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("marketplace.json"),
        r#"{"name": "acme-tools", "registryVersion": 9,
            "plugins": [{"name": "shipper", "source": "./s", "category": "development",
                         "author": {"name": "Acme"}, "tags": [], "keywords": [], "domains": [],
                         "sandboxProfile": "strict"}]}"#,
    );

    let catalog = Marketplace::load(&abs(tmp.path()))
        .expect("parses")
        .expect("is a marketplace");

    assert_eq!(catalog.entries.len(), 1);
}

#[test]
fn an_entry_with_no_usable_source_is_dropped_but_still_reported() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("marketplace.json"),
        r#"{"name": "acme-tools", "plugins": [
             {"name": "shipper", "source": "./s"},
             {"name": "mystery", "source": {"type": "carrier-pigeon"}},
             {"name": "sourceless"}]}"#,
    );

    let catalog = Marketplace::load(&abs(tmp.path()))
        .expect("parses")
        .expect("is a marketplace");

    assert_eq!(catalog.entries.len(), 1);
    assert!(catalog.get("mystery").is_none());
    assert_eq!(catalog.skipped, ["mystery", "sourceless"]);
}

#[test]
fn a_directory_that_is_not_a_marketplace_offers_nothing_rather_than_failing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(&tmp.path().join("plugin.json"), r#"{"name": "solo"}"#);

    assert!(
        Marketplace::load(&abs(tmp.path()))
            .expect("not an error")
            .is_none()
    );
}

#[test]
fn a_catalog_that_exists_but_does_not_parse_is_an_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(&tmp.path().join("marketplace.json"), "{ not json");

    assert!(matches!(
        Marketplace::load(&abs(tmp.path())),
        Err(PluginError::Json { .. })
    ));
}

/// Write an `installed_plugins.json` holding one record under `key`.
fn installed(root: &Path, key: &str, entry: &str) -> std::path::PathBuf {
    let path = root.join("installed_plugins.json");
    write(&path, &format!(r#"{{"plugins": {{"{key}": [{entry}]}}}}"#));
    path
}

#[test]
fn an_install_the_person_made_for_themselves_is_visible_anywhere() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let install = tmp.path().join("plugins/shipper");
    std::fs::create_dir_all(&install).expect("mkdir");
    let record = installed(
        tmp.path(),
        "shipper@acme-tools",
        &format!(
            r#"{{"installPath": "{}", "scope": "user", "projectPath": null}}"#,
            install.display()
        ),
    );

    let installs = foreign_installs(&record, &abs(tmp.path()));

    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0].name, "shipper");
    assert_eq!(installs[0].marketplace.as_deref(), Some("acme-tools"));
}

#[test]
fn an_install_recorded_without_a_marketplace_still_names_its_plugin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let install = tmp.path().join("plugins/shipper");
    std::fs::create_dir_all(&install).expect("mkdir");
    let record = installed(
        tmp.path(),
        "shipper",
        &format!(r#"{{"installPath": "{}"}}"#, install.display()),
    );

    let installs = foreign_installs(&record, &abs(tmp.path()));

    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0].name, "shipper");
    assert_eq!(installs[0].marketplace, None);
}

#[test]
fn one_plugin_installed_twice_is_reported_twice() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let first = tmp.path().join("a/shipper");
    let second = tmp.path().join("b/shipper");
    std::fs::create_dir_all(&first).expect("mkdir");
    std::fs::create_dir_all(&second).expect("mkdir");
    let record = installed(
        tmp.path(),
        "shipper@acme-tools",
        &format!(
            r#"{{"installPath": "{}"}}, {{"installPath": "{}"}}"#,
            first.display(),
            second.display()
        ),
    );

    assert_eq!(foreign_installs(&record, &abs(tmp.path())).len(), 2);
}

/// A record tied to `project`, installed somewhere that exists.
fn project_scoped(root: &Path, project: &Path) -> std::path::PathBuf {
    let install = root.join("plugins/shipper");
    std::fs::create_dir_all(&install).expect("mkdir");
    installed(
        root,
        "shipper@acme-tools",
        &format!(
            r#"{{"installPath": "{}", "scope": "project", "projectPath": "{}"}}"#,
            install.display(),
            project.display()
        ),
    )
}

#[test]
fn a_plugin_another_project_installed_does_not_appear_in_this_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let theirs = tmp.path().join("theirs");
    let mine = tmp.path().join("mine");
    std::fs::create_dir_all(&theirs).expect("mkdir");
    std::fs::create_dir_all(&mine).expect("mkdir");
    let record = project_scoped(tmp.path(), &theirs);

    assert!(foreign_installs(&record, &abs(&mine)).is_empty());
}

#[test]
fn a_project_scoped_plugin_appears_inside_its_own_project() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("theirs");
    let inner = project.join("crates/deep");
    std::fs::create_dir_all(&inner).expect("mkdir");
    let record = project_scoped(tmp.path(), &project);

    assert_eq!(foreign_installs(&record, &abs(&project)).len(), 1);
    assert_eq!(foreign_installs(&record, &abs(&inner)).len(), 1);
}

#[test]
fn a_sibling_whose_name_starts_with_the_project_name_is_not_inside_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("b");
    let sibling = tmp.path().join("bc");
    std::fs::create_dir_all(&project).expect("mkdir");
    std::fs::create_dir_all(&sibling).expect("mkdir");
    let record = project_scoped(tmp.path(), &project);

    assert!(foreign_installs(&record, &abs(&sibling)).is_empty());
}

#[test]
fn an_entry_that_claims_a_project_but_names_none_stays_hidden() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let install = tmp.path().join("plugins/shipper");
    std::fs::create_dir_all(&install).expect("mkdir");
    let record = installed(
        tmp.path(),
        "shipper@acme-tools",
        &format!(
            r#"{{"installPath": "{}", "scope": "local", "projectPath": ""}}"#,
            install.display()
        ),
    );

    assert!(foreign_installs(&record, &abs(tmp.path())).is_empty());
}

#[test]
fn a_project_path_ties_an_entry_down_whatever_its_scope_claims() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let theirs = tmp.path().join("theirs");
    let mine = tmp.path().join("mine");
    std::fs::create_dir_all(&theirs).expect("mkdir");
    std::fs::create_dir_all(&mine).expect("mkdir");
    let install = tmp.path().join("plugins/shipper");
    std::fs::create_dir_all(&install).expect("mkdir");
    let record = installed(
        tmp.path(),
        "shipper@acme-tools",
        &format!(
            r#"{{"installPath": "{}", "scope": "user", "projectPath": "{}"}}"#,
            install.display(),
            theirs.display()
        ),
    );

    assert!(foreign_installs(&record, &abs(&mine)).is_empty());
    assert_eq!(foreign_installs(&record, &abs(&theirs)).len(), 1);
}

#[test]
fn an_install_whose_files_are_gone_is_not_offered() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let record = installed(
        tmp.path(),
        "shipper@acme-tools",
        &format!(
            r#"{{"installPath": "{}"}}, {{"installPath": "plugins/relative"}}"#,
            tmp.path().join("plugins/uninstalled").display()
        ),
    );

    assert!(foreign_installs(&record, &abs(tmp.path())).is_empty());
}

#[test]
fn another_programs_file_being_corrupt_does_not_fail_the_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let record = tmp.path().join("installed_plugins.json");
    write(&record, "{\"plugins\": [oh dear");

    assert!(foreign_installs(&record, &abs(tmp.path())).is_empty());
}

#[test]
fn never_having_used_the_other_harness_is_not_a_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");

    assert!(foreign_installs(&tmp.path().join("nothing.json"), &abs(tmp.path())).is_empty());
}
