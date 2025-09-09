use std::{
  collections::HashMap,
  ffi::{OsStr, OsString},
  path::PathBuf,
  sync::{Mutex, OnceLock},
};

use color_eyre::{
  Result,
  eyre::{self, Context, bail},
};
use secrecy::{ExposeSecret, SecretString};
use subprocess::{Exec, ExitStatus, Redirection};
use thiserror::Error;
use tracing::{debug, info, warn};
use which::which;

use crate::{
  installable::Installable,
  interface::NixBuildPassthroughArgs,
};

static PASSWORD_CACHE: OnceLock<Mutex<HashMap<String, SecretString>>> =
  OnceLock::new();

fn get_cached_password(host: &str) -> Option<SecretString> {
  let cache = PASSWORD_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
  let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
  guard.get(host).cloned()
}

fn cache_password(host: &str, password: SecretString) {
  let cache = PASSWORD_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
  let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
  guard.insert(host.to_string(), password);
}

fn ssh_wrap(
  cmd: Exec,
  ssh: Option<&str>,
  password: Option<&SecretString>,
) -> Exec {
  if let Some(ssh) = ssh {
    let mut ssh_cmd = Exec::cmd("ssh")
      .arg("-T")
      .arg(ssh)
      .arg(cmd.to_cmdline_lossy());

    if let Some(pwd) = password {
      ssh_cmd = ssh_cmd.stdin(format!("{}\n", pwd.expose_secret()).as_str());
    }

    ssh_cmd
  } else {
    cmd
  }
}

#[allow(dead_code)] // shut up
#[derive(Debug, Clone)]
pub enum EnvAction {
  /// Set an environment variable to a specific value
  Set(String),

  /// Preserve an environment variable from the current environment
  Preserve,

  /// Remove/unset an environment variable
  Remove,
}

/// Strategy for choosing a privilege elevation program.
/// - `Auto`: try supported programs in fallback order.
/// - `Prefer(PathBuf)`: try the specified program, then fallback.
/// - `Force(&'static str)`: use only the specified program, error if not
///   available.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElevationStrategy {
  Auto,
  Prefer(PathBuf),
  Force(&'static str),
}

impl ElevationStrategy {
  pub fn resolve(&self) -> Result<PathBuf> {
    match self {
      ElevationStrategy::Auto => Self::choice(),
      ElevationStrategy::Prefer(program) => {
        which(program).or_else(|_| {
          let auto = Self::choice()?;
          warn!(
            "{} not found. Using {} instead",
            program.to_string_lossy(),
            auto.to_string_lossy()
          );
          Ok(auto)
        })
      },
      ElevationStrategy::Force(program) => Ok(program.into()),
    }
  }

  /// Gets a path to a privilege elevation program based on what is available in
  /// the system.
  ///
  /// This funtion checks for the existence of common privilege elevation
  /// program names in the `PATH` using the `which` crate and returns a Ok
  /// result with the `OsString` of the path to the binary. In the case none
  /// of the checked programs are found a Err result is returned.
  ///
  /// The search is done in this order:
  ///
  /// 1. `doas`
  /// 2. `sudo`
  /// 3. `run0`
  /// 4. `pkexec`
  ///
  /// The logic for choosing this order is that a person with `doas` installed
  /// is more likely to be using it as their main privilege elevation program.
  /// `run0` and `pkexec` are preinstalled in any `NixOS` system with polkit
  /// support installed, so they have been placed lower as it's easier to
  /// deactivate sudo than it is to remove `run0`/`pkexec`
  ///
  /// # Returns
  ///
  /// * `Result<PathBuf>` - The absolute path to the privilege elevation program
  ///   binary or an error if a program can't be found.
  fn choice() -> Result<PathBuf> {
    const STRATEGIES: [&str; 4] = ["doas", "sudo", "run0", "pkexec"];

    for strategy in STRATEGIES {
      if let Ok(path) = which(strategy) {
        debug!(?path, "{strategy} path found");
        return Ok(path);
      }
    }

    Err(eyre::eyre!(
      "No elevation strategy found. Checked: {}",
      STRATEGIES.join(", ")
    ))
  }
}

#[derive(Debug)]
pub struct Command {
  dry:         bool,
  message:     Option<String>,
  command:     OsString,
  args:        Vec<OsString>,
  elevate:     Option<ElevationStrategy>,
  ssh:         Option<String>,
  show_output: bool,
  env_vars:    HashMap<String, EnvAction>,
}

impl Command {
  pub fn new<S: AsRef<OsStr>>(command: S) -> Self {
    Self {
      dry:         false,
      message:     None,
      command:     command.as_ref().to_os_string(),
      args:        vec![],
      elevate:     None,
      ssh:         None,
      show_output: false,
      env_vars:    HashMap::new(),
    }
  }

