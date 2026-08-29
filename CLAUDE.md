# shep-deploy — CLAUDE.md

A deploy dog for [shep](https://github.com/TurtIeSocks/shep). Watches a git
branch, builds a release in an isolated worktree, swaps a `current` symlink
with `rename(2)`, reloads the sheep, and rolls back if the new release does not
come up. Unix only, deliberately. MIT OR Apache-2.0.

Consumes `shep-core` and `shep-client` as **published crates**, never as a
path dependency. That is the point of the project, not an accident: it exists
partly to find out whether somebody outside shep's workspace can build a dog
from what shep publishes, and a path dependency would paper over anything
missing.

## Commands

### `cargo test --all-features` FAILS on a fresh clone, and that is correct

The integration tier needs a real shep binary and panics without one:

```text
the integration tier needs $SHEP_BIN pointing at a built shep binary,
for example SHEP_BIN="$(command -v shep)": NotPresent
```

So the local loop is one of these, not the shep-workspace habit of
`--all-features`:

```bash
cargo test
```

```bash
SHEP_BIN="$(command -v shep)" cargo test --all-features
```

**And `$SHEP_BIN` has a version floor, which a green local run will not tell
you about.** The integration tier asserts behaviour shep only has from 0.1.10:
a probed app reloads serially, and a replacement that never becomes ready is
left `starting` rather than `online`. Point `SHEP_BIN` at an older shep and
`a_release_that_cannot_come_up_is_rolled_back_and_the_old_release_serves`
passes for the wrong reason. That happened on 2026-08-28: the local tier was
green against an installed 0.1.8 while CI, which installs the current release,
failed. Check `shep --version` before trusting a green integration run, or
point `SHEP_BIN` at a build of shep's `main`.

CI already does it right: `cargo test --locked` for the units, then a separate
job that runs `cargo install shep --locked` and then
`cargo test --features integration`. That installs the PUBLISHED shep from
crates.io, not a git checkout, and the workflow says so in its own comment: a
red integration job is a signal against shep's released surface rather than
against whatever `main` looks like that day. This file claimed "from the tip
of `main`" until round 8 of the founder's review read the workflow. Only a human running the obvious command
hits this. Cost me a wrong "baseline is green" call on 2026-08-28.

### The gate

Each from its own command with `$?` read directly, never through a pipe. In zsh
a pipeline's `$?` is the last command's, so `cargo test ... | tail` reports
`tail`'s success while the suite is red. That exact mistake produced four
`EXIT=0` lines over a failing suite on 2026-08-28.

```bash
cargo fmt --all --check
```

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

```bash
SHEP_BIN="$(command -v shep)" cargo test --all-features
```

```bash
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
```

269 unit tests and 7 integration as of 2026-08-28, ~20s and ~31s. The number moves with every task; treat it as a shape, not a checksum.

## Architecture

Binary crate, no library target. Twenty modules under `src/`, the big three
being `deploy.rs` (the state machine), `optin.rs` (first cutover) and `poll.rs`
(the dog's loop).

`#[cfg(test)] mod fixtures` is declared from `main.rs`, which is the only way a
binary crate with no lib target can share test helpers. Use
`fixtures::run_git`, `fixtures::head_of`, `fixtures::fixture_release`,
`fixtures::TEST_BUDGET`. Qualified rather than glob-imported on purpose:
`shared.rs` has a production `run_git` and `deploy.rs` a production `head_of`,
both in scope in their own test modules.

`tests/integration.rs` is a separate compilation target and keeps its own copy
of those helpers. That is the cost of the boundary, not an oversight.

## The security surface, which is the whole point of reviewing this crate

A deploy runs a build command **from the repository being deployed**. Anyone who
can land a commit on the tracked branch chooses that command. Three bounds
exist and each has a precise edge:

- **uid/gid drop** (`build.rs`), applied to the build's `Command`, so it bounds
  the CHILD. Anything the parent does afterwards on paths the child chose is
  outside it. A path-traversal in `build.artifacts` exploited exactly that gap
  on 2026-08-28 and reached the tree's own `deploy.toml`.
- **cleared environment** (`build.rs`), so a build gets `BASE_ENV` plus the
  operator's `passthrough` plus the release's `[build] env`, and nothing else.
  `SSH_AUTH_SOCK` is excluded on purpose.
- **`user` on the app**, which is the only bound that also covers what the
  build can READ. It does not cover the project's own `.env`: gitignored files
  are symlinked into every release by design, so a build reads whatever the app
  reads.

`docs/specs/deferred.md` records Landlock as the thing that would close the
class rather than the instances.

## Gotchas

- **`deploy.toml` is hand-edited by operators.** `deploy.rs` tells them to type
  `verify = "alive"` into it. It denies unknown fields for that reason; keep it
  that way.
- **`Error::Config` is built at 35 sites across ten modules.** It is not the
  `[dog.deploy]` variant its doc used to claim.
- **`retention` keeps `retention + 1` directories.** The live release is spared
  in addition to the newest N, deliberately. Rin's call, 2026-08-28: document
  it rather than change it.
- **A regression test for the artifact escape is worthless without
  `CARGO_TARGET_DIR` set.** Without it, source and destination collapse to the
  same path and the self-copy guard returns `Ok` before reaching anything the
  test is about. The test says so in its own doc; do not simplify it.
- **`git::fetch` is the only git call that touches a network**, and the only
  one using `run_git_within`. The other ten `run_git` callers are local.

## Style

Follows shep's `shep-idiomatic-rust` skill and `docs/idiomatic-rust.md`
(IR-1..IR-46). `#![forbid(unsafe_code)]` at `main.rs:33`, which is why
`build.rs` resolves users by shelling out to `id` rather than calling
`getpwnam`.
