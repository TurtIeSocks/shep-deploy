# shep-deploy

[![Crates.io Version](https://img.shields.io/crates/v/shep-deploy.svg)](https://crates.io/crates/shep-deploy)
[![License](https://img.shields.io/crates/l/shep-deploy.svg)](https://github.com/TurtIeSocks/shep-deploy#license)
[![MSRV](https://img.shields.io/crates/msrv/shep-deploy.svg)](https://crates.io/crates/shep-deploy)
[![CI](https://github.com/TurtIeSocks/shep-deploy/actions/workflows/test.yml/badge.svg)](https://github.com/TurtIeSocks/shep-deploy/actions/workflows/test.yml)

A deploy dog for [shep](https://github.com/TurtIeSocks/shep).

Watches a git branch, builds a release in an isolated directory, swaps to it, reloads the sheep, and rolls back on its own if the new release does not come up.

It is an external dog, the same shape as [shep-log-rotate](https://github.com/TurtIeSocks/shep-log-rotate): an ordinary binary you adopt, talking to the daemon over the socket the CLI already uses.

## What one deploy does

1. Fetch into a bare clone, and compare the branch head to the last deployed sha.
2. `git worktree add` the new sha, sharing the object store.
3. Symlink the shared files in: whatever git ignores and `.shepignore` does not.
4. Run the build, as the app's `user` if it sets one.
5. `rename(2)` `current` onto the new release.
6. `Reload` the sheep.
7. Verify. On failure, put `current` back and reload again.

Steps 1 to 4 never touch the running app. A build that fails costs a directory, not an outage.

Verification waits for a *new* process to reach `Online`, not for any process to be `Online`. shep answers a reload before it has finished one, and keeps the old instance when the replacement never becomes ready, so "something is online" is true throughout a deploy that failed.

## Layout

Everything lives under `$SHEP_HOME/deploy/<sheep>/`:

```text
git/                 one bare clone, shared by every release
releases/<sha>/      a worktree per release
current -> releases/<sha>
deploy.toml          remote, branch, deployed sha, verify mode, watch mode
```

The sheep's `cwd` is `current`, permanently. Set it explicitly when you register the app: a Flockfile `cwd` left to default is resolved at registration, which pins the sheep to one release and makes every later swap invisible to it.

## Usage

```sh
shep-deploy deploy <sheep>
shep-deploy deploy <sheep> --watch auto|manual
shep-deploy setup <sheep>
shep-deploy survey
shep-deploy on-remove
```

`--watch` changes the setting and returns without deploying. `survey` reports where every registered sheep stands and starts, registers and writes nothing.

`shep-deploy on-remove` is the lifecycle hook: shep runs it before forgetting the dog, and it puts every sheep back where it ran before the dog took over. A sheep the dog bootstrapped has nowhere to go back to, so it is left running from `current` and the report says exactly that, with the path. **The deploy tree is never deleted.** It is not the dog's to delete, and in the bootstrap case a running app is still pointing into it.

`setup` takes a sheep over: it builds the tree, fetches the repository, links the shared files in, builds the first release, and re-registers the sheep with its `cwd` set to `current`. **The first cutover is the one deploy that may have downtime.** It runs two instances at once, so an app that does not bind with `SO_REUSEPORT` cannot take its own port while the original still holds it, and the new instance is then removed and the original left serving. Every deploy after the first replaces the instance rather than joining it, and does not meet this.

A cutover that was abandoned leaves the tree behind, and **a sheep is not a deploy target until a cutover lands.** Its record names no deployed release, so `shep deploy` against it does not stop: it builds, swaps, reloads the sheep at its own checkout, sees a real turnover, and reports success for a release nothing served. Remove its tree and run `setup` again once the cause is fixed. Both `setup` and the failure message say this, and both print the resolved path to remove rather than a `$SHEP_HOME` you would have to expand yourself. `shep deploy` refuses such a target outright, `setup` leaves it `watch = manual` until the cutover lands, and `--watch auto` on one is refused for the same reason: it would ask for that deploy once every interval, unattended.

**It is also the one deploy that is not verified against the readiness probe.** shep reports a freshly started process `Online` once its `listen_timeout` elapses, whatever the probe said, and only aborts a *reload* whose replacement was not ready. So `setup` checks what it can: a new process started and was still the same process, not errored and not restarted, ten seconds later. A release that starts, stays up and serves nothing passes that. Every deploy after the first is verified properly, against the probe, with automatic rollback.

Exit codes follow [shep's own taxonomy](https://github.com/TurtIeSocks/shep/blob/main/docs/specs/shep-v1.md): `0` deployed or already up to date, `2` bad arguments, `4` bad configuration, `5` no daemon answered, `1` anything else. **`12` is this dog's own: the deploy was rejected and the previous release was put back.** A script that treats any nonzero code as "the deploy broke" will be wrong about `12`, where the flock is healthy on the old release.

`verify = "probed"` (the default) needs the app to have a `readiness_probe` or `wait_ready`; without one, shep reports a process `Online` for not having died yet, so there is nothing to verify against and the deploy is refused. `verify = "alive"` is the deliberate downgrade: a new process, still running ten seconds later.

## Security

A deploy runs the build command from the repository being deployed. That is the point of it, and it is also the whole of the risk: `bun install`'s postinstall scripts and `make build` are arbitrary code, chosen by whoever can land a commit on the branch you track.

**That build runs as this process's uid unless the app sets `user`.** Supervised as a dog, this process is the shepherd's child and shares its uid, and shep's own docs recommend running the shepherd as root so it can drop privileges per app. So the default arrangement is a repository's build script running as root, once per deploy.

Set `user` on the app and the build drops to that user's uid and primary group, with the shepherd's supplementary groups cleared, before it runs anything. A compromised build then gets that app's privileges and nothing more.

shep-deploy warns when it is about to run a build as root with no `user` set. It does not refuse. Whether an app runs without a `user` is shep's call and the operator's, not a deploy dog's.

Nothing else here handles credentials. Git auth is inherited from the user the build runs as, so a private repository works exactly as it does in that user's own shell, and no token passes through any URL or argument this crate builds.

## Platform

Unix only. This is deliberate, not a gap waiting to be filled by accident: the deploy model is `rename(2)` over a symlink, the build's privilege drop is a uid and a gid, and both are Unix concepts the code uses directly rather than through a portability layer. Building on Windows fails with one sentence saying so. Windows support is planned and will be scoped on its own.

## Status

The deploy sequence, the operator commands, opt-in, and on-remove restore all work, and there are tests against a real shepherd. Not built yet: the poll loop that makes `watch = auto` mean anything, and Windows.

See [docs/writing-plans/plans/2026-08-26-deploy-engine.md](docs/writing-plans/plans/2026-08-26-deploy-engine.md) for what was built, and the [design spec](docs/brainstorming/specs/2026-08-26-deploy-dog-design.md) for what it is for.

## License

MIT OR Apache-2.0, at your option.