  /// Set whether to run the command with elevated privileges.
  #[must_use]
  pub fn elevate(mut self, elevate: Option<ElevationStrategy>) -> Self {
    self.elevate = elevate;
    self
  }

  /// Set whether to perform a dry run.
  #[must_use]
  pub fn dry(mut self, dry: bool) -> Self {
    self.dry = dry;
    self
  }

  /// Set whether to show command output.
  #[must_use]
  pub fn show_output(mut self, show_output: bool) -> Self {
    self.show_output = show_output;
    self
  }

  /// Set the SSH target for remote command execution.
  #[must_use]
  pub fn ssh(mut self, ssh: Option<String>) -> Self {
    self.ssh = ssh;
    self
  }

  /// Add a single argument to the command.
  #[must_use]
  pub fn arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
    self.args.push(arg.as_ref().to_os_string());
    self
  }

  /// Add multiple arguments to the command.
  #[must_use]
  pub fn args<I>(mut self, args: I) -> Self
  where
    I: IntoIterator,
    I::Item: AsRef<OsStr>,
  {
    for elem in args {
      self.args.push(elem.as_ref().to_os_string());
    }
    self
  }

  /// Set a message to display before running the command.
  #[must_use]
  pub fn message<S: AsRef<str>>(mut self, message: S) -> Self {
    self.message = Some(message.as_ref().to_string());
    self
  }

  /// Preserve multiple environment variables from the current environment
  #[must_use]
  pub fn preserve_envs<I, K>(mut self, keys: I) -> Self
  where
    I: IntoIterator<Item = K>,
    K: AsRef<str>,
  {
    for key in keys {
      let key_str = key.as_ref().to_string();
      self.env_vars.insert(key_str, EnvAction::Preserve);
    }
    self
  }

  /// Configure environment for Nix and NH operations
  #[must_use]
  pub fn with_required_env(mut self) -> Self {
    // Centralized list of environment variables to preserve
    // This is not a part of Nix's environment, but it might be necessary.
    // nixos-rebuild preserves it, so we do too.
    const PRESERVE_ENV: &[&str] = &[
      "LOCALE_ARCHIVE",
      // PATH needs to be preserved so that NH can invoke CLI utilities.
      "PATH",
      // Make sure NIX_SSHOPTS applies to nix commands that invoke ssh, such as
      // `nix copy`
      "NIX_SSHOPTS",
      // This is relevant for Home-Manager systems
      "HOME_MANAGER_BACKUP_EXT",
      // Preserve other Nix-related environment variables
      // TODO: is this everything we need? Previously we only preserved *some*
      // variables and nh continued to work, but any missing vars might
      // break functionality completely unexpectedly. This list could
      // change at any moment. This better be enough. Ugh.
      "NIX_CONFIG",
      "NIX_PATH",
      "NIX_REMOTE",
      "NIX_SSL_CERT_FILE",
      "NIX_USER_CONF_FILES",
    ];

    // Always explicitly set USER if present
    if let Ok(user) = std::env::var("USER") {
      self
        .env_vars
        .insert("USER".to_string(), EnvAction::Set(user));
    }

    // Only propagate HOME for non-elevated commands
    if self.elevate.is_none() {
      if let Ok(home) = std::env::var("HOME") {
        self
          .env_vars
          .insert("HOME".to_string(), EnvAction::Set(home));
      }
    }

    // Preserve all variables in PRESERVE_ENV if present
    for &key in PRESERVE_ENV {
      if std::env::var(key).is_ok() {
        self.env_vars.insert(key.to_string(), EnvAction::Preserve);
      }
    }

    // Explicitly set NH_* variables
    for (key, value) in std::env::vars() {
      if key.starts_with("NH_") {
        self.env_vars.insert(key, EnvAction::Set(value));
      }
    }

    debug!(
      "Configured envs: {}",
      self
        .env_vars
        .iter()
        .map(|(key, action)| {
          match action {
            EnvAction::Set(value) => format!("{key}={value}"),
            EnvAction::Preserve => format!("{key}=<preserved>"),
            EnvAction::Remove => format!("{key}=<removed>"),
          }
        })
        .collect::<Vec<_>>()
        .join(", ")
    );

    self
  }

  fn apply_env_to_exec(&self, mut cmd: Exec) -> Exec {
    for (key, action) in &self.env_vars {
      match action {
        EnvAction::Set(value) => {
          cmd = cmd.env(key, value);
        },
        EnvAction::Preserve => {
          // Only preserve if present in current environment
          if let Ok(value) = std::env::var(key) {
            cmd = cmd.env(key, value);
          }
        },
        EnvAction::Remove => {
          // For remove, we'll handle this in the sudo construction
          // by not including it in preserved variables
        },
      }
    }
    cmd
  }

  /// Creates a Exec that contains elevates the program with proper environment
  /// handling.
  ///
  /// Panics: If called when `self.elevate` is `None`
  fn build_sudo_cmd(&self) -> Result<Exec> {
    let elevation_program = self
      .elevate
      .as_ref()
      .ok_or_else(|| eyre::eyre!("Command not found for elevation"))?
      .resolve()
      .context("Failed to resolve elevation program")?;

    let mut cmd = Exec::cmd(&elevation_program);

    // Use NH_SUDO_ASKPASS program for sudo if present
    let program_name = elevation_program
      .file_name()
      .and_then(|name| name.to_str())
      .ok_or_else(|| {
        eyre::eyre!("Failed to determine elevation program name")
      })?;
    if program_name == "sudo" {
      if let Ok(askpass) = std::env::var("NH_SUDO_ASKPASS") {
        cmd = cmd.env("SUDO_ASKPASS", askpass).arg("-A");
      }
    }

    // NH_PRESERVE_ENV: set to "0" to disable preserving environment variables,
    // "1" to force, unset defaults to force
    let preserve_env = std::env::var("NH_PRESERVE_ENV")
      .as_deref()
      .map(|x| !matches!(x, "0"))
      .unwrap_or(true);

    // Insert 'env' command to explicitly pass environment variables to the
    // elevated command
    cmd = cmd.arg("env");
    for arg in self.env_vars.iter().filter_map(|(key, action)| {
      match action {
        EnvAction::Set(value) => Some(format!("{key}={value}")),
        EnvAction::Preserve if preserve_env => {
          match std::env::var(key) {
            Ok(value) => Some(format!("{key}={value}")),
            Err(_) => None,
          }
        },
        _ => None,
      }
    }) {
      cmd = cmd.arg(arg);
    }

    Ok(cmd)
  }

  /// Create a sudo command for self-elevation with proper environment handling
  ///
  /// # Errors
  ///
  /// Returns an error if the current executable path cannot be determined or
  /// sudo command cannot be built.
  pub fn self_elevate_cmd(
    strategy: ElevationStrategy,
  ) -> Result<std::process::Command> {
    // Get the current executable path
    let current_exe = std::env::current_exe()
      .context("Failed to get current executable path")?;

    // Self-elevation with proper environment handling
    let cmd_builder = Self::new(&current_exe)
      .elevate(Some(strategy))
      .with_required_env();

    let sudo_exec = cmd_builder.build_sudo_cmd()?;

    // Add the target executable and arguments to the sudo command
    let exec_with_args = sudo_exec.arg(&current_exe);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let final_exec = exec_with_args.args(&args);

    // Convert Exec to std::process::Command by parsing the command line
    let cmdline = final_exec.to_cmdline_lossy();
    let parts: Vec<&str> = cmdline.split_whitespace().collect();

    if parts.is_empty() {
      bail!("Failed to build sudo command");
    }

    let mut std_cmd = std::process::Command::new(parts[0]);
    if parts.len() > 1 {
      std_cmd.args(&parts[1..]);
    }

    Ok(std_cmd)
  }

  /// Run the configured command.
  ///
  /// # Errors
  ///
  /// Returns an error if the command fails to execute or returns a non-zero
  /// exit status.
  ///
  /// # Panics
  ///
  /// Panics if the command result is unexpectedly None.
  pub fn run(&self) -> Result<()> {
    // Prompt for sudo password if needed for remote deployment
    // FIXME: this implementation only covers Sudo. I *think* doas and run0 are
    // able to read from stdin, but needs to be tested and possibly
    // mitigated.
    let sudo_password = if self.ssh.is_some() && self.elevate.is_some() {
      let host = self.ssh.as_ref().unwrap();
      if let Some(cached_password) = get_cached_password(host) {
        Some(cached_password)
      } else {
        let password =
          inquire::Password::new(&format!("[sudo] password for {}:", host))
            .without_confirmation()
            .prompt()
            .context("Failed to read sudo password")?;
        let secret_password = SecretString::new(password.into());
        cache_password(host, secret_password.clone());
        Some(secret_password)
      }
    } else {
      None
    };

    let cmd = if self.elevate.is_some() && self.ssh.is_none() {
      // Local elevation
      self.build_sudo_cmd()?.arg(&self.command).args(&self.args)
    } else if self.elevate.is_some() && self.ssh.is_some() {
      // Build elevation command
      let elevation_program = self
        .elevate
        .as_ref()
        .unwrap()
        .resolve()
        .context("Failed to resolve elevation program")?;

      let program_name = elevation_program
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
          eyre::eyre!("Failed to determine elevation program name")
        })?;

      let mut elev_cmd = Exec::cmd(&elevation_program);

      // Add program-specific arguments
      if program_name == "sudo" {
        elev_cmd = elev_cmd.arg("--prompt=").arg("--stdin");
      }

      // Add env command to handle environment variables
      elev_cmd = elev_cmd.arg("env");
      for (key, action) in &self.env_vars {
        match action {
          EnvAction::Set(value) => {
            elev_cmd = elev_cmd.arg(format!("{}={}", key, value));
          },
          EnvAction::Preserve => {
            if let Ok(value) = std::env::var(key) {
              elev_cmd = elev_cmd.arg(format!("{}={}", key, value));
            }
          },
          _ => {},
        }
      }

      elev_cmd.arg(&self.command).args(&self.args)
    } else {
      // No elevation
      self.apply_env_to_exec(Exec::cmd(&self.command).args(&self.args))
    };

    // Configure output redirection based on show_output setting
    let cmd = ssh_wrap(
      if self.show_output {
        cmd.stderr(Redirection::Merge)
      } else {
        cmd.stderr(Redirection::None).stdout(Redirection::None)
      },
      self.ssh.as_deref(),
      sudo_password.as_ref(),
    );

    if let Some(m) = &self.message {
      info!("{m}");
    }

    debug!(?cmd);

    if self.dry {
      return Ok(());
    }

    let msg = self
      .message
      .clone()
      .unwrap_or_else(|| "Command failed".to_string());
    let res = cmd.capture();
    match res {
      Ok(capture) => {
        let status = &capture.exit_status;
        if !status.success() {
          let stderr = capture.stderr_str();
          if stderr.trim().is_empty() {
            return Err(eyre::eyre!(format!(
              "{} (exit status {:?})",
              msg, status
            )));
          }
          return Err(eyre::eyre!(format!(
            "{} (exit status {:?})\nstderr:\n{}",
            msg, status, stderr
          )));
        }
        Ok(())
      },
      Err(e) => Err(e).wrap_err(msg),
    }
  }

  /// Run the configured command and capture its output.
  ///
  /// # Errors
  ///
  /// Returns an error if the command fails to execute.
  pub fn run_capture(&self) -> Result<Option<String>> {
    let cmd = self.apply_env_to_exec(
      Exec::cmd(&self.command)
        .args(&self.args)
        .stderr(Redirection::None)
        .stdout(Redirection::Pipe),
    );

    if let Some(m) = &self.message {
      info!("{m}");
    }

    debug!(?cmd);

    if self.dry {
      return Ok(None);
    }
    Ok(Some(cmd.capture()?.stdout_str()))
  }
}

