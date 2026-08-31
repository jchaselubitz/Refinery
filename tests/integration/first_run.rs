//! First-run behaviour of the installed executable: the help surface, data
//! directory creation, and honest failures for capabilities not built yet.

use std::path::Path;
use std::process::Command;

/// The executable Cargo built for this test run.
fn refinery(data_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_refinery"));
    command.env("REFINERY_DATA_DIR", data_dir);
    // Keep the developer's ambient log filter out of the assertions.
    command.env_remove("REFINERY_LOG");
    // Never touch the credential store of the machine running the tests.
    command.env("REFINERY_CREDENTIALS", "file");
    command
}

#[test]
fn help_lists_the_command_surface() {
    let temp = tempfile::tempdir().expect("temp dir");
    let output = refinery(&temp.path().join("data"))
        .arg("--help")
        .output()
        .expect("run refinery --help");

    assert!(output.status.success(), "--help must exit successfully");
    let help = String::from_utf8(output.stdout).expect("utf-8 help");
    for command in [
        "setup",
        "open",
        "status",
        "doctor",
        "serve",
        "repository",
        "provider",
        "service",
    ] {
        assert!(help.contains(command), "--help should mention {command}");
    }
}

#[test]
fn version_reports_the_crate_version() {
    let temp = tempfile::tempdir().expect("temp dir");
    let output = refinery(&temp.path().join("data"))
        .arg("--version")
        .output()
        .expect("run refinery --version");

    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).expect("utf-8");
    assert!(text.contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn the_first_run_creates_a_private_data_directory() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");

    let output = refinery(&data_dir)
        .arg("status")
        .output()
        .expect("run refinery status");
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for subdirectory in ["", "media", "logs", "credentials", "run"] {
        let path = data_dir.join(subdirectory);
        assert!(path.is_dir(), "{} should exist", path.display());
        assert_owner_only(&path);
    }

    let stdout = String::from_utf8(output.stdout).expect("utf-8 status");
    assert!(stdout.contains(&data_dir.display().to_string()));
    assert!(stdout.contains("gemini"));
}

#[test]
fn the_first_run_writes_a_log_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");

    let status = refinery(&data_dir)
        .arg("status")
        .env("REFINERY_LOG", "debug")
        .status()
        .expect("run refinery status");
    assert!(status.success());

    let logs: Vec<_> = std::fs::read_dir(data_dir.join("logs"))
        .expect("read log directory")
        .filter_map(Result::ok)
        .collect();
    assert!(!logs.is_empty(), "a log file should have been written");
}

/// `refinery open` is how a person reaches the interface, and the token in the
/// URL is the only way a browser launch can authenticate its first request.
/// The same installation must hand out the same token, or every launch would
/// invalidate the last window.
#[test]
fn open_prints_a_stable_tokened_loopback_url() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");

    let first = refinery(&data_dir)
        .args(["open", "--print"])
        .output()
        .expect("run refinery open");
    assert!(
        first.status.success(),
        "open --print failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let url = String::from_utf8(first.stdout).expect("utf-8 stdout");
    let url = url.trim();
    assert!(url.starts_with("http://127.0.0.1:"), "not loopback: {url}");
    let token = url.split("?token=").nth(1).expect("a token in the URL");
    assert!(token.len() >= 16, "the token looks too short: {token}");

    let second = refinery(&data_dir)
        .args(["open", "--print"])
        .output()
        .expect("run refinery open again");
    assert_eq!(
        String::from_utf8(second.stdout)
            .expect("utf-8 stdout")
            .trim(),
        url,
        "a second launch must reuse the stored token"
    );
}

#[test]
fn an_invalid_settings_file_refuses_to_start() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    std::fs::write(data_dir.join("refinery.toml"), "[api]\nport = \"nine\"\n").expect("write");

    let output = refinery(&data_dir)
        .arg("status")
        .output()
        .expect("run refinery status");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(stderr.contains("not valid settings"), "got: {stderr}");
}

#[cfg(unix)]
fn assert_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "{} should be owner-only", path.display());
}

#[cfg(not(unix))]
fn assert_owner_only(_path: &Path) {}
