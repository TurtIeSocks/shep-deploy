# shep-deploy

[![Crates.io Version](https://img.shields.io/crates/v/shep-deploy.svg)](https://crates.io/crates/shep-deploy)
[![License](https://img.shields.io/crates/l/shep-deploy.svg)](https://github.com/TurtIeSocks/shep-deploy#license)
[![MSRV](https://img.shields.io/crates/msrv/shep-deploy.svg)](https://crates.io/crates/shep-deploy)
[![CI](https://github.com/TurtIeSocks/shep-deploy/actions/workflows/test.yml/badge.svg)](https://github.com/TurtIeSocks/shep-deploy/actions/workflows/test.yml)

A deploy dog for [shep](https://github.com/TurtIeSocks/shep).

Watches a git branch, builds a release in an isolated directory, swaps to it, reloads the sheep, and rolls back on its own if the new release does not come up.

It is an external dog, the same shape as [shep-log-rotate](https://github.com/TurtIeSocks/shep-log-rotate): an ordinary binary you adopt, talking to the daemon over the socket the CLI already uses.

## Status

Early scaffold. The crate builds and has its error type. Everything else is still to come; see the [implementation plan](docs/writing-plans/plans/2026-08-26-deploy-engine.md).

## License

MIT OR Apache-2.0, at your option.