#[derive(Debug)]
pub struct Build {
  message:     Option<String>,
  installable: Installable,
  extra_args:  Vec<OsString>,
  nom:         bool,
  builder:     Option<String>,
}

impl Build {
  #[must_use]
  pub const fn new(installable: Installable) -> Self {
    Self {
      message: None,
      installable,
      extra_args: vec![],
      nom: false,
      builder: None,
    }
  }

  #[must_use]
  pub fn message<S: AsRef<str>>(mut self, message: S) -> Self {
    self.message = Some(message.as_ref().to_string());
    self
  }

  #[must_use]
  pub fn extra_arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
    self.extra_args.push(arg.as_ref().to_os_string());
    self
  }

  #[must_use]
  pub const fn nom(mut self, yes: bool) -> Self {
    self.nom = yes;
    self
  }

  #[must_use]
  pub fn builder(mut self, builder: Option<String>) -> Self {
    self.builder = builder;
    self
  }

  #[must_use]
  pub fn extra_args<I>(mut self, args: I) -> Self
  where
    I: IntoIterator,
    I::Item: AsRef<OsStr>,
  {
    for elem in args {
      self.extra_args.push(elem.as_ref().to_os_string());
    }
    self
  }

  #[must_use]
  pub fn passthrough(self, passthrough: &NixBuildPassthroughArgs) -> Self {
    self.extra_args(passthrough.generate_passthrough_args())
  }

  /// Run the build command.
  ///
  /// # Errors
  ///
  /// Returns an error if the build command fails to execute.
  pub fn run(&self) -> Result<()> {
    if let Some(m) = &self.message {
      info!("{m}");
    }

    let installable_args = self.installable.to_args();

    let base_command = Exec::cmd("nix")
      .arg("build")
      .args(&installable_args)
      .args(&match &self.builder {
        Some(host) => {
          vec!["--builders".to_string(), format!("ssh://{host} - - - 100")]
        },
        None => vec![],
      })
      .args(&self.extra_args);

    let exit = if self.nom {
      let cmd = {
        base_command
          .args(&["--log-format", "internal-json", "--verbose"])
          .stderr(Redirection::Merge)
          .stdout(Redirection::Pipe)
          | Exec::cmd("nom").args(&["--json"])
      }
      .stdout(Redirection::None);
      debug!(?cmd);
      cmd.join()
    } else {
      let cmd = base_command
        .stderr(Redirection::Merge)
        .stdout(Redirection::None);

      debug!(?cmd);
      cmd.join()
    };

    match exit? {
      ExitStatus::Exited(0) => (),
      other => bail!(ExitError(other)),
    }

    Ok(())
  }
}

