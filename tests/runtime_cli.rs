use std::{
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[test]
fn runtime_executes_javascript_files_from_the_cli() {
    let project_dir = unique_temp_directory();
    let script_path = project_dir.join("main.js");

    fs::write(project_dir.join("answer.js"), "module.exports = 42;")
        .expect("module should be written");
    fs::write(
        &script_path,
        r#"
        const answer = require("./answer");
        console.log(answer);
        setTimeout(() => {
          console.log("done");
        }, 0);
        "#,
    )
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("42"));
    assert!(stdout.contains("done"));
}

#[test]
fn runtime_requires_a_script_path() {
    let output = Command::new(env!("CARGO_BIN_EXE_runtime"))
        .output()
        .expect("runtime binary should execute");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage:"));
}

fn unique_temp_directory() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be valid")
        .as_nanos();

    let path = std::env::temp_dir().join(format!("rust-node-cli-{nanos}"));
    fs::create_dir_all(&path).expect("temp directory should be created");
    path
}
