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
`[build]` block lives in the deployed repository, so whoever can land a commit
points it. Those are different people, and the whole threat model of this crate
turns on the difference.

If a build genuinely needs to salvage output from outside the tree, the
declaration has to come from somewhere the operator controls: an
`artifact_roots` list in `[dog.deploy]`, alongside `passthrough`, which the
repository can then name paths under but cannot extend. Same shape as the
environment allowlist, for the same reason. Not built, because nothing has
asked for it yet and the narrowing costs nothing until something does.
