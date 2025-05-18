#! /usr/bin/env bash
set -eux

echo "Running 'cargo fix' on the codebase"
cargo fix --allow-dirty

echo "Running clippy linter and applying available fixes"
cargo clippy --fix --allow-dirty --\
	-W clippy::pedantic \
	-W clippy::correctness \
	-W clippy::suspicious

echo "Running formatter"
cargo fmt
