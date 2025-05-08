<!-- markdownlint-disable no-duplicate-headings -->

# NH Changelog

## Unreleased

### Added

- Nh now checks if the current Nix implementation has necessary experimental
  features enabled. In mainline Nix (CppNix, etc.) we check for `nix-command`
  and `flakes` being set. In Lix, we also use `repl-flake` as it is still
  provided as an experimental feature.

- Nh will now check if you are using the latest stable, or "recommended,"
  version of Nix (or Lix.) This check has been placed to make it clear we do not
  support legacy/vulnerable versions of Nix, and encourage users to update if
  they have not yet done so.

## 4.0.3

### Added

- Nh now supports specifying `NH_SUDO_ASKPASS` to pass a custom value to
  `SUDO_ASKPASS` in self-elevation. If specified, `sudo` will be called with
  `-A` and the `NH_SUDO_ASKPASS` will be `SUDO_ASKPASS` locally.

### Fixed

- Fix `--configuration` being ignored in `nh home switch`
  ([#262](https://github.com/nix-community/nh/issues/262))

## 4.0.2

### Added

- Add `--json` to `nh search`, which will return results in JSON format. Useful
  for parsing the output of `nh search` with, e.g., jq.

## 4.0.1

### Removed

- NixOS 24.05 is now marked as deprecated, and will emit an error if the search
  command attempts to use it for the channel. While the Elasticsearch backend
  still seems to support 24.05, it is deprecated in Nixpkgs and is actively
  discouraged. Please update your system at your earliest convenience.
