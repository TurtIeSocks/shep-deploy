# shep-deploy — CLAUDE.md

A deploy dog for [shep](https://github.com/shep-pm/shep). Watches a git
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
failed.

The floor moves with `shep-client`, too, and in both directions. The lockfile
pins shep-client 0.2.0, which speaks protocol 3, so a shep older than 0.2.0
fails every integration test at connect with `protocol mismatch (this client
speaks 3)`. The other direction bit on 2026-09-04: the lockfile spoke 2, shep
0.2.0 shipped speaking 3 within the hour, and CI, which installs the current
release, failed all seven tests at connect while the local tier was green
against a scratch 0.1.31. When that happens the fix is the dependency, not
the tests: bump `shep-client` in Cargo.toml and `cargo update -p
shep-client`. Note that `cargo info shep-client` reports the newest
semver-compatible version as "latest", so a 0.1.x lockfile never sees a
0.2.0; ask for it by name, `cargo info shep-client@0.2.0`. Check
`shep --version` before trusting either a green or a red integration run. To
test against the current release without touching the machine's own shep,
install one into a scratch root and point `SHEP_BIN` at it:

```bash
cargo install shep --locked --force --root /tmp/shep-root
```

```bash
SHEP_BIN=/tmp/shep-root/bin/shep cargo test --features integration
```

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

373 unit tests and 7 integration as of 2026-09-04, ~22s and ~31s. The number moves with every task; treat it as a shape, not a checksum.

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
- **The Flockfile read** (`flockfile.rs`) runs in the dog's own process, at its
  own uid, BEFORE any build drops to `user`. A committed `Flockfile.toml` that
  is a symlink is therefore refused unread (`O_NOFOLLOW`), and a parse error
  names the line number and never quotes the line: a link at
  `/root/.ssh/id_rsa` used to have the key's first line printed to the log
  through the parser's own message. Found 2026-09-03. The operator's override
  is the one file that IS followed, because `shared::link_into` made it a link.

`docs/specs/deferred.md` records Landlock as the thing that would close the
class rather than the instances.

## Gotchas

- **`deploy.toml` is hand-edited by operators.** `deploy.rs` tells them to type
  `verify = "alive"` into it. It denies unknown fields for that reason; keep it
  that way.
- **`Error::Config` is built at dozens of sites across ten modules.** It is
  not the dog-section variant its doc used to claim.
- **The dog's section is `[deploy]` in `$SHEP_HOME/dogs.toml`**, since shep
  0.1.32; before that it was `[dog.deploy]` in `shep.toml`, and a shepherd
  migrates the old spelling only at boot. A test that writes the section
  after the shepherd is up has to write the new file, or the dog runs at its
  default interval. Cost the integration job on 2026-09-04.
- **`retention` keeps the newest N, and spares two more by name.** The release
  `current` points at and the one `deploy.toml` names are never removed,
  whatever their age. In the ordinary case the live release is the newest, so
  a target holds N directories; only after a deploy died between its swap and
  its record write can it hold up to N + 2. README.md and the config doc said
  "N besides the live one" until 2026-09-03; the code never did that.
- **`State::read` validates values, not only shape.** A sha has to be 40 (or
  64) hex characters, `remote` and `branch` cannot be empty or begin with `-`,
  `checkout` has to be absolute, and `origin_cwd`/`origin_script` come as a
  pair. A test fixture that writes a record with `deployed = "a1b2c3d"` is
  refused on read; use a full sha.
- **`optin::Prepared` carries the tree's `flock`.** It lives from `prepare`
  through `cut_over`. A test that calls `prepare` twice on one tree has to
  drop the first `Prepared` first, or the second is refused as
  `AlreadyDeploying`.
- **Never derive the markers directory from `Tree::completion("")`.** The
  result has a trailing separator and its `parent()` is the tree root; a sweep
  written that way removed `current`. `Tree::completions()` is the directory.
- **`io::ErrorKind::FilesystemLoop` is unstable.** An `O_NOFOLLOW` open that
  meets a link answers `ELOOP`; test it with `shared::is_eloop`.
- **`deploy.toml` carries the pre-adoption `AppConfig` as `origin`.** It
  serialises as `[origin]` plus its sub-tables at the END of the file
  whatever the field's position, because this crate's `toml` hoists every
  scalar above every table. It holds `env` verbatim, so the file is written
  0o600. Two compatibility edges, both
  accepted by Rin on 2026-09-04: a shep-deploy older than that refuses a
  record with the field (`deny_unknown_fields`), and `AppConfig` itself is
  `deny_unknown_fields` in shep-core, so a record written against a newer
  shep-core than the reader's refuses too.
- **A committed Flockfile is refused for more than `user`/`group`.** `exec`
  probes, `out_file`/`err_file`, non-loopback probe targets, and `watch`,
  because the daemon acts on all of them at its own uid (read out of
  shep-daemon on 2026-09-04; `flockfile::refuse_repo_privilege` says why per
  field). Presence is checked before shape: serde builds a probe from a
  TOML array too, and a walk that read only tables let one through.
- **`Online` is a one-time verdict in shep.** A crash after it respawns under
  a new pid with the reload committed; a failed post-drain re-probe demotes
  to `Starting` under the same pid. Both verify modes dwell and re-check pids
  and status; `deploy::dwell_for` lengthens the probed dwell to
  `listen_timeout` for a `reuse_port` app, whose re-probe can land that late.
- **`Error::Raced` is held like any other ending.** The tree lock makes a
  rival deploy impossible, so `current` moving under a deploy is a hand or
  the release's build command, and an unheld build that repoints `current`
  would be rebuilt every tick.
- **The Flockfile is parsed once per deploy, through `flockfile::read`.** The
  free `app_config` and `build_spec` are `#[cfg(test)]` wrappers; production
  code asks both questions of one `Merged`.
- **`shared::FromCheckout` is minted only by `shared::to_link`.** It is the
  list of paths linked in from the operator's checkout, and its
  `includes_override` answer is the whole proof that `Flockfile.override.toml`
  is the operator's file. The field is private and there is no `From` impl on
  purpose: a trait impl is reachable from every module, which would let any
  caller mint the proof. Tests build one with `FromCheckout::of`.
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
