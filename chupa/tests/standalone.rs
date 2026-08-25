use std::process::Command;

#[test]
fn standalone_cli_owns_its_state_and_lists_registered_jobs() {
    let state = tempfile::tempdir().unwrap();
    let mirror = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_chupa");

    let add = Command::new(binary)
        .env("CHUPA_STATE_HOME", state.path())
        .args([
            "add",
            "cmd",
            "true",
            mirror.path().to_str().unwrap(),
            "3600",
        ])
        .output()
        .unwrap();
    assert!(add.status.success(), "{}", String::from_utf8_lossy(&add.stderr));
    assert_eq!(String::from_utf8_lossy(&add.stdout).trim(), "1");

    let list = Command::new(binary)
        .env("CHUPA_STATE_HOME", state.path())
        .arg("list")
        .output()
        .unwrap();
    assert!(list.status.success(), "{}", String::from_utf8_lossy(&list.stderr));
    let output = String::from_utf8_lossy(&list.stdout);
    assert!(output.contains("1\tcmd\tpending\ttrue\t"), "{output}");
    assert!(state.path().join("mirrors.db").is_file());
}

#[test]
fn multicall_drivers_are_available_without_sarun() {
    let mirrors = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_chupa"))
        .args(["gitdepot", "list", mirrors.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
