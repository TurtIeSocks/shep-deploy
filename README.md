# shep-deploy

[![Crates.io Version](https://img.shields.io/crates/v/shep-deploy.svg)](https://crates.io/crates/shep-deploy)
[![License](https://img.shields.io/crates/l/shep-deploy.svg)](https://github.com/shep-pm/shep-deploy#license)
[![MSRV](https://img.shields.io/crates/msrv/shep-deploy.svg)](https://crates.io/crates/shep-deploy)
[![CI](https://github.com/shep-pm/shep-deploy/actions/workflows/test.yml/badge.svg)](https://github.com/shep-pm/shep-deploy/actions/workflows/test.yml)

A deploy dog for [shep](https://github.com/shep-pm/shep).

Watches a git branch, builds a release in an isolated directory, swaps to it,
reloads the sheep, and rolls back on its own if the new release does not come
up.

It is an external dog, the same shape as
[shep-log-rotate](https://github.com/shep-pm/shep-log-rotate): an ordinary
binary you adopt, talking to the daemon over the socket the CLI already uses.

## Install

```bash
cargo install shep-deploy
shep adopt shep-deploy
```

Upgrading later wants `--force`:

```bash
cargo install shep-deploy --force
shep restart deploy
```

That flag is not politeness. This dog links `shep-client`, and its own version
does not change when that does, so after you upgrade shep the rebuild you need
is exactly the one cargo declines to do: it prints `already installed, use
--force to override`, builds nothing, and exits 0. It looks like it worked.

`shep adopt` registers it with the shepherd, which supervises it from then on.
`shep dogs` lists what you have adopted.

## Telling it how to build

The build command lives in the deployed repository's own Flockfile, under the
table shep keeps for a dog's config:

```toml
[[app]]
name = "web"
script = "server.js"

[dog.deploy.build]
command = "npm ci && npm run build"
artifacts = ["dist/bundle.js"]
```

shep reads nothing under `dog` and validates none of it. It only had to stop
refusing the document for carrying it, which it does as of 0.1.10, so the same
Flockfile now registers with `shep start` and tells this dog how to build.

That is a change from earlier versions, which put the block at the top level as
`[build]`. shep refused a Flockfile with an unknown top-level key, so an
operator following these instructions could not register their app at all. A
top-level `[build]` is now refused here by name, pointing at the new spelling,
rather than ignored and silently building nothing.

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
build_timeout = "1h"
passthrough = ["CARGO_HOME"]
```

All five are read once, when the dog starts, so changing any takes a `shep
restart deploy`.

`retention` is how many releases each target keeps **besides the live one**, so
a target holds up to `retention + 1` directories. The live release is spared
unconditionally, whatever its age. It cannot be below 2: the release a failed
deploy rolls back to is the second newest, so anything lower would silently
disable rollback, and it is refused rather than clamped.

`git_timeout` bounds the fetch, which is the only git call that talks to a network. The rest operate on local directories and cannot hang on a remote. Targets are deployed one at a
time, so without it a remote that drops packets rather than refusing them stops
every target and every smit refresh, with no error and no log line. Five
minutes by default, which is generous enough for a cold clone of a large
repository.

`build_timeout` bounds the build command, against the same failure and for the
same reason. A build that never finishes holds that same one-at-a-time loop, so
no other target deploys and nothing is logged, because from the loop's point of
view nothing has gone wrong. An hour by default, which no honest build should
reach: it exists to turn a build that will never finish into an ordinary
per-target failure, not to put a schedule on slow work. A build past it is
killed as a process group, so whatever the build command started goes with it.

`passthrough` names environment variables a build may keep from the dog's own
environment. See Security below for why that list starts empty.

One target's failure never stops the others, and never stops the dog. Each
target's outcome is reported on its own and the loop carries on. `up to date`
prints nothing at all, which is the answer to almost every tick of almost
every target, and a line that repeats is said once rather than every interval.

A tick never begins while the previous one is still running, so a push landing
during a build is deployed on the next tick rather than aborting the build in
flight.

Nor does a second process. Each deploy takes an exclusive `flock` on its own
tree for as long as it runs, so `shep-deploy deploy web` typed while the dog is
mid-tick on `web` is refused in one sentence rather than colliding somewhere
inside git. The lock is per sheep, so other targets are unaffected, and the
kernel releases it if the holder dies, so a killed dog leaves nothing behind
holding a sheep hostage.

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

## How it holds its connection

Adopted, the dog announces itself by the name shep gave it, taken from
`SHEP_DOG_NAME`. That is what shep records as a handshake. A dog that does not
send one connects, serves every request correctly, and is still shown as
`silent`, restarted once, and then written off as stale.

The connection survives a handover as well, so `shep upgrade` does not leave
the dog running against a socket nobody is listening on.

A refusal it does not survive. When a newer shepherd rejects the handshake on
protocol skew, no amount of reconnecting fixes it, so the dog reports the
shepherd's version and its refusal message, then exits `5` rather than ticking
on against a connection that will never answer. It cannot name its own protocol
number, because `LinkState::Refused` does not carry one. Upgrade whichever side
is behind and shep starts it again.

Run by hand, `shep-deploy deploy web` announces no name at all. It is not a
dog, and a command claiming to be one would have shep restart the real dog when
the command's own handshake was refused.

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

Most apps cannot set `SO_REUSEPORT`, and some cannot even be made to: Node
refuses `reusePort` on macOS outright. For those, stop the sheep before the
cutover and start it afterwards:

```bash
shep stop web
```
```bash
shep-deploy setup web
```

With the port free the newcomer binds and the cutover lands. It costs the
downtime this section already warns about, and it is the difference between a
setup that works and one that cannot. Measured 2026-08-28: the same app that
failed the cutover while running completed it once stopped.

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
Every deploy after the first is verified against a new process reaching
`Online`.

**That used to be weaker than it sounds, and the gap was shep's rather than
this crate's.** Measured 2026-08-28 against a real shepherd: a sheep with an
HTTP `readiness_probe` that never passes was marked `Online` about a second
into a reload, well inside its `listen_timeout`, and stayed there. The cause
was the reload's overlap. Both instances were up when the replacement's first
probe landed, the outgoing one answered it, and shep took that as the incoming
one proving itself. A release that started and never became ready was verified,
recorded as deployed, and not rolled back, with the app down while the record
said otherwise.

shep fixed it the same day. A probed app is now reloaded serially: the old
instance drains first, so the only process that can answer the replacement's
probe is the replacement. An app that sets `reuse_port` keeps the overlap it
asks for and gets a second probe once the drained instance is gone. Either way
a replacement that never answers is left `starting` rather than `online`, which
is what this crate reads, so `verify = "probed"` now means what it says.

One thing to size correctly. A `reuse_port` app's reload costs one more
`listen_timeout` than it used to, for that second probe, and the budget in
`deploy.rs` is `listen_timeout + graceful_timeout + slack` per instance. For
that one combination the budget can expire mid-check and roll back a release
that was fine. A false rollback rather than a false success, which is the right
direction to fail, but it wants a wider budget.

Rollback works for a release that crashes and for one that starts without
serving. Both are caught.

## Removing it

`shep-deploy on-remove` is the lifecycle hook: shep runs it before forgetting
the dog, and it puts every sheep back where it ran before the dog took over. A
sheep the dog bootstrapped has nowhere to go back to, so it is left running
from `current` and the report says exactly that, with the path.

The deploy tree is never deleted. It is not the dog's to delete, and in the
bootstrap case a running app is still pointing into it.

## Exit codes

Follows [shep's own
taxonomy](https://github.com/shep-pm/shep/blob/main/docs/specs/shep-v1.md):
`0` deployed or already up to date, `2` bad arguments, `4` bad configuration,
`5` no daemon answered or one refused this dog's handshake, `1` anything else.

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
`[dog.deploy]` in `shep.toml`, and the release's own `[dog.deploy.build] env`.
Nothing else. Dropping uid and gid bounds what a build can touch; it does
nothing about what it can read out of its own environment, because those values
are copied in before the drop happens. A dog started with a registry token in
its environment would otherwise hand it to every build it runs.

`SSH_AUTH_SOCK` is not in that base set, on purpose. A forwarded agent reaching
a build lets the build authenticate as you anywhere that agent is trusted.
Fetching happens in the dog's own process and keeps its own environment, so a
private repository still clones; only the build loses the socket. Name it in
`passthrough` if a build genuinely needs it.

`build.artifacts` may only name paths that really land inside the release or
the dog's own build cache, resolved rather than spelled. A `..`, an absolute
path, a committed symlink, or a `CARGO_TARGET_DIR` pointing elsewhere are all
refused. That is narrower than it was: a build whose output genuinely lands
outside both is no longer copyable, because the copy runs in the dog's own
process at its own uid, and the `[dog.deploy.build]` block naming the path
comes from the deployed repository rather than from you.

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
