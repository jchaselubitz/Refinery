//! Running Refinery as a background service.
//!
//! The daemon must be running for Overlord to submit a transcript, so
//! "install it and keep it running" is part of the product, not an exercise
//! left to the user. Each platform's own supervisor does the work: launchd on
//! macOS and a systemd user unit on Linux, both per-user rather than
//! system-wide, because Refinery holds one person's credentials and reads one
//! person's repositories.
//!
//! Three properties are worth stating, because they are what make `service
//! start` safe to run repeatedly:
//!
//! * Installing is idempotent. The unit file is rewritten from the current
//!   executable path and data directory every time, so a moved binary or a
//!   changed `--data-dir` is repaired by running the command again.
//! * Starting implies installing. A user who runs `refinery service start` on
//!   a machine that has never had the unit gets a running service, not an
//!   error telling them to run an install command they were not told about.
//! * Stopping a service that is not running succeeds, because the user's
//!   intent — that it not be running — is already satisfied.
//!
//! Platforms without a supported supervisor are not a failure: `refinery
//! serve` runs the daemon in the foreground everywhere, and that is what the
//! error says.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::DataDir;
use crate::error::{AppError, Result};

/// The reverse-DNS label both launchd and systemd know the service by.
pub const SERVICE_LABEL: &str = "io.cooperativ.refinery";

/// What the platform supervisor says about the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStatus {
    /// The unit is installed and the supervisor reports it running.
    Running {
        /// The process identifier, when the supervisor reports one.
        pid: Option<u32>,
    },
    /// The unit is installed but not currently running.
    Installed,
    /// No unit file is installed for this user.
    NotInstalled,
    /// This platform has no supported supervisor.
    Unsupported {
        /// The platform name, for the message the user sees.
        platform: &'static str,
    },
}

impl ServiceStatus {
    /// Whether the daemon is running under the supervisor.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    /// A one-line description for `status` and `doctor`.
    pub fn describe(&self) -> String {
        match self {
            Self::Running { pid: Some(pid) } => format!("running (pid {pid})"),
            Self::Running { pid: None } => "running".to_owned(),
            Self::Installed => "installed, not running".to_owned(),
            Self::NotInstalled => "not installed".to_owned(),
            Self::Unsupported { platform } => {
                format!("no supported service manager on {platform}; use `refinery serve`")
            }
        }
    }
}

/// Manages the background service for one installation.
#[derive(Debug, Clone)]
pub struct ServiceManager {
    data_dir: DataDir,
    executable: PathBuf,
    unit_path: Option<PathBuf>,
    /// Whether the platform supervisor is actually invoked. Always true for a
    /// manager built by [`ServiceManager::new`].
    supervised: bool,
}

impl ServiceManager {
    /// Build a manager for a data directory, resolving the executable to
    /// install.
    ///
    /// The path of the running binary is what goes into the unit file, so a
    /// service installed from a build directory keeps pointing at that build.
    /// That is the honest behaviour: the alternative — writing a `refinery`
    /// that is resolved from `PATH` at launch — silently starts a different
    /// program than the one the user ran.
    pub fn new(data_dir: DataDir) -> Result<Self> {
        let executable = std::env::current_exe()
            .map_err(|source| AppError::io("the running executable", source))?;
        Ok(Self {
            unit_path: unit_path(),
            data_dir,
            executable,
            supervised: true,
        })
    }

    /// A manager that writes and reports on a unit file at an explicit path but
    /// never invokes launchd or systemd.
    ///
    /// This exists so the setup flow and the service surface can be exercised
    /// end to end without a test installing a real login agent on the machine
    /// running it — which is a side effect no test is entitled to have.
    pub fn unsupervised_at(data_dir: DataDir, unit_path: PathBuf) -> Result<Self> {
        let executable = std::env::current_exe()
            .map_err(|source| AppError::io("the running executable", source))?;
        Ok(Self {
            unit_path: Some(unit_path),
            data_dir,
            executable,
            supervised: false,
        })
    }

    /// Where the unit file lives, when this platform has one.
    pub fn unit_path(&self) -> Option<&Path> {
        self.unit_path.as_deref()
    }