#[derive(Debug, Error)]
#[error("Command exited with status {0:?}")]
pub struct ExitError(ExitStatus);

#[cfg(test)]
mod tests {
  use std::{env, ffi::OsString};

  use serial_test::serial;

  use super::*;

  // Safely manage environment variables in tests
  struct EnvGuard {
    key:      String,
    original: Option<String>,
  }

  impl EnvGuard {
    fn new(key: &str, value: &str) -> Self {
      let original = env::var(key).ok();
      unsafe {
        env::set_var(key, value);
      }
      EnvGuard {
        key: key.to_string(),
        original,
      }
    }
  }

  impl Drop for EnvGuard {
    fn drop(&mut self) {
      unsafe {
        match &self.original {
          Some(val) => env::set_var(&self.key, val),
          None => env::remove_var(&self.key),
        }
      }
    }
  }

  #[test]
  fn test_env_action_variants() {
    // Test that all EnvAction variants are correctly created
    let set_action = EnvAction::Set("test_value".to_string());
    let preserve_action = EnvAction::Preserve;
    let remove_action = EnvAction::Remove;

    match set_action {
      EnvAction::Set(val) => assert_eq!(val, "test_value"),
      _ => panic!("Expected Set variant"),
    }

    assert!(matches!(preserve_action, EnvAction::Preserve));
    assert!(matches!(remove_action, EnvAction::Remove));
  }

