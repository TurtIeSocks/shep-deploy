# Deferred — what shep-deploy does not do yet, and why

Ideas with a reason to exist, recorded so they are decisions rather than
oversights. Nothing here is committed to a version.

Opened 2026-08-28 out of the founder's review, which fixed three real defects
and surfaced two things worth building rather than patching.

## Sandbox the build with Landlock

A deploy runs a build command from the repository being deployed. That is the
point of it and it is also the whole risk surface, which the README says
plainly. Today the only bound on that command is the uid and gid drop in
`build::run`, plus the cleared environment added the same day.

Both are process-level bounds. Neither stops a build from reading or writing
anything that uid can reach, and the review found a concrete escape through
exactly that gap: a `..` in `build.artifacts` reached the tree's own
`deploy.toml`, whose `remote` every later fetch reads. That one is fixed by
refusing the path, but the class is not: the fix names one route out, and a
sandbox would close the whole direction.

**Landlock is the right shape.** It is a Linux LSM that lets a process
voluntarily give up filesystem access it will never need, enforced by the
kernel and inherited by children. No image, no daemon, no root, nothing to
install. The `landlock` crate wraps it. Filesystem rules land in 5.13, TCP
rules in 6.7.

The ruleset almost writes itself, because a build genuinely needs very little:

```text
write:  <release>, <tree>/cache, /tmp
read:   /usr, /lib, /etc/ssl, the toolchain's own cache
deny:   everything else, including $SHEP_HOME/deploy/*/deploy.toml
```

Applied immediately before `exec`, in the same place the uid drop already
happens. The last line is the interesting one: it would have stopped the
artifact escape at the kernel rather than at a path check.

**What makes this a deferral rather than a task.** It is Linux only, and
shep-deploy supports macOS too. macOS has Seatbelt (`sandbox_init`), which is
deprecated, undocumented and awkward to target, so the honest shape is
`sandbox = "landlock"` in `[dog.deploy]`, opt in, a no-op with a note on
macOS. A feature that silently does nothing on half the platforms it claims to
support needs to say so where an operator will read it.

Also worth stating: it is not a substitute for the mitigations that already
exist. Running the shepherd as a non-root user and setting `user` on every app
closes the ordinary cases. Landlock is what closes the unusual ones.

## A command that provisions the unix user a sheep should run as

The single cheapest thing an operator can do for build safety is give each app
its own unprivileged user and set `user` on it. shep's own docs recommend
running the shepherd as root precisely so it can drop to that user per app.

Nothing helps them do it. `useradd`, a home directory, ownership of the deploy
tree, and the `user` key in the Flockfile are four separate steps across two
tools, and getting any of them wrong fails in a way that reads as a broken
deploy rather than as a permissions problem. So the default in practice is no
`user` at all, which means builds run as the shepherd, which means root.

Rin's framing, 2026-08-28: a command for this, "for noobs". That is the right
target. The people most exposed by a build running as root are exactly the
people least likely to know that is what is happening.

Rin's call, same day, on both open questions this originally left hanging:

**It is a shep command, not a dog command.** Nothing about "give this app its
own user" is deploy-specific, and `shep adopt` is already where an operator
goes. Recorded here because the review that surfaced it happened here; the
entry belongs in shep's own `docs/specs/deferred.md` and should move there.

**It does not modify the Flockfile. It reads it.** This is the better shape and
it dissolves the question rather than answering it. The Flockfile already
declares `user = "svc-worker"`; the command reads that declaration and makes it
true on the host. Nothing writes into the deployed repository, so this crate's
refusal of a committed `user` key stands untouched, and the operator's
statement of intent stays the single source of truth.

Rough shape, not a design:

```sh
shep provision <sheep>            # read the Flockfile, say what is missing
shep provision <sheep> --commit   # make it so
```

Reading `user` from the app's own Flockfile entry, then creating that system
user if absent (no login shell, no password), and taking ownership of the
paths that user needs: its deploy tree, its log files, its home. Reporting what
already matched rather than redoing it.

It needs to be idempotent, to refuse rather than adopt a user that already
exists and owns things it did not create, and to print every command before
running it. It needs root, and an operator handing root to a tool deserves to
read what it is about to do first.

The pleasant consequence of reading rather than writing: an app with no `user`
declared has nothing to provision, and the command can say so plainly. That is
the moment to tell someone their build is running as the shepherd, which is
the whole problem this is for.

## An operator-declared artifact source