    /// Whether this platform has a supported supervisor.
    pub fn is_supported(&self) -> bool {
        self.unit_path.is_some()
    }

    /// Write (or rewrite) the unit file for the current executable and data
    /// directory.
    pub fn install(&self) -> Result<PathBuf> {
        let path = self.require_unit_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| AppError::io(parent, source))?;
        }
        std::fs::write(&path, self.unit_contents())
            .map_err(|source| AppError::io(&path, source))?;
        Ok(path)
    }

    /// Remove the unit file, reporting whether one was there.
    pub fn uninstall(&self) -> Result<bool> {
        let path = self.require_unit_path()?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(AppError::io(&path, source)),
        }
    }

    /// Install if needed, then ask the supervisor to start the service.
    pub fn start(&self) -> Result<PathBuf> {
        let path = self.install()?;
        if !self.supervised {
            return Ok(path);
        }
        // Best-effort: clearing a previously loaded definition is how a
        // reinstall picks up an edited unit, but its absence is not an error.
        for command in reload_commands(&path) {
            let _ = run_allowing_failure(command);
        }
        for command in start_commands(&path) {
            run(command)?;
        }
        Ok(path)
    }

    /// Ask the supervisor to stop the service. Stopping an already-stopped
    /// service succeeds.
    pub fn stop(&self) -> Result<()> {
        let path = self.require_unit_path()?;
        if !self.supervised {
            return Ok(());
        }
        for command in stop_commands(&path) {
            // A supervisor that reports "not loaded" has given us what we
            // asked for, so its exit status is not treated as a failure.
            let _ = run_allowing_failure(command);
        }
        Ok(())
    }

    /// Ask the supervisor what it thinks the service is doing.
    pub fn status(&self) -> ServiceStatus {
        let Some(path) = self.unit_path.as_deref() else {
            return ServiceStatus::Unsupported {
                platform: std::env::consts::OS,
            };
        };
        if !path.exists() {
            return ServiceStatus::NotInstalled;
        }
        if !self.supervised {
            return ServiceStatus::Installed;
        }
        query_status()
    }

    fn require_unit_path(&self) -> Result<PathBuf> {
        self.unit_path.clone().ok_or_else(|| {
            AppError::unsupported(
                format!(
                    "background service management on {} (run `refinery serve` instead)",
                    std::env::consts::OS
                ),
                "a future release",
            )
        })
    }

    /// The unit file text for this platform.
    ///
    /// Exposed for tests, which assert the generated unit rather than
    /// installing one into the developer's own login session.
    pub fn unit_contents(&self) -> String {
        let executable = self.executable.display();
        let data_dir = self.data_dir.root().display();
        let log_dir = self.data_dir.log_dir();

        #[cfg(target_os = "macos")]
        {
            // launchd needs its own stdout/stderr destinations; the daemon's
            // structured log still goes to the rotating file, and these catch
            // anything that escapes before logging is installed.
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{SERVICE_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{executable}</string>
    <string>--data-dir</string>
    <string>{data_dir}</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{stdout}</string>
  <key>StandardErrorPath</key>
  <string>{stderr}</string>
</dict>
</plist>
"#,
                stdout = log_dir.join("service.out.log").display(),
                stderr = log_dir.join("service.err.log").display(),
            )
        }

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let _ = &log_dir;
            format!(
                "[Unit]\n\
                 Description=Refinery prompt refinement service\n\
                 Documentation=https://github.com/cooperativ-labs/refinery\n\
                 After=network.target\n\
                 \n\
                 [Service]\n\
                 Type=simple\n\
                 ExecStart={executable} --data-dir {data_dir} serve\n\
                 Restart=on-failure\n\
                 RestartSec=5\n\
                 \n\
                 [Install]\n\
                 WantedBy=default.target\n"
            )
        }

        #[cfg(not(unix))]
        {
            let _ = (&log_dir, executable, data_dir);
            String::new()
        }
    }
}

/// Where this platform expects a per-user unit file.
fn unit_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = directories::UserDirs::new()?.home_dir().to_path_buf();
        Some(
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{SERVICE_LABEL}.plist")),
        )
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| directories::UserDirs::new().map(|dirs| dirs.home_dir().join(".config")))?;
        Some(base.join("systemd").join("user").join("refinery.service"))
    }

    #[cfg(not(unix))]
    {
        None
    }
}