  #[test]
  fn test_command_new() {
    let cmd = Command::new("test-command");

    assert_eq!(cmd.command, OsString::from("test-command"));
    assert!(!cmd.dry);
    assert!(cmd.message.is_none());
    assert!(cmd.args.is_empty());
    assert!(cmd.elevate.is_none());
    assert!(cmd.ssh.is_none());
    assert!(!cmd.show_output);
    assert!(cmd.env_vars.is_empty());
  }

  #[test]
  fn test_command_builder_pattern() {
    let cmd = Command::new("test")
      .dry(true)
      .show_output(true)
      .elevate(Some(ElevationStrategy::Force("sudo")))
      .ssh(Some("host".to_string()))
      .message("test message")
      .arg("arg1")
      .args(["arg2", "arg3"]);

    assert!(cmd.dry);
    assert!(cmd.show_output);
    assert_eq!(cmd.elevate, Some(ElevationStrategy::Force("sudo")));
    assert_eq!(cmd.ssh, Some("host".to_string()));
    assert_eq!(cmd.message, Some("test message".to_string()));
    assert_eq!(cmd.args, vec![
      OsString::from("arg1"),
      OsString::from("arg2"),
      OsString::from("arg3")
    ]);
  }

  #[test]
  fn test_preserve_envs() {
    let cmd = Command::new("test").preserve_envs(["VAR1", "VAR2", "VAR3"]);

    assert_eq!(cmd.env_vars.len(), 3);
    assert!(matches!(
      cmd.env_vars.get("VAR1"),
      Some(EnvAction::Preserve)
    ));
    assert!(matches!(
      cmd.env_vars.get("VAR2"),
      Some(EnvAction::Preserve)
    ));
    assert!(matches!(
      cmd.env_vars.get("VAR3"),
      Some(EnvAction::Preserve)
    ));
  }

  #[test]
  #[serial]
  fn test_with_required_env_home_user() {
    let _home_guard = EnvGuard::new("HOME", "/test/home");
    let _user_guard = EnvGuard::new("USER", "testuser");

    let cmd = Command::new("test").with_required_env();

    // Should preserve HOME and USER as Set actions
    assert!(
      matches!(cmd.env_vars.get("HOME"), Some(EnvAction::Set(val)) if val == "/test/home")
    );
    assert!(
      matches!(cmd.env_vars.get("USER"), Some(EnvAction::Set(val)) if val == "testuser")
    );

    // Should preserve all Nix-related variables if present
    for key in [
      "PATH",
      "NIX_CONFIG",
      "NIX_PATH",
      "NIX_REMOTE",
      "NIX_SSHOPTS",
      "NIX_SSL_CERT_FILE",
      "NIX_USER_CONF_FILES",
      "LOCALE_ARCHIVE",
      "HOME_MANAGER_BACKUP_EXT",
    ] {
      if cmd.env_vars.contains_key(key) {
        assert!(matches!(cmd.env_vars.get(key), Some(EnvAction::Preserve)));
      }
    }
  }

