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
```

`--watch` changes the setting and returns without deploying.

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

The deploy sequence and the operator command work, and there are tests against a real shepherd. Not built yet: the poll loop that makes `watch = auto` mean anything, opt-in (surveying the flock and cutting a sheep over for the first time), release retention, and Windows.

See [docs/writing-plans/plans/2026-08-26-deploy-engine.md](docs/writing-plans/plans/2026-08-26-deploy-engine.md) for what was built, and the [design spec](docs/brainstorming/specs/2026-08-26-deploy-dog-design.md) for what it is for.

## License

MIT OR Apache-2.0, at your option.
