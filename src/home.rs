use std::{env, ffi::OsString, path::PathBuf};

use color_eyre::{
  Result,
  eyre::{Context, bail, eyre},
};
use tracing::{debug, info, warn};

use crate::{
  commands,
  commands::Command,
  installable::Installable,
  interface::{
    self,
    DiffType,
    HomeRebuildArgs,
    HomeReplArgs,
    HomeRollbackArgs,
    HomeSubcommand,
  },
  update::update,
  util::{get_hostname, print_dix_diff},
};

// Use HOME env var
const HOME_PROFILE: &str = "~/.local/state/nix/profiles/home-manager";
const CURRENT_PROFILE: &str =
  "~/.local/state/home-manager/gcroots/current-home";

const SPEC_LOCATION: &str = "~/.local/share/home-manager/specialisation";

impl interface::HomeArgs {
  /// Run the `home` subcommand.
  ///
  /// # Errors
  ///
  /// Returns an error if the operation fails.
  pub fn run(self) -> Result<()> {
    use HomeRebuildVariant::{Build, Switch};
    match self.subcommand {
      HomeSubcommand::Switch(args) => args.rebuild(&Switch),
      HomeSubcommand::Build(args) => {
        if args.common.ask || args.common.dry {
          warn!("`--ask` and `--dry` have no effect for `nh home build`");
        }
        args.rebuild(&Build)
      },
      HomeSubcommand::Repl(args) => args.run(),
      HomeSubcommand::Rollback(args) => args.rollback(),
    }
  }
}

#[derive(Debug)]
enum HomeRebuildVariant {
  Build,
  Switch,
}

impl HomeRebuildArgs {
  fn rebuild(self, variant: &HomeRebuildVariant) -> Result<()> {
    use HomeRebuildVariant::Build;

    if self.update_args.update_all || self.update_args.update_input.is_some() {
      update(&self.common.installable, self.update_args.update_input)?;
    }

    let (out_path, _tempdir_guard): (PathBuf, Option<tempfile::TempDir>) =
      if let Some(ref p) = self.common.out_link {
        (p.clone(), None)
      } else {
        let dir = tempfile::Builder::new().prefix("nh-home").tempdir()?;
        (dir.as_ref().join("result"), Some(dir))
      };

    debug!("Output path: {out_path:?}");

    // Use NH_HOME_FLAKE if available, otherwise use the provided installable
    let installable = if let Ok(home_flake) = env::var("NH_HOME_FLAKE") {
      debug!("Using NH_HOME_FLAKE: {}", home_flake);

      let mut elems = home_flake.splitn(2, '#');
      let reference = match elems.next() {
        Some(r) => r.to_owned(),
        None => return Err(eyre!("NH_HOME_FLAKE missing reference part")),
      };
      let attribute = elems
        .next()
        .map(crate::installable::parse_attribute)
        .unwrap_or_default();

      Installable::Flake {
        reference,
        attribute,
      }
    } else {
      self.common.installable.clone()
    };

    let toplevel = toplevel_for(
      installable,
      true,
      &self.extra_args,
      self.configuration.clone(),
    )?;

    commands::Build::new(toplevel)
      .extra_arg("--out-link")
      .extra_arg(&out_path)
      .extra_args(&self.extra_args)
      .passthrough(&self.common.passthrough)
      .message("Building Home-Manager configuration")
      .nom(!self.common.no_nom)
      .run()
      .wrap_err("Failed to build Home-Manager configuration")?;

    let prev_generation: Option<PathBuf> = [
      PathBuf::from("/nix/var/nix/profiles/per-user")
        .join(env::var("USER").map_err(|_| eyre!("Couldn't get username"))?)
        .join("home-manager"),
      PathBuf::from(
        env::var("HOME").map_err(|_| eyre!("Couldn't get home directory"))?,
      )
      .join(".local/state/nix/profiles/home-manager"),
    ]
    .into_iter()
    .find(|next| next.exists());

    debug!("Previous generation: {prev_generation:?}");

    let spec_location = PathBuf::from(std::env::var("HOME")?)
      .join(".local/share/home-manager/specialisation");

    let current_specialisation = if let Some(s) = spec_location.to_str() {
      std::fs::read_to_string(s).ok()
    } else {
      tracing::warn!("spec_location path is not valid UTF-8");
      None
    };

    let target_specialisation = if self.no_specialisation {
      None
    } else {
      current_specialisation.or(self.specialisation)
    };

    debug!("target_specialisation: {target_specialisation:?}");

    let target_profile: PathBuf = if let Some(spec) = &target_specialisation {
      out_path.join("specialisation").join(spec)
    } else {
      out_path.clone()
    };

    // just do nothing for None case (fresh installs)
    if let Some(generation) = prev_generation {
      match self.common.diff {
        DiffType::Never => {
          debug!("Not running dix as the --diff flag is set to never.");
        },
        _ => {
          let _ = print_dix_diff(&generation, &target_profile);
        },
      }
    }

    if self.common.dry || matches!(variant, Build) {
      if self.common.ask {
        warn!("--ask has no effect as dry run was requested");
      }
      return Ok(());
    }

    if self.common.ask {
      let confirmation = inquire::Confirm::new("Apply the config?")
        .with_default(false)
        .prompt()?;

      if !confirmation {
        bail!("User rejected the new config");
      }
    }

    if let Some(ext) = &self.backup_extension {
      info!("Using {} as the backup extension", ext);
      unsafe {
        env::set_var("HOME_MANAGER_BACKUP_EXT", ext);
      }
    }

    Command::new(target_profile.join("activate"))
      .with_required_env()
      .message("Activating configuration")
      .run()
      .wrap_err("Activation failed")?;

    debug!("Completed operation with output path: {target_profile:?}");

    Ok(())
  }
}

