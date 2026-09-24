use std::{
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[test]
fn runtime_executes_javascript_files_from_the_cli() {
    let script_path = unique_temp_script_path();

    fs::write(&script_path, "console.log('hello from runtime');")
        .expect("script should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_runtime"))
        .arg(&script_path)
        .output()
        .expect("runtime binary should execute");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("hello from runtime"));

    let _ = fs::remove_file(script_path);
}

#[test]
fn runtime_requires_a_script_path() {
    let output = Command::new(env!("CARGO_BIN_EXE_runtime"))
        .output()
        .expect("runtime binary should execute");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage:"));
}

fn unique_temp_script_path() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be valid")
        .as_nanos();

    std::env::temp_dir().join(format!("rust-node-cli-{nanos}.js"))
}
