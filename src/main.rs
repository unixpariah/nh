mod checks;
mod clean;
mod commands;
mod completion;
mod darwin;
mod generations;
mod home;
mod installable;
mod interface;
mod json;
mod logging;
mod nixos;
mod notify;
mod search;
mod update;
mod util;

use color_eyre::Result;

use crate::commands::ElevationStrategy;

pub const NH_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const NH_REV: Option<&str> = option_env!("NH_REV");

fn main() -> Result<()> {
  let args = <crate::interface::Main as clap::Parser>::parse();

  // Set up logging
  crate::logging::setup_logging(args.verbosity)?;
  tracing::debug!("{args:#?}");
  tracing::debug!(%NH_VERSION, ?NH_REV);

  // Check Nix version upfront
  checks::verify_nix_environment()?;

  // Once we assert required Nix features, validate NH environment checks
  // For now, this is just NH_* variables being set. More checks may be
  // added to setup_environment in the future.
  checks::verify_variables()?;

  let elevation = args
    .elevation_program
    .map_or(ElevationStrategy::Auto, ElevationStrategy::Prefer);

  args.command.run(elevation)
}