impl HomeRollbackArgs {
  fn rollback(&self) -> Result<()> {
    // Find previous generation or specific generation
    let target_generation = if let Some(gen_number) = self.to {
      find_generation_by_number(gen_number)?
    } else {
      find_previous_generation()?
    };

    info!("Rolling back to generation {}", target_generation.number);

    // Construct path to the generation
    let profile_dir = Path::new(HOME_PROFILE).parent().unwrap_or_else(|| {
      tracing::warn!(
        "SYSTEM_PROFILE has no parent, defaulting to /nix/var/nix/profiles"
      );
      Path::new("/nix/var/nix/profiles")
    });
    let generation_link =
      profile_dir.join(format!("system-{}-link", target_generation.number));

    // Handle specialisations
    let current_specialisation = fs::read_to_string(SPEC_LOCATION).ok();

    let target_specialisation = if self.no_specialisation {
      None
    } else {
      self.specialisation.clone().or(current_specialisation)
    };

    debug!("target_specialisation: {target_specialisation:?}");

    // Compare changes between current and target generation
    if matches!(self.diff, DiffType::Never) {
      debug!(
        "Not running dix as the target hostname is different from the system \
         hostname."
      );
    } else {
      debug!(
        "Comparing with target profile: {}",
        generation_link.display()
      );
      let _ = print_dix_diff(&PathBuf::from(CURRENT_PROFILE), &generation_link);
    }

    if self.dry {
      info!(
        "Dry run: would roll back to generation {}",
        target_generation.number
      );
      return Ok(());
    }

    if self.ask {
      let confirmation = inquire::Confirm::new(&format!(
        "Roll back to generation {}?",
        target_generation.number
      ))
      .with_default(false)
      .prompt()?;

      if !confirmation {
        bail!("User rejected the rollback");
      }
    }

    // Get current generation number for potential rollback
    let current_gen_number = match get_current_generation_number() {
      Ok(num) => num,
      Err(e) => {
        warn!("Failed to get current generation number: {}", e);
        0
      },
    };

    // Set the system profile
    info!("Setting system profile...");

    // Instead of direct symlink operations, use a command
    Command::new("ln")
            .arg("-sfn") // force, symbolic link
            .arg(&generation_link)
            .arg(HOME_PROFILE)
            .message("Setting home profile")
            .with_required_env()
            .run()
            .wrap_err("Failed to set home profile during rollback")?;

    // Determine the correct profile to use with specialisations
    let final_profile = match &target_specialisation {
      None => generation_link,
      Some(spec) => {
        let spec_path = generation_link.join("specialisation").join(spec);
        if spec_path.exists() {
          spec_path
        } else {
          warn!(
            "Specialisation '{}' does not exist in generation {}",
            spec, target_generation.number
          );
          warn!("Using base configuration without specialisations");
          generation_link
        }
      },
    };

    // Activate the configuration
    info!("Activating...");

    let switch_to_configuration =
      final_profile.join("bin").join("switch-to-configuration");

    if !switch_to_configuration.exists() {
      return Err(eyre!(
        "The 'switch-to-configuration' binary is missing from the built \
         configuration.\n\nThis typically happens when 'system.switch.enable' \
         is set to false in your\nNixOS configuration. To fix this, please \
         either:\n1. Remove 'system.switch.enable = false' from your \
         configuration, or\n2. Set 'system.switch.enable = true' \
         explicitly\n\nIf the problem persists, please open an issue on our \
         issue tracker!"
      ));
    }

    match Command::new(&switch_to_configuration)
      .arg("switch")
      .elevate(elevate.then_some(elevation.clone()))
      .preserve_envs(["NIXOS_INSTALL_BOOTLOADER"])
      .with_required_env()
      .run()
    {
      Ok(()) => {
        info!(
          "Successfully rolled back to generation {}",
          target_generation.number
        );
      },
      Err(e) => {
        // If activation fails, rollback the profile
        if current_gen_number > 0 {
          let current_gen_link =
            profile_dir.join(format!("system-{current_gen_number}-link"));

          Command::new("ln")
                        .arg("-sfn") // Force, symbolic link
                        .arg(&current_gen_link)
                        .arg(HOME_PROFILE)
                        .elevate(elevate.then_some(elevation))
                        .message("Rolling back system profile")
                        .with_required_env()
                        .run()
                        .wrap_err("NixOS: Failed to restore previous system profile after failed activation")?;
        }

        return Err(eyre!("Activation (switch) failed: {}", e))
          .context("Failed to activate configuration");
      },
    }

    Ok(())
  }
}

