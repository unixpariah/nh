use std::{cmp::Ordering, env};

use color_eyre::Result;
use semver::Version;
use tracing::{debug, warn};

use crate::util::{self, NixVariant};

/// Verifies if the installed Nix version meets requirements
///
/// # Returns
///
/// * `Result<()>` - Ok if version requirements are met, error otherwise
pub fn check_nix_version() -> Result<()> {
    if env::var("NH_NO_CHECKS").is_ok() {
        return Ok(());
    }

    let version = util::get_nix_version()?;
    let nix_variant = util::get_nix_variant()?;

    // XXX: Both Nix and Lix follow semantic versioning (semver). Update the
    // versions below once latest stable for either of those packages change.
    // We *also* cannot (or rather, will not) make this check for non-nixpkgs
    // Nix variants, since there is no good baseline for what to support
    // without the understanding of stable/unstable branches. What do we check
    // for, whether upstream made an announcement? No thanks.
    // TODO: Set up a CI to automatically update those in the future.
    const MIN_LIX_VERSION: &str = "2.91.1";
    const MIN_NIX_VERSION: &str = "2.24.14";

    // Minimum supported versions. Those should generally correspond to
    // latest package versions in the stable branch.
    //
    // Q: Why are you doing this?
    // A: First of all to make sure we do not make baseless assumptions
    // about the user's system; we should only work around APIs that we
    // are fully aware of, and not try to work around every edge case.
    // Also, nh should be responsible for nudging the user to use the
    // relevant versions of the software it wraps, so that we do not have
    // to try and support too many versions. NixOS stable and unstable
    // will ALWAYS be supported, but outdated versions will not. If your
    // Nix fork uses a different versioning scheme, please open an issue.
    let min_version = match nix_variant {
        util::NixVariant::Lix => MIN_LIX_VERSION,
        _ => MIN_NIX_VERSION,
    };

    let current = Version::parse(&version)?;
    let required = Version::parse(min_version)?;

    match current.cmp(&required) {
        Ordering::Less => {
            let binary_name = match nix_variant {
                util::NixVariant::Lix => "Lix",
                util::NixVariant::Determinate => "Determinate Nix",
                util::NixVariant::Nix => "Nix",
            };
            warn!(
                "Warning: {} version {} is older than the recommended minimum version {}. You may encounter issues.",
                binary_name, version, min_version
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Handles environment variable setup and returns if a warning should be shown
///
/// # Returns
///
/// * `Result<bool>` - True if a warning should be shown about the FLAKE
///   variable, false otherwise
pub fn setup_environment() -> Result<bool> {
    let mut do_warn = false;

    if let Ok(f) = std::env::var("FLAKE") {
        // Set NH_FLAKE if it's not already set
        if std::env::var("NH_FLAKE").is_err() {
            unsafe {
                std::env::set_var("NH_FLAKE", f);
            }

            // Only warn if FLAKE is set and we're using it to set NH_FLAKE
            // AND none of the command-specific env vars are set
            if std::env::var("NH_OS_FLAKE").is_err()
                && std::env::var("NH_HOME_FLAKE").is_err()
                && std::env::var("NH_DARWIN_FLAKE").is_err()
            {
                do_warn = true;
            }
        }
    }

    Ok(do_warn)
}

/// Consolidate all necessary checks for Nix functionality into a single
/// function. This will be executed in the main function, but can be executed
/// before critical commands to double-check if necessary.
///
/// NOTE: Experimental feature checks are now done per-command to avoid
/// redundant error messages for features not needed by the specific command.
///
/// # Returns
///
/// * `Result<()>` - Ok if all checks pass, error otherwise
pub fn verify_nix_environment() -> Result<()> {
    if env::var("NH_NO_CHECKS").is_ok() {
        return Ok(());
    }

    // Only check version globally. Features are checked per-command now.
    // This function is kept as is for backwards compatibility.
    check_nix_version()?;
    Ok(())
}

/// Trait for types that have feature requirements
pub trait FeatureRequirements {
    /// Returns the list of required experimental features
    fn required_features(&self) -> Vec<&'static str>;

    /// Checks if all required features are enabled
    fn check_features(&self) -> Result<()> {
        if env::var("NH_NO_CHECKS").is_ok() {
            return Ok(());
        }

        let required = self.required_features();
        if required.is_empty() {
            return Ok(());
        }

        debug!("Required Nix features: {}", required.join(", "));

        let missing = util::get_missing_experimental_features(&required)?;
        if !missing.is_empty() {
            return Err(color_eyre::eyre::eyre!(
                "Missing required experimental features for this command: {}",
                missing.join(", ")
            ));
        }

        debug!("All required Nix features are enabled");
        Ok(())
    }
}

/// Feature requirements for commands that use flakes
#[derive(Debug)]
pub struct FlakeFeatures;

impl FeatureRequirements for FlakeFeatures {
    fn required_features(&self) -> Vec<&'static str> {
        let mut features = vec![];

        // Determinate Nix doesn't require nix-command or flakes to be experimental
        // as they simply decided to mark those as no-longer-experimental-lol. Remove
        // redundant experimental features if the Nix variant is determinate.
        if let Ok(variant) = util::get_nix_variant() {
            if !matches!(variant, NixVariant::Determinate) {
                features.push("nix-command");
                features.push("flakes");
            }
        }

        features
    }
}

/// Feature requirements for legacy (non-flake) commands
/// XXX: There are actually no experimental feature requirements for legacy (nix2) CLI
/// but since move-fast-break-everything is a common mantra among Nix & Nix-adjecent
/// software, I've implemented this. Do not remove, this is simply for futureproofing.
#[derive(Debug)]
pub struct LegacyFeatures;

impl FeatureRequirements for LegacyFeatures {
    fn required_features(&self) -> Vec<&'static str> {
        vec![]
    }
}

/// Feature requirements for OS repl commands
#[derive(Debug)]
pub struct OsReplFeatures {
    pub is_flake: bool,
}

impl FeatureRequirements for OsReplFeatures {
    fn required_features(&self) -> Vec<&'static str> {
        let mut features = vec![];

        // For non-flake repls, no experimental features needed
        if !self.is_flake {
            return features;
        }

        // For flake repls, check if we need experimental features
        if let Ok(variant) = util::get_nix_variant() {
            match variant {
                NixVariant::Determinate => {
                    // Determinate Nix doesn't need experimental features
                }
                NixVariant::Lix => {
                    features.push("nix-command");
                    features.push("flakes");

                    // Lix-specific repl-flake feature for older versions
                    if let Ok(version) = util::get_nix_version() {
                        if let Ok(current) = Version::parse(&version) {
                            if let Ok(threshold) = Version::parse("2.93.0") {
                                if current < threshold {
                                    features.push("repl-flake");
                                }
                            }
                        }
                    }
                }
                NixVariant::Nix => {
                    features.push("nix-command");
                    features.push("flakes");
                }
            }
        }

        features
    }
}

/// Feature requirements for Home Manager repl commands
#[derive(Debug)]
pub struct HomeReplFeatures {
    pub is_flake: bool,
}

impl FeatureRequirements for HomeReplFeatures {
    fn required_features(&self) -> Vec<&'static str> {
        let mut features = vec![];

        // For non-flake repls, no experimental features needed
        if !self.is_flake {
            return features;
        }

        // For flake repls, only need nix-command and flakes
        if let Ok(variant) = util::get_nix_variant() {
            if !matches!(variant, NixVariant::Determinate) {
                features.push("nix-command");
                features.push("flakes");
            }
        }

        features
    }
}

/// Feature requirements for Darwin repl commands
#[derive(Debug)]
pub struct DarwinReplFeatures {
    pub is_flake: bool,
}

impl FeatureRequirements for DarwinReplFeatures {
    fn required_features(&self) -> Vec<&'static str> {
        let mut features = vec![];

        // For non-flake repls, no experimental features needed
        if !self.is_flake {
            return features;
        }

        // For flake repls, only need nix-command and flakes
        if let Ok(variant) = util::get_nix_variant() {
            if !matches!(variant, NixVariant::Determinate) {
                features.push("nix-command");
                features.push("flakes");
            }
        }

        features
    }
}

/// Feature requirements for commands that don't need experimental features
#[derive(Debug)]
pub struct NoFeatures;

impl FeatureRequirements for NoFeatures {
    fn required_features(&self) -> Vec<&'static str> {
        vec![]
    }
}
