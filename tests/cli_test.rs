use std::process::Command;

/// Runs the asimeow binary with `args`; returns (success, stdout + stderr).
fn asimeow(args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_asimeow"))
        .args(args)
        .output()
        .expect("asimeow runs");
    let text = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    (output.status.success(), text)
}

#[test]
fn test_json_prints_one_parseable_object_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("app");
    std::fs::create_dir_all(project.join("node_modules")).unwrap();
    std::fs::write(project.join("package.json"), "{}").unwrap();
    let config = format!(
        "roots:\n  - path: {}\nignore: []\nrules:\n  - name: node\n    file_match: package.json\n    exclusions:\n      - node_modules\n",
        project.display()
    );
    let config_path = dir.path().join("config.yaml");
    std::fs::write(&config_path, config).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_asimeow"))
        .args(["--json", "--dry-run", "-c", config_path.to_str().unwrap()])
        .output()
        .expect("asimeow runs");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout is exactly one JSON object");
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["new"], 1);
    assert_eq!(report["changed"][0]["kind"], "new");
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_shared_options_work_after_a_subcommand() {
    for args in [
        ["recommend", "-c", "/nonexistent/asimeow.yaml"],
        ["-c", "/nonexistent/asimeow.yaml", "recommend"],
    ] {
        let (ok, text) = asimeow(&args);
        assert!(!ok);
        assert!(
            text.contains("Specified config file not found: /nonexistent/asimeow.yaml"),
            "{args:?} should reach the config lookup, got: {text}"
        );
    }
    let (ok, text) = asimeow(&["stats", "-v", "-t", "2"]);
    assert!(ok, "{text}");
}