fn find_previous_generation() -> Result<generations::GenerationInfo> {
  let profile_path = PathBuf::from(HOME_PROFILE);

  let mut generations: Vec<generations::GenerationInfo> = fs::read_dir(
    profile_path
      .parent()
      .unwrap_or(Path::new("/nix/var/nix/profiles")),
  )?
  .filter_map(|entry| {
    entry.ok().and_then(|e| {
      let path = e.path();
      if let Some(filename) = path.file_name() {
        if let Some(name) = filename.to_str() {
          if name.starts_with("system-") && name.ends_with("-link") {
            return generations::describe(&path);
          }
        }
      }
      None
    })
  })
  .collect();

  if generations.is_empty() {
    bail!("No generations found");
  }

  generations.sort_by(|a, b| {
    a.number
      .parse::<u64>()
      .unwrap_or(0)
      .cmp(&b.number.parse::<u64>().unwrap_or(0))
  });

  let current_idx = generations
    .iter()
    .position(|g| g.current)
    .ok_or_else(|| eyre!("Current generation not found"))?;

  if current_idx == 0 {
    bail!("No generation older than the current one exists");
  }

  Ok(generations[current_idx - 1].clone())
}

fn find_generation_by_number(
  number: u64,
) -> Result<generations::GenerationInfo> {
  let profile_path = PathBuf::from(HOME_PROFILE);

  let generations: Vec<generations::GenerationInfo> = fs::read_dir(
    profile_path
      .parent()
      .unwrap_or(Path::new("/nix/var/nix/profiles")),
  )?
  .filter_map(|entry| {
    entry.ok().and_then(|e| {
      let path = e.path();
      if let Some(filename) = path.file_name() {
        if let Some(name) = filename.to_str() {
          if name.starts_with("system-") && name.ends_with("-link") {
            return generations::describe(&path);
          }
        }
      }
      None
    })
  })
  .filter(|generation| generation.number == number.to_string())
  .collect();

  if generations.is_empty() {
    bail!("Generation {} not found", number);
  }

  Ok(generations[0].clone())
}

fn get_current_generation_number() -> Result<u64> {
  let profile_path = PathBuf::from(HOME_PROFILE);

  let generations: Vec<generations::GenerationInfo> = fs::read_dir(
    profile_path
      .parent()
      .unwrap_or(Path::new("/nix/var/nix/profiles")),
  )?
  .filter_map(|entry| entry.ok().and_then(|e| generations::describe(&e.path())))
  .collect();

  let current_gen = generations
    .iter()
    .find(|g| g.current)
    .ok_or_else(|| eyre!("Current generation not found"))?;

  current_gen
    .number
    .parse::<u64>()
    .wrap_err("Invalid generation number")
}

