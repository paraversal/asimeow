use anyhow::Result;
use asimeow::catalog::{catalog, Confidence};
use asimeow::config::{default_rules, Config, Root, Rule};
use asimeow::recommend::{
    find_candidates, git_status, recommend, Applied, Candidate, Findings, Git, Report, Suggestion,
    ToolCache,
};
use asimeow::report::{applied_text, recommend_snippet, recommend_text, Paint};
use asimeow::tm;
use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
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
    find_candidates(config, &catalog(), 4, None)
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

#[test]
fn test_tool_caches_are_listed_not_searched() -> Result<()> {
    let home = tempdir()?;
    let cached = home.path().join(".npm").join("some-package");
    fs::create_dir_all(cached.join("build"))?;
    File::create(cached.join("package.json"))?;

    let cfg = config(home.path(), vec![node_rule()], &[]);
    let findings = find_candidates(&cfg, &catalog(), 4, Some(home.path()))?;

    assert!(
        findings.candidates.is_empty(),
        "nothing suggested inside a cache"
    );
    assert_eq!(findings.caches, vec![home.path().join(".npm")]);
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A git repository with an ignored `.next` (1000 bytes) and a committed `dist` (500
/// bytes), or None if git isn't available.
fn repo_with_build_folders() -> Result<Option<tempfile::TempDir>> {
    let temp_dir = tempdir()?;
    let app = temp_dir.path().join("app");
    fs::create_dir_all(app.join(".next"))?;
    fs::create_dir_all(app.join("dist"))?;
    File::create(app.join("package.json"))?;
    fs::write(app.join(".next").join("page.js"), vec![0u8; 1000])?;
    fs::write(app.join("dist").join("lib.js"), vec![0u8; 500])?;
    fs::write(app.join(".gitignore"), ".next/\n")?;
    if !git(&app, &["init", "-q"]) {
        return Ok(None);
    }
    assert!(git(&app, &["add", "."]));
    assert!(git(&app, &["commit", "-q", "-m", "init"]));
    Ok(Some(temp_dir))
}

#[test]
fn test_git_status() -> Result<()> {
    let Some(temp_dir) = repo_with_build_folders()? else {
        return Ok(()); // no git here
    };
    let app = temp_dir.path().join("app");
    assert_eq!(git_status(&app.join(".next")), Git::Ignored);
    assert_eq!(git_status(&app.join("dist")), Git::Tracked);
    let outside = tempdir()?;
    fs::create_dir(outside.path().join("build"))?;
    assert_eq!(git_status(&outside.path().join("build")), Git::NoRepo);
    Ok(())
}

#[test]
fn test_recommend_sizes_groups_and_flags_tracked_folders() -> Result<()> {
    let Some(temp_dir) = repo_with_build_folders()? else {
        return Ok(());
    };
    // A second project outside the repo with a bigger .next
    let other = temp_dir.path().join("other");
    fs::create_dir_all(other.join(".next"))?;
    File::create(other.join("package.json"))?;
    fs::write(other.join(".next").join("big.js"), vec![0u8; 3000])?;

    let report = recommend(
        &config(temp_dir.path(), vec![node_rule()], &[]),
        &catalog(),
        4,
        None,
    )?;

    let next = &report.suggestions[0];
    assert_eq!(next.folder, ".next");
    assert_eq!(next.bytes, 4000);
    assert_eq!(next.confidence, Confidence::High);
    assert_eq!(
        next.paths,
        vec![
            (other.join(".next"), 3000),
            (temp_dir.path().join("app").join(".next"), 1000)
        ]
    );
    let dist = &report.suggestions[1];
    assert_eq!((dist.folder.as_str(), dist.bytes), ("dist", 500));
    assert_eq!(dist.confidence, Confidence::Check);
    assert_eq!(dist.tracked, vec![temp_dir.path().join("app").join("dist")]);
    Ok(())
}

fn suggestion(
    folder: &str,
    rule: &str,
    marker: &str,
    config_rule: Option<&str>,
    confidence: Confidence,
    bytes: u64,
    path: &str,
) -> Suggestion {
    Suggestion {
        folder: folder.to_string(),
        catalog_rule: rule.to_string(),
        file_match: marker.to_string(),
        config_rule: config_rule.map(str::to_string),
        confidence,
        bytes,
        paths: vec![(PathBuf::from(path), bytes)],
        tracked: vec![],
    }
}

fn sample_report() -> Report {
    Report {
        suggestions: vec![
            suggestion(
                ".next",
                "node",
                "package.json",
                Some("node"),
                Confidence::High,
                2_900_000_000,
                "/h/web/.next",
            ),
            suggestion(
                ".Rproj.user",
                "r",
                "*.Rproj",
                None,
                Confidence::High,
                14_900_000,
                "/h/stats/.Rproj.user",
            ),
            suggestion(
                "out",
                "node",
                "package.json",
                Some("node"),
                Confidence::Check,
                175_000_000,
                "/h/ext/out",
            ),
        ],
        caches: vec![ToolCache {
            path: PathBuf::from("/h/.npm"),
            bytes: 6_000_000,
            excluded: false,
        }],
        already_excluded: 2,
        dirs: 17_158,
        elapsed: Duration::from_millis(1500),
    }
}

#[test]
fn test_recommend_text() {
    let text = recommend_text(
        &sample_report(),
        Some(Path::new("/h")),
        "~/.config/asimeow/config.yaml",
        false,
        false,
        Paint(false),
    );
    assert_eq!(
        text,
        "Scanned 17,158 folders in 1.5s.\n\
         \n\
         Could save 3.1 GB more in 3 folders:\n\
         \n  Likely safe\n\
         \x20    2.9 GB  .next                   1 folder  add to rule \"node\"\n\
         \x20            e.g. ~/web/.next\n\
         \x20   14.9 MB  .Rproj.user             1 folder  new rule \"r\"\n\
         \x20            e.g. ~/stats/.Rproj.user\n\
         \n  Check before adding (generic names, or tracked by git)\n\
         \x20    175 MB  out                     1 folder  add to rule \"node\"\n\
         \x20            e.g. ~/ext/out\n\
         \n\
         Tool caches (downloadable again; exclude one with `asimeow exclude <path>`):\n\
         \x20    6.0 MB  ~/.npm\n\
         \n\
         2 more folders that the catalog knows are already excluded by other means (like cargo marking target).\n\
         \n\
         → Apply the likely-safe ones with asimeow recommend --apply (2 folders, 2.9 GB).\n\
         \x20 They're added to your config and excluded right away. Preview with --dry-run; -v prints the config snippet instead.\n"
    );
}

#[test]
fn test_recommend_text_verbose_adds_the_snippet_and_applying_drops_the_hint() {
    let verbose = recommend_text(
        &sample_report(),
        None,
        "config.yaml",
        true,
        false,
        Paint(false),
    );
    assert!(verbose.contains(
        "\nAdd to config.yaml:\n\n# in rule \"node\", add under exclusions:\n  - .next\n"
    ));
    let applying = recommend_text(
        &sample_report(),
        None,
        "config.yaml",
        false,
        true,
        Paint(false),
    );
    assert!(!applying.contains("--apply"));
    assert!(!applying.contains("Add to"));
}

#[test]
fn test_color_only_when_asked() {
    let plain = recommend_text(&sample_report(), None, "c", false, false, Paint(false));
    let colored = recommend_text(&sample_report(), None, "c", false, false, Paint(true));
    assert!(!plain.contains('\x1b'));
    assert!(colored.contains("\x1b[1;32mLikely safe\x1b[0m"));
    // Without the escape codes, the text is the same
    let stripped = regex_free_strip(&colored);
    assert_eq!(stripped, plain);
}

/// Removes ANSI color codes (`ESC [ … m`).
fn regex_free_strip(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn test_applied_text() {
    let applied = Applied {
        additions: vec![
            asimeow::config_edit::Addition {
                rule: "python".into(),
                file_match: "requirements.txt".into(),
                folders: vec![".pytest_cache".into()],
                existing: true,
            },
            asimeow::config_edit::Addition {
                rule: "r".into(),
                file_match: "*.Rproj".into(),
                folders: vec![".Rproj.user".into()],
                existing: false,
            },
        ],
        marked: 13,
        marked_bytes: 15_000_000,
        ..Applied::default()
    };
    assert_eq!(
        applied_text(&applied, None, "config.yaml", true, Paint(false)),
        "\nDry run: would add to config.yaml:\n\
         \x20 rule \"python\": .pytest_cache\n\
         \x20 new rule \"r\" (*.Rproj): .Rproj.user\n\
         Would exclude 13 folders (15.0 MB). Nothing was written.\n"
    );
}

#[test]
fn test_new_rules_in_the_snippet_are_valid_config() {
    let report = Report {
        suggestions: sample_report()
            .suggestions
            .into_iter()
            .filter(|s| s.config_rule.is_none())
            .collect(),
        ..Report::default()
    };
    let rules: Vec<Rule> = serde_yaml::from_str(&recommend_snippet(&report)).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].file_match, "*.Rproj");
    assert_eq!(rules[0].exclusions, vec![".Rproj.user"]);
}