  #[test]
  #[serial]
  fn test_with_required_env_missing_home_user() {
    // Test behavior when HOME/USER are not set
    unsafe {
      env::remove_var("HOME");
      env::remove_var("USER");
    }

    let cmd = Command::new("test").with_required_env();

    // Should not have HOME or USER in env_vars if they're not set
    assert!(!cmd.env_vars.contains_key("HOME"));
    assert!(!cmd.env_vars.contains_key("USER"));

    // Should preserve Nix-related variables if present
    for key in [
      "PATH",
      "NIX_CONFIG",
      "NIX_PATH",
      "NIX_REMOTE",
      "NIX_SSHOPTS",
      "NIX_SSL_CERT_FILE",
      "NIX_USER_CONF_FILES",
      "LOCALE_ARCHIVE",
      "HOME_MANAGER_BACKUP_EXT",
    ] {
      if let Some(action) = cmd.env_vars.get(key) {
        assert!(matches!(action, EnvAction::Preserve));
      }
    }
  }

  #[test]
  #[serial]
  fn test_with_required_env_nh_vars() {
    let _guard1 = EnvGuard::new("NH_TEST_VAR", "test_value");
    let _guard2 = EnvGuard::new("NH_ANOTHER_VAR", "another_value");
    let _guard3 = EnvGuard::new("NOT_NH_VAR", "should_not_be_included");

    let cmd = Command::new("test").with_required_env();

    // Should include NH_* variables as Set actions
    assert!(
      matches!(cmd.env_vars.get("NH_TEST_VAR"), Some(EnvAction::Set(val)) if val == "test_value")
    );
    assert!(
      matches!(cmd.env_vars.get("NH_ANOTHER_VAR"), Some(EnvAction::Set(val)) if val == "another_value")
    );

    // Should not include non-NH variables
    assert!(!cmd.env_vars.contains_key("NOT_NH_VAR"));
  }

  #[test]
  #[serial]
  fn test_combined_env_methods() {
    let _home_guard = EnvGuard::new("HOME", "/test/home");
    let _nh_guard = EnvGuard::new("NH_TEST", "nh_value");

    let cmd = Command::new("test")
      .with_required_env()
      .preserve_envs(["EXTRA_VAR"]);

    // Should have HOME from with_nix_env
    assert!(
      matches!(cmd.env_vars.get("HOME"), Some(EnvAction::Set(val)) if val == "/test/home")
    );

    // Should have NH variables from with_nh_env
    assert!(
      matches!(cmd.env_vars.get("NH_TEST"), Some(EnvAction::Set(val)) if val == "nh_value")
    );

    // Should have Nix variables preserved
    assert!(matches!(
      cmd.env_vars.get("PATH"),
      Some(EnvAction::Preserve)
    ));

    // Should have extra preserved variable
    assert!(matches!(
      cmd.env_vars.get("EXTRA_VAR"),
      Some(EnvAction::Preserve)
    ));
  }

  #[test]
  fn test_env_vars_override_behavior() {
    let mut cmd = Command::new("test");

    // First add a variable as Preserve
    cmd
      .env_vars
      .insert("TEST_VAR".to_string(), EnvAction::Preserve);
    assert!(matches!(
      cmd.env_vars.get("TEST_VAR"),
      Some(EnvAction::Preserve)
    ));

    // Then override it as Set
    cmd.env_vars.insert(
      "TEST_VAR".to_string(),
      EnvAction::Set("new_value".to_string()),
    );
    assert!(
      matches!(cmd.env_vars.get("TEST_VAR"), Some(EnvAction::Set(val)) if val == "new_value")
    );
  }

  #[test]
  fn test_build_sudo_cmd_basic() {
    let cmd =
      Command::new("test").elevate(Some(ElevationStrategy::Force("sudo")));
    let sudo_exec = cmd.build_sudo_cmd().unwrap();

    // Platform-agnostic: 'sudo' may not be the first token if env vars are
    // injected (e.g., NH_SUDO_ASKPASS). Accept any command line where
    // 'sudo' is present as a token.
    let cmdline = sudo_exec.to_cmdline_lossy();
    assert!(cmdline.split_whitespace().any(|tok| tok == "sudo"));
  }