fn toplevel_for<I, S>(
  installable: Installable,
  push_drv: bool,
  extra_args: I,
  configuration_name: Option<String>,
) -> Result<Installable>
where
  I: IntoIterator<Item = S>,
  S: AsRef<std::ffi::OsStr>,
{
  let mut res = installable;
  let extra_args: Vec<OsString> = {
    let mut vec = Vec::new();
    for elem in extra_args {
      vec.push(elem.as_ref().to_owned());
    }
    vec
  };

  let toplevel = ["config", "home", "activationPackage"]
    .into_iter()
    .map(String::from);

  match res {
    Installable::Flake {
      ref reference,
      ref mut attribute,
    } => {
      // If user explicitly selects some other attribute in the installable
      // itself then don't push homeConfigurations
      if !attribute.is_empty() {
        debug!(
          "Using explicit attribute path from installable: {:?}",
          attribute
        );
        return Ok(res);
      }

      attribute.push(String::from("homeConfigurations"));

      let flake_reference = reference.clone();
      let mut found_config = false;

      // Check if an explicit configuration name was provided via the flag
      if let Some(config_name) = configuration_name {
        // Verify the provided configuration exists
        let func = format!(r#" x: x ? "{config_name}" "#);
        let check_res = commands::Command::new("nix")
          .with_required_env()
          .arg("eval")
          .args(&extra_args)
          .arg("--apply")
          .arg(func)
          .args(
            (Installable::Flake {
              reference: flake_reference.clone(),
              attribute: attribute.clone(),
            })
            .to_args(),
          )
          .run_capture()
          .wrap_err(format!(
            "Failed running nix eval to check for explicit configuration \
             '{config_name}'"
          ))?;

        if check_res.map(|s| s.trim().to_owned()).as_deref() == Some("true") {
          debug!("Using explicit configuration from flag: {config_name:?}");

          attribute.push(config_name);
          if push_drv {
            attribute.extend(toplevel.clone());
          }

          found_config = true;
        } else {
          // Explicit config provided but not found
          let tried_attr_path = {
            let mut attr_path = attribute.clone();
            attr_path.push(config_name);
            Installable::Flake {
              reference: flake_reference,
              attribute: attr_path,
            }
            .to_args()
            .join(" ")
          };
          bail!(
            "Explicitly specified home-manager configuration not found: \
             {tried_attr_path}"
          );
        }
      }

      // If no explicit config was found via flag, try automatic detection
      if !found_config {
        let username =
          std::env::var("USER").map_err(|_| eyre!("Couldn't get username"))?;
        let hostname = get_hostname()?;
        let mut tried = vec![];

        for attr_name in [format!("{username}@{hostname}"), username] {
          let func = format!(r#" x: x ? "{attr_name}" "#);
          let check_res = commands::Command::new("nix")
            .with_required_env()
            .arg("eval")
            .args(&extra_args)
            .arg("--apply")
            .arg(func)
            .args(
              (Installable::Flake {
                reference: flake_reference.clone(),
                attribute: attribute.clone(),
              })
              .to_args(),
            )
            .run_capture()
            .wrap_err(format!(
              "Failed running nix eval to check for automatic configuration \
               '{attr_name}'"
            ))?;

          let current_try_attr = {
            let mut attr_path = attribute.clone();
            attr_path.push(attr_name.clone());
            attr_path
          };
          tried.push(current_try_attr.clone());

          if let Some("true") =
            check_res.map(|s| s.trim().to_owned()).as_deref()
          {
            debug!("Using automatically detected configuration: {}", attr_name);
            attribute.push(attr_name);
            if push_drv {
              attribute.extend(toplevel.clone());
            }
            found_config = true;
            break;
          }
        }

        // If still not found after automatic detection, error out
        if !found_config {
          let tried_str = tried
            .into_iter()
            .map(|a| {
              Installable::Flake {
                reference: flake_reference.clone(),
                attribute: a,
              }
              .to_args()
              .join(" ")
            })
            .collect::<Vec<_>>()
            .join(", ");
          bail!(
            "Couldn't find home-manager configuration automatically, tried: \
             {tried_str}"
          );
        }
      }
    },
    Installable::File {
      ref mut attribute, ..
    } => {
      if push_drv {
        attribute.extend(toplevel);
      }
    },
    Installable::Expression {
      ref mut attribute, ..
    } => {
      if push_drv {
        attribute.extend(toplevel);
      }
    },
    Installable::Store { .. } => {},
  }

  Ok(res)
}

impl HomeReplArgs {
  fn run(self) -> Result<()> {
    // Use NH_HOME_FLAKE if available, otherwise use the provided installable
    let installable = if let Ok(home_flake) = env::var("NH_HOME_FLAKE") {
      debug!("Using NH_HOME_FLAKE: {home_flake}");

      let mut elems = home_flake.splitn(2, '#');
      let reference = match elems.next() {
        Some(r) => r.to_owned(),
        None => return Err(eyre!("NH_HOME_FLAKE missing reference part")),
      };
      let attribute = elems
        .next()
        .map(crate::installable::parse_attribute)
        .unwrap_or_default();

      Installable::Flake {
        reference,
        attribute,
      }
    } else {
      self.installable
    };

    let toplevel = toplevel_for(
      installable,
      false,
      &self.extra_args,
      self.configuration.clone(),
    )?;

    Command::new("nix")
      .with_required_env()
      .arg("repl")
      .args(toplevel.to_args())
      .show_output(true)
      .run()?;

    Ok(())
  }
}
