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

## A command that provisions the user a sheep should run as

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

Rough shape, not a design:

```sh
shep-deploy provision <sheep>          # what it would do, then stop
shep-deploy provision <sheep> --commit # do it
```

Creating a system user with no login shell and no password, taking ownership
of that sheep's deploy tree, and reporting the `user = "..."` line to add. It
would need to be idempotent, to refuse rather than adopt a user that already
exists and owns other things, and to say exactly what it is about to run
before it runs it, because it needs root and an operator handing root to a
tool deserves to read the commands first.

**Open questions this does not answer.** Whether it writes the Flockfile line
itself or only prints it, given the Flockfile lives in the deployed repository
and this crate refuses committed `user` keys for good reasons. Whether it
belongs here at all rather than in shep, since nothing about it is
deploy-specific and `shep adopt` is where an operator already goes to set a
dog up.