  #[test]
  #[serial]
  fn test_build_sudo_cmd_with_preserve_vars() {
    let _preserve_env_guard = EnvGuard::new("NH_PRESERVE_ENV", "1");
    let _var1_guard = EnvGuard::new("VAR1", "1");
    let _var2_guard = EnvGuard::new("VAR2", "2");

    let cmd = Command::new("test")
      .preserve_envs(["VAR1", "VAR2"])
      .elevate(Some(ElevationStrategy::Force("sudo")));

    let sudo_exec = cmd.build_sudo_cmd().unwrap();
    let cmdline = sudo_exec.to_cmdline_lossy();

    assert!(cmdline.contains("env"));
    assert!(cmdline.contains("VAR1=1"));
    assert!(cmdline.contains("VAR2=2"));
  }

  #[test]
  #[serial]
  fn test_build_sudo_cmd_with_disabled_preserve_vars() {
    let _preserve_env_guard = EnvGuard::new("NH_PRESERVE_ENV", "0");
    let _var1_guard = EnvGuard::new("VAR1", "1");
    let _var2_guard = EnvGuard::new("VAR2", "2");

    let cmd = Command::new("test")
      .preserve_envs(["VAR1", "VAR2"])
      .elevate(Some(ElevationStrategy::Force("sudo")));

    let sudo_exec = cmd.build_sudo_cmd().unwrap();
    let cmdline = sudo_exec.to_cmdline_lossy();

    assert!(cmdline.contains("env"));
    assert!(!cmdline.contains("VAR1=1"));
    assert!(!cmdline.contains("VAR2=2"));
  }

  #[test]
  #[serial]
  fn test_build_sudo_cmd_with_set_vars() {
    let mut cmd =
      Command::new("test").elevate(Some(ElevationStrategy::Force("sudo")));
    cmd.env_vars.insert(
      "TEST_VAR".to_string(),
      EnvAction::Set("test_value".to_string()),
    );

    let sudo_exec = cmd.build_sudo_cmd().unwrap();
    let cmdline = sudo_exec.to_cmdline_lossy();

    // Should contain env command with variable
    assert!(cmdline.contains("env"));
    assert!(cmdline.contains("TEST_VAR=test_value"));
  }

  #[test]
  #[serial]
  fn test_build_sudo_cmd_with_remove_vars() {
    let _preserve_guard = EnvGuard::new("VAR_TO_PRESERVE", "preserve");
    let _remove_guard = EnvGuard::new("VAR_TO_REMOVE", "remove");

    let mut cmd =
      Command::new("test").elevate(Some(ElevationStrategy::Force("sudo")));
    cmd
      .env_vars
      .insert("VAR_TO_PRESERVE".to_string(), EnvAction::Preserve);
    cmd
      .env_vars
      .insert("VAR_TO_REMOVE".to_string(), EnvAction::Remove);

    let sudo_exec = cmd.build_sudo_cmd().unwrap();
    let cmdline = sudo_exec.to_cmdline_lossy();

    assert!(cmdline.contains("env"));
    assert!(cmdline.contains("VAR_TO_PRESERVE=preserve"));
    assert!(!cmdline.contains("VAR_TO_REMOVE"));
  }

  #[test]
  #[serial]
  fn test_build_sudo_cmd_with_askpass() {
    let _guard = EnvGuard::new("NH_SUDO_ASKPASS", "/path/to/askpass");

    let cmd =
      Command::new("test").elevate(Some(ElevationStrategy::Force("sudo")));
    let sudo_exec = cmd.build_sudo_cmd().unwrap();
    let cmdline = sudo_exec.to_cmdline_lossy();

    // Should contain -A flag for askpass
    assert!(cmdline.contains("-A"));
  }

  #[test]
  #[serial]
  fn test_build_sudo_cmd_env_added_once() {
    let _preserve_guard = EnvGuard::new("PRESERVE_VAR", "preserve");

    let mut cmd =
      Command::new("test").elevate(Some(ElevationStrategy::Force("sudo")));
    cmd.env_vars.insert(
      "TEST_VAR1".to_string(),
      EnvAction::Set("value1".to_string()),
    );
    cmd.env_vars.insert(
      "TEST_VAR2".to_string(),
      EnvAction::Set("value2".to_string()),
    );
    cmd
      .env_vars
      .insert("PRESERVE_VAR".to_string(), EnvAction::Preserve);

    let sudo_exec = cmd.build_sudo_cmd().unwrap();
    let cmdline = sudo_exec.to_cmdline_lossy();

    // Count occurrences of "env" in the command line
    let env_count = cmdline.matches(" env ").count()
      + usize::from(cmdline.starts_with("env "))
      + usize::from(cmdline.ends_with(" env"));

    // Should contain env command exactly once when there are explicit
    // environment variables
    assert_eq!(
      env_count, 1,
      "env command should appear exactly once in: {cmdline}"
    );

    // Should contain our explicit environment variables
    assert!(cmdline.contains("TEST_VAR1=value1"));
    assert!(cmdline.contains("TEST_VAR2=value2"));
    // and the preserved too
    assert!(cmdline.contains("PRESERVE_VAR=preserve"));
  }