/// Commands run before starting, whose failure is not a failure to start.
#[cfg(target_os = "macos")]
fn reload_commands(_path: &Path) -> Vec<Command> {
    // Unloading a previously bootstrapped definition is what makes an edited
    // plist take effect; there is nothing to unload on a first install.
    let mut bootout = Command::new("launchctl");
    bootout.args(["bootout", &format!("{}/{SERVICE_LABEL}", launchd_target())]);
    vec![bootout]
}

#[cfg(target_os = "macos")]
fn start_commands(path: &Path) -> Vec<Command> {
    let target = launchd_target();
    let mut bootstrap = Command::new("launchctl");
    bootstrap.arg("bootstrap").arg(&target).arg(path);
    let mut kickstart = Command::new("launchctl");
    kickstart.args(["kickstart", "-k", &format!("{target}/{SERVICE_LABEL}")]);
    vec![bootstrap, kickstart]
}

#[cfg(target_os = "macos")]
fn stop_commands(_path: &Path) -> Vec<Command> {
    let mut bootout = Command::new("launchctl");
    bootout.args(["bootout", &format!("{}/{SERVICE_LABEL}", launchd_target())]);
    vec![bootout]
}

/// The launchd domain for this login session, which is where a per-user agent
/// belongs.
#[cfg(target_os = "macos")]
fn launchd_target() -> String {
    format!("gui/{}", current_uid())
}

