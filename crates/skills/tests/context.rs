//! Each test names a rule from the crate's job description and would fail if
//! the rule broke, however the code was rearranged.
//!
//! An integration test is not `#[cfg(test)]`, so the clippy allowance for
//! panicking in tests does not reach it. Same waiver the other suites take.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use keke_plugin::PluginScope;
use keke_plugin::PluginSet;
use keke_plugin::load;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_protocol::SessionId;
use keke_protocol::ThreadId;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn plugin_with_skill(root: &Path, plugin: &str, skill: &str, description: &str, body: &str) {
    let dir = root.join(plugin);
    write(
        &dir.join("plugin.json"),
        &format!(r#"{{"name": "{plugin}"}}"#),
    );
    write(
        &dir.join(format!("skills/{skill}/SKILL.md")),
        &format!("---\nname: {skill}\ndescription: {description}\n---\n\n{body}\n"),
    );
}

async fn contributed_fragments(set: &PluginSet) -> Vec<keke_plugin_api::ContextFragment> {
    let mut builder = ExtensionRegistryBuilder::new();
    keke_skills::install(&mut builder, set);
    let registry = builder.build();
    let ctx = ExtensionContext::new(SessionId::new(), ThreadId::new());

    let mut fragments = Vec::new();
    for contributor in registry.context_contributors() {
        fragments.extend(contributor.contribute_turn_context(&ctx).await);
    }
    fragments
}

#[tokio::test]
async fn only_descriptions_reach_the_context_window_not_bodies() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plugin_with_skill(
        tmp.path(),
        "acme",
        "review",
        "how this team reviews",
        "SECRET-BODY-CONTENT should not be injected up front",
    );
    let plugin = load(&tmp.path().join("acme"), PluginScope::User).expect("resolves");
    let set = PluginSet::compose(vec![plugin]).expect("composes");

    let fragments = contributed_fragments(&set).await;

    assert_eq!(fragments.len(), 1);
    assert!(fragments[0].text.contains("how this team reviews"));
    assert!(!fragments[0].text.contains("SECRET-BODY-CONTENT"));
}

#[tokio::test]
async fn no_skills_means_no_fragment_at_all() {
    let set = PluginSet::default();

    let fragments = contributed_fragments(&set).await;

    assert!(fragments.is_empty(), "empty section is wasted context");
}

#[tokio::test]
async fn the_skills_fragment_is_ordered_as_tool_guidance() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plugin_with_skill(tmp.path(), "acme", "review", "how we review", "body");
    let plugin = load(&tmp.path().join("acme"), PluginScope::User).expect("resolves");
    let set = PluginSet::compose(vec![plugin]).expect("composes");

    let fragments = contributed_fragments(&set).await;

    // Convention documented on ContextFragment::order: 100+ is tool guidance,
    // not harness identity (negative) or deployment persona (0).
    assert!(fragments[0].order >= 100);
}

#[tokio::test]
async fn two_plugins_contributing_the_same_skill_name_are_both_listed_namespaced() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plugin_with_skill(tmp.path(), "alpha", "review", "alpha's review skill", "a");
    plugin_with_skill(tmp.path(), "beta", "review", "beta's review skill", "b");
    let plugins = vec![
        load(&tmp.path().join("alpha"), PluginScope::User).expect("resolves"),
        load(&tmp.path().join("beta"), PluginScope::User).expect("resolves"),
    ];
    let set = PluginSet::compose(plugins).expect("composes");

    let fragments = contributed_fragments(&set).await;

    let text = &fragments[0].text;
    assert!(text.contains("alpha:review"));
    assert!(text.contains("beta:review"));
}

#[tokio::test]
async fn reading_a_skill_body_strips_the_frontmatter() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plugin_with_skill(
        tmp.path(),
        "acme",
        "review",
        "how we review",
        "Only the body should come back.",
    );
    let plugin = load(&tmp.path().join("acme"), PluginScope::User).expect("resolves");
    let set = PluginSet::compose(vec![plugin]).expect("composes");

    let body = keke_skills::read_skill_body(&set, "acme:review")
        .await
        .expect("known skill");

    assert!(body.contains("Only the body should come back."));
    assert!(!body.contains("description:"));
}

#[tokio::test]
async fn reading_an_unqualified_or_unknown_name_is_refused_without_touching_disk() {
    let tmp = tempfile::tempdir().expect("tempdir");
    plugin_with_skill(tmp.path(), "acme", "review", "how we review", "body");
    let plugin = load(&tmp.path().join("acme"), PluginScope::User).expect("resolves");
    let set = PluginSet::compose(vec![plugin]).expect("composes");

    let error = keke_skills::read_skill_body(&set, "../../etc/passwd")
        .await
        .expect_err("not a known skill");

    assert!(matches!(error, keke_skills::SkillError::Unknown { .. }));
}