  #[test]
  fn test_build_new() {
    let installable = Installable::Flake {
      reference: "github:user/repo".to_string(),
      attribute: vec!["package".to_string()],
    };

    let build = Build::new(installable.clone());

    assert!(build.message.is_none());
    assert_eq!(build.installable.to_args(), installable.to_args());
    assert!(build.extra_args.is_empty());
    assert!(!build.nom);
    assert!(build.builder.is_none());
  }

  #[test]
  fn test_build_builder_pattern() {
    let installable = Installable::Flake {
      reference: "github:user/repo".to_string(),
      attribute: vec!["package".to_string()],
    };

    let build = Build::new(installable)
      .message("Building package")
      .extra_arg("--verbose")
      .extra_args(["--option", "setting", "value"])
      .nom(true)
      .builder(Some("build-host".to_string()));

    assert_eq!(build.message, Some("Building package".to_string()));
    assert_eq!(build.extra_args, vec![
      OsString::from("--verbose"),
      OsString::from("--option"),
      OsString::from("setting"),
      OsString::from("value")
    ]);
    assert!(build.nom);
    assert_eq!(build.builder, Some("build-host".to_string()));
  }

  #[test]
  fn test_ssh_wrap_with_ssh() {
    let cmd = subprocess::Exec::cmd("echo").arg("hello");
    let wrapped = ssh_wrap(cmd, Some("user@host"), None);

    let cmdline = wrapped.to_cmdline_lossy();
    assert!(cmdline.starts_with("ssh"));
    assert!(cmdline.contains("-T"));
    assert!(cmdline.contains("user@host"));
  }

  #[test]
  fn test_ssh_wrap_without_ssh() {
    let cmd = subprocess::Exec::cmd("echo").arg("hello");
    let wrapped = ssh_wrap(cmd.clone(), None, None);

    // Should return the original command unchanged
    assert_eq!(wrapped.to_cmdline_lossy(), cmd.to_cmdline_lossy());
  }

  #[test]
  fn test_ssh_wrap_with_password() {
    let cmd = subprocess::Exec::cmd("echo").arg("hello");
    let password = SecretString::new("testpass".into());
    let wrapped = ssh_wrap(cmd, Some("user@host"), Some(&password));

    let cmdline = wrapped.to_cmdline_lossy();
    assert!(cmdline.starts_with("ssh"));
    assert!(cmdline.contains("-T"));
    assert!(cmdline.contains("user@host"));
  }

  #[test]
  #[serial]
  fn test_apply_env_to_exec() {
    let _guard = EnvGuard::new("EXISTING_VAR", "existing_value");

    let mut cmd = Command::new("test");
    cmd.env_vars.insert(
      "SET_VAR".to_string(),
      EnvAction::Set("set_value".to_string()),
    );
    cmd
      .env_vars
      .insert("EXISTING_VAR".to_string(), EnvAction::Preserve);
    cmd
      .env_vars
      .insert("MISSING_VAR".to_string(), EnvAction::Preserve);
    cmd
      .env_vars
      .insert("REMOVE_VAR".to_string(), EnvAction::Remove);

    let exec = subprocess::Exec::cmd("echo");
    let result = cmd.apply_env_to_exec(exec);

    // We *can't* easily test the exact environment variables set on Exec,
    // but we *can* verify the method doesn't panic and returns an Exec
    let cmdline = result.to_cmdline_lossy();
    assert!(
      cmdline.contains("echo"),
      "Command line should contain 'echo': {cmdline}"
    );
  }

  #[test]
  fn test_exit_error_display() {
    let exit_status = subprocess::ExitStatus::Exited(1);
    let error = ExitError(exit_status);

    let error_string = format!("{error}");
    assert!(error_string.contains("Command exited with status"));
    assert!(error_string.contains("Exited(1)"));
  }

  #[test]
  fn test_env_action_debug() {
    let set_action = EnvAction::Set("value".to_string());
    let preserve_action = EnvAction::Preserve;
    let remove_action = EnvAction::Remove;

    // Test that Debug is implemented (this will compile-fail if not)
    let _debug_set = format!("{set_action:?}");
    let _debug_preserve = format!("{preserve_action:?}");
    let _debug_remove = format!("{remove_action:?}");
  }

  #[test]
  fn test_env_action_clone() {
    let original = EnvAction::Set("value".to_string());
    let cloned = original.clone();

    match (original, cloned) {
      (EnvAction::Set(orig_val), EnvAction::Set(cloned_val)) => {
        assert_eq!(orig_val, cloned_val);
      },
      _ => panic!("Clone should preserve variant and value"),
    }
  }
}