/// This user's numeric id.
///
/// Read through `id -u` rather than adding a libc dependency for one value;
/// its output is a decimal integer on every platform that has launchd.
#[cfg(target_os = "macos")]
fn current_uid() -> u32 {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn reload_commands(_path: &Path) -> Vec<Command> {
    let mut reload = Command::new("systemctl");
    reload.args(["--user", "daemon-reload"]);
    vec![reload]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn start_commands(_path: &Path) -> Vec<Command> {
    let mut enable = Command::new("systemctl");
    enable.args(["--user", "enable", "--now", "refinery.service"]);
    vec![enable]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn stop_commands(_path: &Path) -> Vec<Command> {
    let mut disable = Command::new("systemctl");
    disable.args(["--user", "disable", "--now", "refinery.service"]);
    vec![disable]
}

#[cfg(not(unix))]
fn reload_commands(_path: &Path) -> Vec<Command> {
    Vec::new()
}

#[cfg(not(unix))]
fn start_commands(_path: &Path) -> Vec<Command> {
    Vec::new()
}

#[cfg(not(unix))]
fn stop_commands(_path: &Path) -> Vec<Command> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn query_status() -> ServiceStatus {
    let output = Command::new("launchctl")
        .args(["print", &format!("{}/{SERVICE_LABEL}", launchd_target())])
        .output();
    let Ok(output) = output else {
        return ServiceStatus::Installed;
    };
    if !output.status.success() {
        return ServiceStatus::Installed;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    match text
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .and_then(|value| value.trim().parse().ok())
    {
        Some(pid) => ServiceStatus::Running { pid: Some(pid) },
        None => ServiceStatus::Installed,
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn query_status() -> ServiceStatus {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "refinery.service",
            "--property=MainPID,ActiveState",
        ])
        .output();
    let Ok(output) = output else {
        return ServiceStatus::Installed;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut pid = None;
    let mut active = false;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("MainPID=") {
            pid = value.trim().parse::<u32>().ok().filter(|value| *value != 0);
        }
        if let Some(value) = line.strip_prefix("ActiveState=") {
            active = value.trim() == "active" || value.trim() == "activating";
        }
    }
    if active {
        ServiceStatus::Running { pid }
    } else {
        ServiceStatus::Installed
    }
}

#[cfg(not(unix))]
fn query_status() -> ServiceStatus {
    ServiceStatus::Unsupported {
        platform: std::env::consts::OS,
    }
}

/// Run a supervisor command, turning a non-zero exit into an error that quotes
/// what the supervisor said.
fn run(mut command: Command) -> Result<()> {
    let name = describe(&command);
    let output = command
        .output()
        .map_err(|source| AppError::io(command.get_program(), source))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    Err(AppError::config(format!(
        "`{name}` failed{}",
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )))
}

fn run_allowing_failure(mut command: Command) -> bool {
    command
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn describe(command: &Command) -> String {
    let mut text = command.get_program().to_string_lossy().into_owned();
    for arg in command.get_args() {
        text.push(' ');
        text.push_str(&arg.to_string_lossy());
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> (tempfile::TempDir, ServiceManager) {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = DataDir::at(temp.path().join("data")).expect("resolve");
        let manager = ServiceManager::new(data_dir).expect("manager");
        (temp, manager)
    }

    #[test]
    fn the_status_descriptions_read_as_sentences() {
        assert_eq!(
            ServiceStatus::Running { pid: Some(42) }.describe(),
            "running (pid 42)"
        );
        assert_eq!(
            ServiceStatus::Installed.describe(),
            "installed, not running"
        );
        assert_eq!(ServiceStatus::NotInstalled.describe(), "not installed");
        assert!(ServiceStatus::Running { pid: None }.is_running());
        assert!(!ServiceStatus::Installed.is_running());
    }

    #[cfg(unix)]
    #[test]
    fn the_unit_names_this_executable_and_this_data_directory() {
        let (temp, manager) = manager();
        let unit = manager.unit_contents();

        assert!(
            unit.contains(&temp.path().join("data").display().to_string()),
            "the unit must pin the data directory it was installed for:\n{unit}"
        );
        assert!(
            unit.contains(
                &std::env::current_exe()
                    .expect("current exe")
                    .display()
                    .to_string()
            ),
            "the unit must pin the executable that installed it:\n{unit}"
        );
        assert!(
            unit.contains("serve"),
            "the unit must run the daemon:\n{unit}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_unit_is_a_launch_agent_plist() {
        let (_temp, manager) = manager();
        let unit = manager.unit_contents();
        assert!(unit.starts_with("<?xml"));
        assert!(unit.contains(SERVICE_LABEL));
        assert!(unit.contains("<key>RunAtLoad</key>"));
        assert!(manager
            .unit_path()
            .expect("path")
            .to_string_lossy()
            .contains("LaunchAgents"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_linux_unit_is_a_systemd_user_unit() {
        let (_temp, manager) = manager();
        let unit = manager.unit_contents();
        assert!(unit.contains("[Service]"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(manager
            .unit_path()
            .expect("path")
            .to_string_lossy()
            .contains("systemd/user"));
    }

    #[cfg(unix)]
    #[test]
    fn installing_is_idempotent_and_uninstall_reports_what_it_did() {
        // The unit is written to a temporary location rather than the
        // developer's real login session, so the test never touches launchd or
        // systemd.
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = DataDir::at(temp.path().join("data")).expect("resolve");
        let manager = ServiceManager::unsupervised_at(
            data_dir,
            temp.path().join("units").join("refinery.unit"),
        )
        .expect("manager");

        let first = manager.install().expect("install");
        let second = manager.install().expect("reinstall");
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(&first).expect("read"),
            manager.unit_contents()
        );

        assert!(manager.uninstall().expect("uninstall"));
        assert!(
            !manager.uninstall().expect("second uninstall"),
            "uninstalling twice must report that nothing was there"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_uninstalled_service_reports_not_installed() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = DataDir::at(temp.path().join("data")).expect("resolve");
        let manager = ServiceManager::unsupervised_at(
            data_dir,
            temp.path().join("units").join("absent.unit"),
        )
        .expect("manager");
        assert_eq!(manager.status(), ServiceStatus::NotInstalled);

        manager.install().expect("install");
        assert_eq!(manager.status(), ServiceStatus::Installed);
        // Starting an unsupervised manager writes the unit and stops there, so
        // a test can exercise the flow without touching the login session.
        manager.start().expect("start");
        manager.stop().expect("stop");
    }

    #[test]
    fn a_failing_command_quotes_what_the_supervisor_said() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo 'no such service' >&2; exit 1"]);
        let error = run(command).expect_err("must fail");
        assert!(error.to_string().contains("no such service"));
    }
}
