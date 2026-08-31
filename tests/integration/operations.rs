//! The operator surface of the installed executable: `doctor`, `status`,
//! `repository`, and `provider`, exercised as a user runs them.
//!
//! These tests drive the real binary against a throwaway data directory. They
//! deliberately never touch the machine's credential store or login session:
//! the fallback credential file is what a data directory with no OS store
//! yields on CI, and the service checks only read what the supervisor reports.

use std::path::Path;
use std::process::Command;

fn refinery(data_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_refinery"));
    command.env("REFINERY_DATA_DIR", data_dir);
    command.env_remove("REFINERY_LOG");
    // A test must never read or write the credential store of the machine
    // running it. This is the same escape hatch a user with a broken keyring
    // reaches for, so the tests exercise a supported configuration.
    command.env("REFINERY_CREDENTIALS", "file");
    command
}

/// Run a command and return (success, stdout, stderr).
fn run(command: &mut Command) -> (bool, String, String) {
    let output = command.output().expect("run refinery");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn doctor_reports_every_check_with_a_remedy_for_each_problem() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (success, stdout, _stderr) =
        run(refinery(&temp.path().join("data")).args(["doctor", "--offline"]));

    // A fresh installation has no API key and no local token, so doctor must
    // exit non-zero: a health check that always succeeds tells nobody anything.
    assert!(!success, "doctor must fail on an unconfigured installation");

    for check in [
        "data directory",
        "database",
        "credential store",
        "provider",
        "repositories",
        "local API",
        "Overlord",
        "service",
    ] {
        assert!(
            stdout.contains(check),
            "doctor omitted the `{check}` check:\n{stdout}"
        );
    }

    // Every failing line is followed by a remedy line.
    let lines: Vec<_> = stdout.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("fail") || line.starts_with("warn") {
            assert!(
                lines.get(index + 1).is_some_and(|next| next.contains('→')),
                "`{line}` has no remedy line:\n{stdout}"
            );
        }
    }
    assert!(stdout.contains("run `refinery provider configure gemini`"));
}

#[cfg(unix)]
#[test]
fn a_loosened_data_directory_repairs_itself_on_the_next_run() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");

    // Create the directory through the product itself, then break it.
    run(refinery(&data_dir).args(["status"]));
    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o755)).expect("loosen");

    let (_success, stdout, _stderr) = run(refinery(&data_dir).args(["doctor", "--offline"]));
    assert!(
        stdout.contains("present and owner-only"),
        "a loosened directory should be repaired on start:\n{stdout}"
    );
    let mode = std::fs::metadata(&data_dir)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "the directory was not tightened again");
}

#[cfg(unix)]
#[test]
fn doctor_detects_a_world_readable_credential_file_and_says_how_to_repair_it() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");
    run(refinery(&data_dir).args(["status"]));

    // A file, unlike a directory, is not rewritten on start, so this is the
    // exposure that survives to be diagnosed.
    let credentials = data_dir.join("credentials");
    std::fs::create_dir_all(&credentials).expect("create credentials dir");
    let path = credentials.join("credentials.json");
    std::fs::write(&path, r#"{"secrets":{}}"#).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("loosen");

    let (_success, stdout, _stderr) = run(refinery(&data_dir).args(["doctor", "--offline"]));
    assert!(
        stdout.contains("readable beyond its owner"),
        "doctor did not detect the exposed credential file:\n{stdout}"
    );
    assert!(
        stdout.contains("chmod 600"),
        "no repair instruction:\n{stdout}"
    );
}

#[test]
fn doctor_detects_a_repository_that_has_been_moved_away() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("create project");

    let (added, stdout, stderr) = run(refinery(&data_dir)
        .args(["repository", "add"])
        .arg(&project));
    assert!(added, "repository add failed: {stderr}");
    assert!(stdout.contains("Registered"), "{stdout}");

    std::fs::remove_dir_all(&project).expect("remove project");

    let (_success, stdout, _stderr) = run(refinery(&data_dir).args(["doctor", "--offline"]));
    assert!(
        stdout.contains("registered repositories are missing"),
        "doctor did not notice the missing repository:\n{stdout}"
    );
    assert!(stdout.contains("refinery repository add"), "{stdout}");
}