`build.artifacts` can no longer name a source outside the release and the
dog's own cache. Refused since 2026-08-28, because `CARGO_TARGET_DIR` comes
from the deployed repository's Flockfile and the copy-back runs in the dog's
own process at its own uid: pointing it anywhere on the host read that file
into the release, where a static-serving app then hands it out over HTTP. No
build code had to run.

That narrows something this module's own doc described as intended: "artifacts
remains for the builds that genuinely do put their output somewhere the release
cannot see, including one an operator points elsewhere themselves."

The words hid the problem. The operator does not point it anywhere; the
`[dog.deploy.build]` block lives in the deployed repository, so whoever can
land a commit points it. Those are different people, and the whole threat model
of this crate turns on the difference.

If a build genuinely needs to salvage output from outside the tree, the
declaration has to come from somewhere the operator controls: an
`artifact_roots` list in `[dog.deploy]`, alongside `passthrough`, which the
repository can then name paths under but cannot extend. Same shape as the
environment allowlist, for the same reason. Not built, because nothing has
asked for it yet and the narrowing costs nothing until something does.

## Resolve an artifact's destination without a race

`copy_artifact` checks where a destination really lands, then opens it.
Between those two steps a build's backgrounded job can swap a directory
component for a symlink, and the kernel follows it: `O_NOFOLLOW` governs the
last component of a path and nothing above it.

Round 7 of the founder's review raised this from two independent lenses.
Neither won the race in a harness. This crate has measured a comparable one
being won in 0.03s, so treat it as narrow rather than as theatre.

Two things narrow it already. A component that is a symlink when the first
check runs is refused at any depth IF it resolves outside the release and the
cache, because `lands_within` resolves the deepest existing ancestor rather
than trusting the spelling. And the destination is resolved again immediately
before the open, so what is left is one call rather than the several syscalls
it was.

Closing it needs a handle-based walk, and the flag matters.
`RESOLVE_NO_SYMLINKS` is the obvious reach and is wrong: it refuses every
symlinked component,
including `release/target`, which `shared::link_cache` creates on purpose. A
design built on it would refuse the ordinary cargo arrangement, which is the
same mistake an earlier attempt already made in code.

What is wanted is confinement rather than refusal: `openat2` with
`RESOLVE_IN_ROOT`, which follows a link but cannot leave the root it was given,
or a per-component walk that opens each part by handle and checks containment
as it goes.

Which root is the whole question, and the obvious answer is wrong. Rooting at
the release refuses `release/target`, because `link_cache` points it at
`<tree>/cache/target`, and the cache is the release's sibling rather than its
child. `RESOLVE_IN_ROOT` confines each lookup to one `dirfd`, so a
release-rooted lookup cannot follow that link anywhere useful: an absolute
target is reinterpreted against the release, and a relative one climbing out is
clamped. The design would refuse the ordinary cargo arrangement again, one
layer deeper than the two attempts before it.

The tree root is the answer, since `releases/<sha>` and `cache` are both under
it, so a single confined lookup spans exactly the two places an artifact may
legitimately land and nothing else. A two-root walk checking containment
against release and cache separately works too, and is what `lands_within`
already does by name today.

This entry used to say that needs `unsafe` and stop there, which was the wrong
place to stop. `#![forbid(unsafe_code)]` at `src/main.rs:33` rules out calling
the syscall from here, and a crate that does it for us is the ordinary answer
rather than a dodge: `cap-std` gives capability-based directory handles on both
platforms this crate supports, and `rustix` exposes `openat2` where the kernel
has it. Neither is exotic and both are maintained.

So the cost is a dependency and a rewrite of `copy_artifact`'s destination
handling, not a safety principle. `openat2` alone is Linux 5.6 and later, so
anything cross-platform means `cap-std`, whose directory handles give the same
confinement without naming a flag.

Still deferred, now for the reason that is actually the reason: the window is
one call wide, what gets written through it is the build's own output, and the
rewrite touches the most-reviewed function in the crate. Worth its own change
rather than one made on the way past.

An earlier attempt refused every symlinked component outright. That is
recorded here because it looks correct and is not: `shared::link_cache` makes
`release/target` a link at the dog's own cache, so the refusal broke the
ordinary cargo arrangement rather than an attack.

## Two things shep should export, so a dog stops copying them

Round 9 of the founder's review audited the boundary this crate exists to
test: whether somebody outside shep's workspace can build a dog from what shep
publishes. Both findings are gaps in shep, not bugs here, so neither is fixed
in this repository.

