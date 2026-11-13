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
