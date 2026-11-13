use anyhow::Result;
use asimeow::catalog::{catalog, Confidence};
use asimeow::config::{default_rules, Config, Root, Rule};
use asimeow::recommend::{find_candidates, Candidate, Findings};
use asimeow::tm;
use std::collections::HashSet;
use std::fs::{self, File};
use std::path::Path;
use tempfile::tempdir;

#[test]
fn test_catalog_parses_with_unique_names() {
    let rules = catalog();
    assert!(rules.len() > 20);
    let names: HashSet<&str> = rules.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names.len(), rules.len(), "rule names are unique");
    // Shared folder lists (YAML anchors) resolve
    let pyproject = rules.iter().find(|r| r.name == "python-pyproject").unwrap();
    assert_eq!(pyproject.confidence_of(".venv"), Some(Confidence::High));
    assert_eq!(pyproject.confidence_of("build"), Some(Confidence::Check));
}

#[test]
fn test_catalog_covers_every_init_default() {
    let rules = catalog();
    for default in default_rules() {
        for folder in &default.exclusions {
            let found = rules.iter().any(|r| {
                r.file_match.to_lowercase() == default.file_match.to_lowercase()
                    && r.confidence_of(folder).is_some()
            });
            assert!(
                found,
                "{} / {folder} missing from the catalog",
                default.file_match
            );
        }
    }
}

fn config(root: &Path, rules: Vec<Rule>, ignore: &[&str]) -> Config {
    Config {
        roots: vec![Root {
            path: root.to_str().unwrap().to_string(),
        }],
        ignore: ignore.iter().map(|s| s.to_string()).collect(),
        rules,
    }
}

fn node_rule() -> Rule {
    Rule {
        name: "node".to_string(),
        file_match: "package.json".to_string(),
        exclusions: vec!["node_modules".to_string()],
    }
}

fn find(config: &Config) -> Result<Findings> {
    find_candidates(config, &catalog(), 4)
}

fn folders(candidates: &[Candidate]) -> Vec<&str> {
    candidates.iter().map(|c| c.folder.as_str()).collect()
}

#[test]
fn test_finds_what_the_config_misses_and_does_not_walk_into_it() -> Result<()> {
    let temp_dir = tempdir()?;
    let app = temp_dir.path().join("app");
    fs::create_dir_all(app.join("node_modules").join("dep"))?;
    File::create(app.join("node_modules").join("dep").join("package.json"))?;
    fs::create_dir_all(app.join(".next").join("server"))?;
    File::create(app.join(".next").join("server").join("package.json"))?;
    File::create(app.join("package.json"))?;
    fs::create_dir_all(app.join(".turbo"))?;
    File::create(app.join("build"))?; // a file, not a folder: not a candidate

    let findings = find(&config(temp_dir.path(), vec![node_rule()], &[]))?;

    assert_eq!(folders(&findings.candidates), vec![".next", ".turbo"]);
    let next = &findings.candidates[0];
    assert_eq!(next.path, app.join(".next"));
    assert_eq!(next.catalog_rule, "node");
    assert_eq!(next.confidence, Confidence::High);
    assert_eq!(
        next.config_rule.as_deref(),
        Some("node"),
        "can be added to the node rule"
    );
    assert_eq!(
        findings.dirs, 2,
        "root and app; generated folders are not walked"
    );
    Ok(())
}

#[test]
fn test_new_rule_suggested_when_config_has_no_matching_rule() -> Result<()> {
    let temp_dir = tempdir()?;
    let app = temp_dir.path().join("android");
    fs::create_dir_all(app.join(".gradle"))?;
    fs::create_dir_all(app.join("build"))?;
    File::create(app.join("build.gradle.kts"))?;

    let findings = find(&config(temp_dir.path(), vec![node_rule()], &[]))?;

    let gradle: Vec<(&str, Confidence, Option<&str>)> = findings
        .candidates
        .iter()
        .map(|c| (c.folder.as_str(), c.confidence, c.config_rule.as_deref()))
        .collect();
    assert_eq!(
        gradle,
        vec![
            (".gradle", Confidence::High, None),
            ("build", Confidence::Check, None)
        ]
    );
    Ok(())
}

#[test]
fn test_already_marked_folders_are_counted_not_suggested() -> Result<()> {
    let temp_dir = tempdir()?;
    let crate_dir = temp_dir.path().join("tool");
    fs::create_dir_all(crate_dir.join("target"))?;
    File::create(crate_dir.join("Cargo.toml"))?;
    assert!(tm::set_exclusion_mark(&crate_dir.join("target"), true));

    let findings = find(&config(temp_dir.path(), vec![], &[]))?;

    assert!(findings.candidates.is_empty());
    assert_eq!(findings.already_excluded, 1);
    Ok(())
}

#[test]
fn test_ignored_folders_are_skipped() -> Result<()> {
    let temp_dir = tempdir()?;
    let hidden = temp_dir.path().join("archive").join("app");
    fs::create_dir_all(hidden.join(".next"))?;
    File::create(hidden.join("package.json"))?;

    let findings = find(&config(temp_dir.path(), vec![], &["archive"]))?;

    assert!(findings.candidates.is_empty());
    Ok(())
}
