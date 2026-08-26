use std::process::Command;

#[test]
fn help_succeeds() {
    let output = Command::new(env!("CARGO_BIN_EXE_clankervm-server"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--port"));
    assert!(!stdout.contains("--listen"));
    assert!(!stdout.contains("--setup-script"));
}
