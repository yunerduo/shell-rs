//! Windows: the setup program updates its own install.
//!
//! ShellRS is installed per user by an Inno Setup program, which owns the
//! files, the shortcuts and the entry in 应用和功能. An update runs the new
//! version's setup silently, the same way the person ran the first one. A
//! running program cannot be overwritten, so a hidden PowerShell waits for
//! ShellRS to exit, runs the setup, and starts ShellRS again when asked to.
//!
//! The same AppId may be installed in more than one folder, and Inno would
//! otherwise always pick its default one. The setup is therefore told which
//! folder the running copy came from (`/DIR`), writes a log there, and its
//! exit code is checked: a failed or misdirected install is written down
//! instead of restarting the old version and leaving the person guessing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use semver::Version;

use super::error::UpdateError;
use super::install::{
    INSTALL_LOG, INSTALL_RESULT, InstallKind, Installer, Launch, Relaunch, Staged,
};
use super::manifest::Release;
use crate::i18n::t;

/// Waits for ShellRS to exit, runs the setup into the folder this copy came
/// from, then starts ShellRS again if the install went through. Values come
/// in through the environment, not the script's text, so no path needs
/// escaping; Windows PowerShell 5.1 joins the argument array with spaces
/// and keeps quotes as written, so `/DIR` and `/LOG` bring their own.
const WAITER: &str = r#"$ErrorActionPreference = 'SilentlyContinue'
$waitFor = [int]$env:SHELLAS_UPDATE_PID
while (Get-Process -Id $waitFor) { Start-Sleep -Milliseconds 100 }
$arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-')
$arguments += ('/DIR="{0}"' -f $env:SHELLAS_UPDATE_DIR)
$arguments += ('/LOG="{0}"' -f $env:SHELLAS_UPDATE_LOG)
$setup = Start-Process -FilePath $env:SHELLAS_UPDATE_SETUP -ArgumentList $arguments -Wait -PassThru
$code = -1
if ($setup) { $code = $setup.ExitCode }
$reason = ''
if ($code -ne 0) {
  $reason = 'installer_failed'
} else {
  $installed = ''
  if (Test-Path -LiteralPath $env:SHELLAS_UPDATE_APP) {
    $installed = (Get-Item -LiteralPath $env:SHELLAS_UPDATE_APP).VersionInfo.ProductVersion
  }
  $version = $env:SHELLAS_UPDATE_VERSION
  if (-not (($installed -eq $version) -or ($installed -like ($version + '.*')))) {
    $reason = 'installed_elsewhere'
  }
}
$result = @{ reason = $reason; exit_code = $code } | ConvertTo-Json -Compress
[IO.File]::WriteAllText($env:SHELLAS_UPDATE_RESULT, $result, (New-Object System.Text.UTF8Encoding($false)))
if (($reason -eq '') -and ($env:SHELLAS_UPDATE_RELAUNCH -eq '1')) {
  Start-Process -FilePath $env:SHELLAS_UPDATE_APP
}
"#;

pub struct SetupInstaller {
    kind: InstallKind,
    exe: PathBuf,
    /// Downloads, the setup log and the hand-over's answer.
    folder: PathBuf,
}

impl SetupInstaller {
    pub fn new(exe: PathBuf, folder: PathBuf) -> Self {
        Self {
            kind: InstallKind::WindowsSetup { exe: exe.clone() },
            exe,
            folder,
        }
    }
}

/// Everything the hidden PowerShell needs; `dir` is the folder the running
/// copy came from, `result` where it writes what happened.
pub struct Waiter<'a> {
    pub pid: u32,
    pub setup: &'a Path,
    pub app: &'a Path,
    pub dir: &'a Path,
    pub log: &'a Path,
    pub result: &'a Path,
    pub version: &'a Version,
    pub relaunch: bool,
}

