#[test]
fn help_lists_all_subcommands() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_coder"))
        .arg("--help")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    for sub in [
        "build",
        "proxy",
        "models",
        "config",
        "doctor",
        "completions",
        "init",
        "run",
    ] {
        assert!(text.contains(sub), "missing subcommand: {sub}");
    }
    assert!(
        !text.contains("keys"),
        "no keys subcommand: provider keys live only in a plaintext secrets file"
    );
}

#[test]
fn version_flag_prints_the_crate_version() {
    for flag in ["--version", "-V"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_coder"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(output.status.success(), "{flag} should exit 0");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            concat!("coder ", env!("CARGO_PKG_VERSION")),
            "{flag}"
        );
    }
}
