use anyhow::Result;
use asimeow::catalog::catalog;
use asimeow::config::Config;
use asimeow::config_edit::{apply, Addition};
use asimeow::recommend::{self, recommend};
use asimeow::tm;
use std::fs::{self, File};
use tempfile::tempdir;

fn add(rule: &str, folders: &[&str]) -> Addition {
    Addition {
        rule: rule.to_string(),
        file_match: String::new(),
        folders: folders.iter().map(|f| f.to_string()).collect(),
        existing: true,
    }
}

fn new_rule(rule: &str, file_match: &str, folders: &[&str]) -> Addition {
    Addition {
        rule: rule.to_string(),
        file_match: file_match.to_string(),
        folders: folders.iter().map(|f| f.to_string()).collect(),
        existing: false,
    }
}

/// Block lists with the items at the key's indent, comments, and a key after `rules`.
const BLOCK_STYLE: &str = "\
# my backup excludes
roots:
- path: ~/
rules:
- name: python
  file_match: requirements.txt
  exclusions:
  - __pycache__ # bytecode
  - .venv
- name: rust
  file_match: Cargo.toml
  exclusions:
  - target

ignore:
- .git
";

#[test]
fn test_block_style_keeps_comments_and_layout() -> Result<()> {
    let edited = apply(
        BLOCK_STYLE,
        &[
            add("python", &[".pytest_cache"]),
            new_rule("r", "*.Rproj", &[".Rproj.user"]),
        ],
    )?;
    assert_eq!(
        edited,
        "\
# my backup excludes
roots:
- path: ~/
rules:
- name: python
  file_match: requirements.txt
  exclusions:
  - __pycache__ # bytecode
  - .venv
  - .pytest_cache
- name: rust
  file_match: Cargo.toml
  exclusions:
  - target
- name: r
  file_match: '*.Rproj'
  exclusions:
  - .Rproj.user

ignore:
- .git
"
    );
    Ok(())
}

/// The repository's example: indented items and one-line quoted flow lists.
#[test]
fn test_flow_style_from_the_example_config() -> Result<()> {
    let text = fs::read_to_string("config.example.yaml")?;
    let edited = apply(
        &text,
        &[
            add("node", &[".next"]),
            new_rule("cmake", "CMakeLists.txt", &["cmake-build-debug"]),
        ],
    )?;
    assert!(edited.contains(r#"exclusions: ["node_modules", "dist", ".next"]"#));
    assert!(edited.contains(
        "  - name: cmake\n    file_match: CMakeLists.txt\n    exclusions:\n    - cmake-build-debug\n"
    ));
    let config: Config = serde_yaml::from_str(&edited)?;
    assert_eq!(config.rules.last().unwrap().name, "cmake");
    Ok(())
}

#[test]
fn test_unknown_rule_or_unsupported_layout_is_an_error() {
    assert!(apply(BLOCK_STYLE, &[add("node", &[".next"])]).is_err());
    let multiline_flow = "roots: []\nrules:\n- name: node\n  file_match: package.json\n  exclusions: [\n    node_modules]\n";
    assert!(apply(multiline_flow, &[add("node", &[".next"])]).is_err());
}

#[test]
fn test_recommend_apply_dry_run_then_for_real() -> Result<()> {
    let temp_dir = tempdir()?;
    let project = temp_dir.path().join("tests-project");
    fs::create_dir_all(project.join(".pytest_cache"))?;
    fs::write(project.join(".pytest_cache").join("v"), vec![0u8; 700])?;
    File::create(project.join("requirements.txt"))?;
    let config_path = temp_dir.path().join("config.yaml");
    let text = format!(
        "roots:\n- path: {}\nrules:\n- name: python\n  file_match: requirements.txt\n  exclusions:\n  - .venv\n",
        project.display()
    );
    fs::write(&config_path, &text)?;
    let config: Config = serde_yaml::from_str(&text)?;
    let report = recommend(&config, &catalog(), 2, None)?;

    let preview = recommend::apply(&report, &config_path, true)?;
    assert_eq!((preview.marked, preview.marked_bytes), (1, 700));
    assert_eq!(
        fs::read_to_string(&config_path)?,
        text,
        "dry run writes nothing"
    );
    assert!(!tm::has_exclusion_mark(&project.join(".pytest_cache")));

    let applied = recommend::apply(&report, &config_path, false)?;
    assert_eq!(applied.marked, 1);
    assert!(tm::has_exclusion_mark(&project.join(".pytest_cache")));
    assert_eq!(
        fs::read_to_string(config_path.with_extension("yaml.bak"))?,
        text
    );
    let updated: Config = serde_yaml::from_str(&fs::read_to_string(&config_path)?)?;
    assert_eq!(updated.rules[0].exclusions, vec![".venv", ".pytest_cache"]);

    let again = recommend(&updated, &catalog(), 2, None)?;
    assert!(again.suggestions.is_empty(), "nothing left to suggest");
    Ok(())
}