/// The hidden PowerShell that installs `setup` once process `pid` is gone.
pub fn waiter(waiter: Waiter<'_>) -> Launch {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
    let program = PathBuf::from(root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let env = |name: &str, value: OsString| (OsString::from(name), value);
    Launch {
        program,
        args: [
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-Command",
            WAITER,
        ]
        .map(OsString::from)
        .to_vec(),
        envs: vec![
            env("SHELLAS_UPDATE_PID", waiter.pid.to_string().into()),
            env("SHELLAS_UPDATE_SETUP", waiter.setup.as_os_str().to_owned()),
            env("SHELLAS_UPDATE_APP", waiter.app.as_os_str().to_owned()),
            env("SHELLAS_UPDATE_DIR", waiter.dir.as_os_str().to_owned()),
            env("SHELLAS_UPDATE_LOG", waiter.log.as_os_str().to_owned()),
            env(
                "SHELLAS_UPDATE_RESULT",
                waiter.result.as_os_str().to_owned(),
            ),
            env("SHELLAS_UPDATE_VERSION", waiter.version.to_string().into()),
            env(
                "SHELLAS_UPDATE_RELAUNCH",
                if waiter.relaunch { "1" } else { "0" }.into(),
            ),
        ],
    }
}

impl Installer for SetupInstaller {
    fn kind(&self) -> &InstallKind {
        &self.kind
    }

    /// The verified setup program is run as it is.
    fn stage(&self, package: &Path, release: &Release) -> Result<Staged, UpdateError> {
        Ok(Staged {
            version: release.version.clone(),
            path: package.to_path_buf(),
        })
    }

    fn apply(&self, staged: &Staged, relaunch: bool) -> Result<Relaunch, UpdateError> {
        if !staged.path.is_file() {
            return Err(UpdateError::Install(
                t!("update.install.setup_missing").to_string(),
            ));
        }
        let dir = self.exe.parent().unwrap_or_else(|| Path::new("."));
        Ok(Relaunch::Hand(waiter(Waiter {
            pid: std::process::id(),
            setup: &staged.path,
            app: &self.exe,
            dir,
            log: &self.folder.join(INSTALL_LOG),
            result: &self.folder.join(INSTALL_RESULT),
            version: &staged.version,
            relaunch,
        })))
    }

    /// The setup program removes what the previous version left.
    // The package waits in the downloads folder, which the updater tidies.
    fn clean_up(&self, _: &Version) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch_waiter(pid: u32, setup: &Path, app: &Path, relaunch: bool) -> Launch {
        waiter(Waiter {
            pid,
            setup,
            app,
            dir: Path::new(r"C:\Users\me\AppData\Local\Programs\ShellAs"),
            log: Path::new(r"C:\Users\me\AppData\Local\shellas\updates\install.log"),
            result: Path::new(r"C:\Users\me\AppData\Local\shellas\updates\install-result.json"),
            version: &Version::new(0, 2, 0),
            relaunch,
        })
    }

    #[test]
    fn the_windows_waiter_runs_the_installer_then_relaunches() {
        let setup = Path::new(r"C:\Users\me\AppData\Local\shellas\updates\0.2.0\setup.exe");
        let app = Path::new(r"C:\Users\me\AppData\Local\Programs\ShellAs\shellas.exe");
        let launch = launch_waiter(4242, setup, app, true);
        // A string compare: on the Mac running the tests, backslashes are
        // not separators.
        let program = launch.program.to_string_lossy().replace('/', "\\");
        assert!(
            program.ends_with(r"\System32\WindowsPowerShell\v1.0\powershell.exe"),
            "{program}"
        );
        assert_eq!(
            launch.args.last().map(|script| script.to_str().unwrap()),
            Some(WAITER)
        );
        let env = |name: &str| {
            launch
                .envs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(env("SHELLAS_UPDATE_PID"), Some("4242".into()));
        assert_eq!(env("SHELLAS_UPDATE_SETUP"), Some(setup.as_os_str().into()));
        assert_eq!(env("SHELLAS_UPDATE_APP"), Some(app.as_os_str().into()));
        assert_eq!(
            env("SHELLAS_UPDATE_DIR"),
            Some(
                Path::new(r"C:\Users\me\AppData\Local\Programs\ShellAs")
                    .as_os_str()
                    .into()
            )
        );
        assert_eq!(
            env("SHELLAS_UPDATE_LOG"),
            Some(
                Path::new(r"C:\Users\me\AppData\Local\shellas\updates\install.log")
                    .as_os_str()
                    .into()
            )
        );
        assert_eq!(
            env("SHELLAS_UPDATE_RESULT"),
            Some(
                Path::new(r"C:\Users\me\AppData\Local\shellas\updates\install-result.json")
                    .as_os_str()
                    .into()
            )
        );
        assert_eq!(env("SHELLAS_UPDATE_VERSION"), Some("0.2.0".into()));
        assert_eq!(env("SHELLAS_UPDATE_RELAUNCH"), Some("1".into()));
        assert_eq!(
            launch_waiter(1, setup, app, false)
                .envs
                .iter()
                .find(|(key, _)| key == "SHELLAS_UPDATE_RELAUNCH")
                .map(|(_, value)| value.clone()),
            Some("0".into())
        );
        // Silent, without the setup's own 继续吗？ question, into this copy's
        // folder, with a log, and looking at the exit code.
        for switch in [
            "/VERYSILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/SP-",
            "/DIR=",
            "/LOG=",
        ] {
            assert!(WAITER.contains(switch), "{switch}");
        }
        for expected in ["ExitCode", "installer_failed", "installed_elsewhere"] {
            assert!(WAITER.contains(expected), "{expected}");
        }
        // The old version is only started when the install went through.
        assert!(WAITER.contains("($reason -eq '')"));
    }

    #[test]
    fn a_missing_setup_is_not_handed_over() {
        let installer = SetupInstaller::new(
            PathBuf::from(r"C:\ShellAs\shellas.exe"),
            PathBuf::from(r"C:\Users\me\AppData\Local\shellas\updates"),
        );
        let staged = Staged {
            version: semver::Version::new(0, 2, 0),
            path: PathBuf::from("/nowhere/setup.exe"),
        };
        assert!(matches!(
            installer.apply(&staged, true),
            Err(UpdateError::Install(_))
        ));
    }
}