#[test]
fn doctor_never_prints_a_stored_credential() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");
    let secret = "AIzaIntegrationSecretKeyValue00";

    // Write the key into the fallback credential file directly, which is the
    // shape a machine without an OS credential store produces.
    run(refinery(&data_dir).args(["status"]));
    let credentials = data_dir.join("credentials");
    std::fs::create_dir_all(&credentials).expect("create credentials dir");
    std::fs::write(
        credentials.join("credentials.json"),
        format!(r#"{{"secrets":{{"provider.gemini.api_key":"{secret}"}}}}"#),
    )
    .expect("write credential file");

    let (_success, stdout, stderr) = run(refinery(&data_dir).args(["doctor", "--offline"]));
    assert!(
        !stdout.contains(secret) && !stderr.contains(secret),
        "doctor leaked a stored credential:\n{stdout}\n{stderr}"
    );

    // Nor may the key reach the log file, which is the other place it could
    // escape to and the one nobody would notice.
    for entry in std::fs::read_dir(data_dir.join("logs")).expect("read logs") {
        let path = entry.expect("entry").path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            !text.contains(secret),
            "a credential reached {}",
            path.display()
        );
    }
}

#[test]
fn repositories_are_registered_listed_and_forgotten_by_canonical_path() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");
    let project = temp.path().join("project");
    std::fs::create_dir_all(project.join("src")).expect("create project");

    let (empty_ok, stdout, _) = run(refinery(&data_dir).args(["repository", "list"]));
    assert!(empty_ok);
    assert!(stdout.contains("No repositories registered"), "{stdout}");

    // Registering through a nested path plus `..` must produce the same
    // repository as registering the root directly.
    run(refinery(&data_dir)
        .args(["repository", "add"])
        .arg(project.join("src").join("..")));
    run(refinery(&data_dir)
        .args(["repository", "add"])
        .arg(&project));

    let (_ok, stdout, _) = run(refinery(&data_dir).args(["repository", "list"]));
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.contains("project"))
            .count(),
        1,
        "the same root was registered twice:\n{stdout}"
    );

    let (_ok, stdout, _) = run(refinery(&data_dir)
        .args(["repository", "remove"])
        .arg(&project));
    assert!(stdout.contains("Forgot"), "{stdout}");
    assert!(
        project.is_dir(),
        "forgetting a repository must not touch it"
    );
}

#[test]
fn registering_something_that_is_not_a_project_is_refused_with_a_reason() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");
    let file = temp.path().join("README.md");
    std::fs::write(&file, "hello").expect("write");

    let (success, _stdout, stderr) =
        run(refinery(&data_dir).args(["repository", "add"]).arg(&file));
    assert!(!success);
    assert!(stderr.contains("register the directory"), "{stderr}");

    let (success, _stdout, stderr) = run(refinery(&data_dir).args(["repository", "add", "/"]));
    assert!(!success);
    assert!(stderr.contains("too broad"), "{stderr}");
}

#[test]
fn status_reports_the_installation_without_revealing_secrets() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");

    let (success, stdout, stderr) = run(refinery(&data_dir).arg("status"));
    assert!(success, "{stderr}");
    for field in [
        "data directory",
        "provider",
        "credentials",
        "overlord",
        "service",
        "repositories",
        "cases",
        "product metrics",
    ] {
        assert!(
            stdout.contains(field),
            "status omitted `{field}`:\n{stdout}"
        );
    }
    assert!(stdout.contains("not stored"), "{stdout}");
    for metric in [
        "delivered",
        "question rate",
        "answer resume",
        "deliveries",
        "validation",
    ] {
        assert!(
            stdout.contains(metric),
            "status omitted product metric `{metric}`:\n{stdout}"
        );
    }
}

#[test]
fn a_non_interactive_setup_says_it_needs_a_terminal_rather_than_hanging() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (success, _stdout, stderr) =
        run(refinery(&temp.path().join("data")).args(["setup", "--offline"]));
    assert!(!success);
    assert!(stderr.contains("terminal"), "{stderr}");
}

#[test]
fn configuring_an_unconfigured_provider_names_the_setting_to_change() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (success, _stdout, stderr) = run(refinery(&temp.path().join("data")).args([
        "provider",
        "configure",
        "openai",
        "--offline",
    ]));
    assert!(!success);
    assert!(stderr.contains("provider.backend"), "{stderr}");
}

#[test]
fn service_status_answers_without_touching_the_login_session() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (success, stdout, stderr) =
        run(refinery(&temp.path().join("data")).args(["service", "status"]));
    assert!(success, "{stderr}");
    assert!(!stdout.trim().is_empty(), "service status printed nothing");
}

#[test]
fn the_credential_backend_can_be_pinned_to_a_file() {
    // The escape hatch for a machine whose keyring is broken or prompting.
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("data");
    let (success, stdout, stderr) = run(refinery(&data_dir).arg("status"));

    assert!(success, "{stderr}");
    assert!(
        stdout.contains("owner-only file"),
        "the pinned backend was not used:\n{stdout}"
    );
}
