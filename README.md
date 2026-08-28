# shep-deploy

[![Crates.io Version](https://img.shields.io/crates/v/shep-deploy.svg)](https://crates.io/crates/shep-deploy)
[![License](https://img.shields.io/crates/l/shep-deploy.svg)](https://github.com/TurtIeSocks/shep-deploy#license)
[![MSRV](https://img.shields.io/crates/msrv/shep-deploy.svg)](https://crates.io/crates/shep-deploy)
[![CI](https://github.com/TurtIeSocks/shep-deploy/actions/workflows/test.yml/badge.svg)](https://github.com/TurtIeSocks/shep-deploy/actions/workflows/test.yml)

A deploy dog for [shep](https://github.com/TurtIeSocks/shep).

Watches a git branch, builds a release in an isolated directory, swaps to it,
reloads the sheep, and rolls back on its own if the new release does not come
up.

It is an external dog, the same shape as
[shep-log-rotate](https://github.com/TurtIeSocks/shep-log-rotate): an ordinary
binary you adopt, talking to the daemon over the socket the CLI already uses.

## Install

```bash
cargo install shep-deploy
shep adopt shep-deploy
```

`shep adopt` registers it with the shepherd, which supervises it from then on.
`shep dogs` lists what you have adopted.

## What one deploy does

1. Fetch into a bare clone, and compare the branch head to the last deployed
   sha.
2. `git worktree add` the new sha, sharing the object store.
3. Symlink the shared files in: whatever git ignores and `.shepignore` does
   not.
4. Run the build, as the app's `user` if it sets one.
5. `rename(2)` `current` onto the new release.
6. `Reload` the sheep.
7. Verify. On failure, put `current` back and reload again.

Steps 1 to 4 never touch the running app. A build that fails costs a
directory, not an outage.

Verification waits for a *new* process to reach `Online`, not for any process
to be `Online`. shep answers a reload before it has finished one, and keeps
the old instance when the replacement never becomes ready, so "something is
online" is true throughout a deploy that failed.

## Layout

Everything lives under `$SHEP_HOME/deploy/<sheep>/`:

```text
git/                 one bare clone, shared by every release
releases/<sha>/      a worktree per release
current -> releases/<sha>
deploy.toml          remote, branch, deployed sha, held sha, verify mode, watch mode
```

The sheep's `cwd` is `current`, permanently. Set it explicitly when you
register the app: a Flockfile `cwd` left to default is resolved at
registration, which pins the sheep to one release and makes every later swap
invisible to it.

## Commands

```sh
shep-deploy deploy <sheep>
shep-deploy deploy <sheep> --watch auto|manual
shep-deploy setup <sheep>
shep-deploy survey
shep-deploy on-remove
```

`--watch` changes the setting and returns without deploying. `survey` reports
where every registered sheep stands and starts, registers and writes nothing.

## Running as a dog

Adopted as a dog, `shep-deploy` takes no arguments and polls instead. Every 30
seconds by default, it deploys any `watch = "auto"` target whose branch has
moved. Configure it in `shep.toml`:

```toml
[dog.deploy]
interval = "30s"
retention = 5
git_timeout = "5m"
passthrough = ["CARGO_HOME"]
```

All four are read once, when the dog starts, so changing any takes a `shep
restart deploy`.

`retention` is how many releases each target keeps **besides the live one**, so
a target holds up to `retention + 1` directories. The live release is spared
unconditionally, whatever its age. It cannot be below 2: the release a failed
deploy rolls back to is the second newest, so anything lower would silently
disable rollback, and it is refused rather than clamped.

`git_timeout` bounds any single git subprocess. Targets are deployed one at a
time, so without it a remote that drops packets rather than refusing them stops
every target and every smit refresh, with no error and no log line. Five
minutes by default, which is generous enough for a cold clone of a large
repository.

`passthrough` names environment variables a build may keep from the dog's own
environment. See Security below for why that list starts empty.

One target's failure never stops the others, and never stops the dog. Each
target's outcome is reported on its own and the loop carries on. `up to date`
prints nothing at all, which is the answer to almost every tick of almost
every target, and a line that repeats is said once rather than every interval.

A tick never begins while the previous one is still running, so a push landing
during a build is deployed on the next tick rather than aborting the build in
flight.

Every tick also paints each target's smit, so `shep flock` shows which branch
and sha it is on without a second command:

```text
▲ main@a1b2c3      watched
⏸ main@f6e5d4      manual
```

Republished every tick rather than on change: shep holds a smit in memory only
for as long as the connection that painted it stays open, so a dog that only
published on change would show nothing at all after a daemon restart until its
next deploy. A refused smit is logged and otherwise ignored, since it is
cosmetic and never worth failing a deploy over.

## A commit that fails is held

A commit that does not land is left alone until the branch moves. It is
written into `deploy.toml` as `failed`, because otherwise the branch head and
the deployed sha stay different and every tick runs the whole sequence again:
fetch, full rebuild, swap, reload, wait out the verification budget, roll back,
reload again. Two reloads of a live app and a build every thirty seconds, from
one bad commit, until somebody notices.

Pushing a fix clears it, which is what CI does with a red commit; so does
`shep deploy <sheep>`, which retries the same commit deliberately. The tradeoff
is deliberate too: a deploy that failed on a network blip rather than on the
commit waits for one of those two, rather than being retried on the next tick.

`shep-deploy survey` shows such a target as `held`, naming the commit it is
holding, so a target stuck since yesterday does not read like one with nothing
to do. Survey reads the record, never the remote, so a hold that a push already
cleared still shows until the next tick.

## Taking a sheep over

`setup` takes a sheep over: it builds the tree, fetches the repository, links
the shared files in, builds the first release, and re-registers the sheep with
its `cwd` set to `current`.

The first cutover is the one deploy that may have downtime. It runs two
instances at once, so an app that does not bind with `SO_REUSEPORT` cannot take
its own port while the original still holds it, and the new instance is then
removed and the original left serving. Every deploy after the first replaces
the instance rather than joining it, and does not meet this.

A cutover that was abandoned leaves the tree behind, and a sheep is not a
deploy target until a cutover lands. `shep-deploy deploy` refuses such a target
before anything at all happens, because its record names no deployed release.

That refusal is load-bearing rather than tidy. Without it the deploy would not
stop: it would build, swap, reload the sheep at its own checkout, see a real
turnover, and report success for a release nothing served.

Remove its tree and run `setup` again once the cause is fixed. Both `setup` and
the failure message say this, and both print the resolved path to remove rather
than a `$SHEP_HOME` you would have to expand yourself. `setup` leaves it
`watch = manual` until the cutover lands, and `--watch auto` on one is refused
for the same reason: it would ask for that deploy once every interval,
unattended.

## Verification

`verify = "probed"` (the default) needs the app to have a `readiness_probe` or
`wait_ready`; without one, shep reports a process `Online` for not having died
yet, so there is nothing to verify against and the deploy is refused.
`verify = "alive"` is the deliberate downgrade: a new process, still running
ten seconds later.

The first cutover is also the one deploy that is not verified against the
readiness probe. shep reports a freshly started process `Online` once its
`listen_timeout` elapses, whatever the probe said, and only aborts a *reload*
whose replacement was not ready. So `setup` checks what it can: a new process
started and was still the same process, not errored and not restarted, ten
seconds later. A release that starts, stays up and serves nothing passes that.
Every deploy after the first is verified properly, against the probe, with
automatic rollback.

## Removing it

`shep-deploy on-remove` is the lifecycle hook: shep runs it before forgetting
the dog, and it puts every sheep back where it ran before the dog took over. A
sheep the dog bootstrapped has nowhere to go back to, so it is left running
from `current` and the report says exactly that, with the path.

The deploy tree is never deleted. It is not the dog's to delete, and in the
bootstrap case a running app is still pointing into it.

## Exit codes

Follows [shep's own
taxonomy](https://github.com/TurtIeSocks/shep/blob/main/docs/specs/shep-v1.md):
`0` deployed or already up to date, `2` bad arguments, `4` bad configuration,
`5` no daemon answered, `1` anything else.

Two are this dog's own:

| code | means | the flock is |
|---|---|---|
| `12` | the deploy was rejected and the previous release was put back | healthy, on the old release |
| `13` | a first cutover landed and then could not tidy up | healthy, on the new release |

A script that treats any nonzero code as "the deploy broke" will be wrong about
both, because in each case the flock is serving.

## Security

A deploy runs the build command from the repository being deployed. That is
the point of it, and it is also the whole of the risk: `bun install`'s
postinstall scripts and `make build` are arbitrary code, chosen by whoever can
land a commit on the branch you track.

That build runs as this process's uid unless the app sets `user`. Supervised as
a dog, this process is the shepherd's child and shares its uid, and shep's own
docs recommend running the shepherd as root so it can drop privileges per app.
So the default arrangement is a repository's build script running as root, once
per deploy.

Set `user` on the app and the build drops to that user's uid and primary group,
with the shepherd's supplementary groups cleared, before it runs anything. A
compromised build then gets that app's privileges and nothing more.

shep-deploy warns when it is about to run a build as root with no `user` set.
It does not refuse. Whether an app runs without a `user` is shep's call and the
operator's, not a deploy dog's.

A build starts from a cleared environment, not this process's. It gets `PATH`,
`HOME`, `LANG`, `LC_ALL` and `TZ`, whatever `passthrough` names in
`[dog.deploy]`, and the release's own `[build] env`. Nothing else. Dropping uid
and gid bounds what a build can touch; it does nothing about what it can read
out of its own environment, because those values are copied in before the drop
happens. A dog started with a registry token in its environment would otherwise
hand it to every build it runs.

`SSH_AUTH_SOCK` is not in that base set, on purpose. A forwarded agent reaching
a build lets the build authenticate as you anywhere that agent is trusted.
Fetching happens in the dog's own process and keeps its own environment, so a
private repository still clones; only the build loses the socket. Name it in
`passthrough` if a build genuinely needs it.

**What the allowlist does not cover, and cannot.** Your project's own secrets
are not in the dog's environment, they are in your repository's working tree. A
`.env` file is gitignored, which is exactly the rule that makes shep-deploy
symlink it into every release, so the build reads it and so does the app at
runtime. That is the intended behaviour and the reason shared files exist. It
does mean a build command can read every secret the app itself can read, no
matter what this allowlist says. The bound worth having there is `user`, so
that a build and the app it builds are confined to the same one account.

Nothing else here handles credentials. Git auth is inherited from the user the
dog runs as, so a private repository works as it does in that user's own shell,
and no token passes through any URL or argument this crate builds.

## Platform

Unix only. This is deliberate, not a gap waiting to be filled by accident: the
deploy model is `rename(2)` over a symlink, the build's privilege drop is a uid
and a gid, and both are Unix concepts the code uses directly rather than
through a portability layer. Building on Windows fails with one sentence saying
so. Windows support is planned and will be scoped on its own.

## Status

Working: the deploy sequence, the operator commands, opt-in, the poll loop,
retention, and restore on removal. Tested against a real shepherd.

Not built: Windows.

See
[docs/writing-plans/plans/2026-08-26-deploy-engine.md](docs/writing-plans/plans/2026-08-26-deploy-engine.md)
for what was built, and the [design
spec](docs/brainstorming/specs/2026-08-26-deploy-dog-design.md) for what it is
for.

## License

MIT OR Apache-2.0, at your option.