**The reload deadline slack.** `deploy::RELOAD_DEADLINE_SLACK` is five seconds,
and its own doc says it is copied from `RELOAD_DEADLINE_SLACK` in shep's
`supervisor.rs`. That one is a private `const` in shep-daemon, which dogs
correctly do not depend on. So the value a dog uses to size its reload budget
is a transcription, and it goes wrong silently if shep ever retunes it.

What would fix it is shep exporting the slack from shep-core, or better the
whole `listen_timeout + graceful_timeout + slack` formula, so a dog asks for a
reload budget instead of computing one. shep-client already sets the
precedent: its own source calls `spawn::DAEMON_ALREADY_RUNNING` an exit-code
contract worth exporting.

**The exit-code taxonomy.** `main::code_for` hardcodes 4 for a config refusal
and 5 for a failure to connect, sourced from `docs/specs/shep-v1.md` in a
different repository that is not vendored here. Nothing checks the copy at
compile time. The same fix applies: name the taxonomy publicly alongside
`DAEMON_ALREADY_RUNNING` so a dog writes a constant rather than a literal
copied out of somebody else's documentation.

Until then both copies stay, documented, which is the honest state rather than
a fix.

## Whether verification should read the bus instead of polling

`verify.rs` decides a reload finished by polling `Describe` and diffing pid
sets between generations. shep's own bus already reports both outcomes by
name, as `ProcessEventKind::Reloaded` and `ProcessEventKind::ReloadAbandoned`,
and `Client::subscribe` is exported.

So the crate infers something it could be told. Round 9 raised it as a
question rather than a defect, and it is Rin's call: the polling path works,
is covered by fake-based tests, and answers correctly for a shepherd that
refuses a subscription.

If it is worth pursuing, the shape that keeps what already works is a second,
optional trait method on `Daemon` feeding an event-driven path, with the
polling path as the fallback, rather than a replacement.

## A reload's replacement was verified by the instance it replaced -- FIXED in shep 0.1.10

**Fixed upstream on 2026-08-28, the day it was found.** shep now reloads a
probed app serially: the old instance drains before the replacement starts, so
the only process that can answer the replacement's probe is the replacement. An
app that sets `reuse_port` keeps the overlap it asked for and gets a second
probe once the drained instance is gone. `reuse_port` stopped being refused in
the same release and is now the opt-in for that overlap.

Verified end to end against the testbed this crate's review built: the same
broken release that was reported `deployed` with exit 0 while the old instance
answered its probe is now refused and rolled back with exit 12, and the healthy
release is what ends up serving. `README.md` describes the working behaviour.

The title below is what this entry was called when it was open, and it was
wrong in a way worth keeping. shep never ignored the probe. It honoured it, and
the probe was answered by the wrong process.

### As it was found: shep marks a reload's replacement Online without the readiness probe

Found by deploying real repositories against a real shepherd, which twelve
rounds of code review could not find: every test in this crate drives a fake
`Daemon` whose `describe` answers whatever the test wants, and `verify.rs`'s
own doc already says a fake like that is how an earlier bug survived two
rounds.

Measured 2026-08-28, shep 0.1.8 with shep-deploy at this branch. A testbed app
declares an HTTP `readiness_probe` at a port it then stops listening on. The
probe is registered and confirmed present in shep's `flock.json`. On reload:

```text
t+0.0s  probe answers YES  status=online  pid=7349   (the old release)
t+1.0s  probe answers no   status=online  pid=7745   (the new one, already Online)
t+25.8s probe answers no   status=online  pid=7745
```

One second, against a `listen_timeout` of ten, with the probe never passing.
`shep-deploy deploy` reported `deployed` and exited 0. A control run of `shep
reload` by hand, with no dog involved, behaves identically and also exits 0, so
this is shep's own path rather than anything the dog does.

**What it costs.** `verify = "probed"` is documented as the safe default and
watches for a new process reaching `Online`. Since shep grants `Online` without
the probe, a release that starts and never becomes ready is verified, recorded
as deployed, and never rolled back. The app is down and the record says it is
fine. That is the exact failure automatic rollback exists to prevent.

**Where the fix goes.** shep, not here. `reload_ready_result` in
`shep-daemon/src/supervisor.rs` already aborts a reload when readiness reports
`TimedOut` and an old instance survives, so the machinery exists; what needs
establishing is why a failing probe does not produce `TimedOut` on this path.

Nothing in this crate can work around it honestly. Polling the probe target
itself would mean shep-deploy reimplementing readiness, and it would still be
guessing: the probe's definition lives in the Flockfile shep owns. So the
README now says what the verification actually checks rather than what it was
intended to check.
