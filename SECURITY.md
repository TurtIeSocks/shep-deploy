# Security Policy

## Disclaimer

shep-deploy is a community project, maintained on a reasonable-effort basis. It
cannot provide legally-binding guarantees of security.

## What this crate does that is dangerous

It runs a build command taken from the repository being deployed. That is the
whole point of a deploy dog, and it is also the entire risk surface. Whoever
can land a commit on the branch you track chooses that command, and it runs on
your host on their schedule.

Everything below is about bounding that, and each bound has an edge worth
knowing.

## Security premises

IF every deployed app sets `user`, so each build drops to an account of its
own, THEN:

- A build is confined to that account's privileges. It cannot read or write
  anything that uid cannot already reach.
- A compromised build reaches that one app's data, and not other apps'.

IF the shepherd merely runs unprivileged, with apps that set no `user`, THEN
the host is protected from a build and the apps are not protected from each
other. Every build runs as the shepherd's own uid, so it reads and writes
whatever any app under that shepherd can, including their deploy trees, their
logs, and any `.env` shared into their releases. That is better than root and
it is not isolation.

IF neither holds, and the shepherd runs as root with apps that set no `user`,
THEN a build script from the tracked branch runs as root, once per deploy.
shep-deploy warns when it is about to do that. It does not refuse: whether an
app runs without a `user` is shep's decision and the operator's, not a deploy
dog's.

## What the bounds cover, precisely

**The uid and gid drop bounds the child process.** It is applied to the build's
`Command`, with gid set before uid so supplementary groups are cleared too.
Anything shep-deploy itself does afterwards, on paths the build chose, runs at
the dog's own uid. A path traversal in `build.artifacts` exploited exactly that
gap on 2026-08-28 and reached the deploy tree's own `deploy.toml`, whose
`remote` every later fetch reads. Absolute and `..`-bearing artifact paths are
refused at parse time now, and again before the copy.

A residual on the destination was open for four review rounds and closed on
2026-09-04. The copy used to resolve where a destination really landed and then
open it, and a component swapped for a symlink between those two steps was
followed: the kernel honours `O_NOFOLLOW` on a path's last component and on
nothing above it. Both ends are now walked one component at a time from the
release or the build cache, with `openat` and `O_NOFOLLOW | O_DIRECTORY`
against the descriptor of the directory above, so nothing between a root and
the file is resolved by name at open time.

A component that answers `ELOOP` is read with `readlinkat` and followed only
when it names one of the two roots exactly, which is what keeps
`shared::link_cache`'s `release/target` working. The walk continues from that
root's own descriptor rather than from wherever the link pointed. Missing
destination parents are made with `mkdirat` on the walked descriptor, and the
file itself is opened `O_CREAT | O_EXCL | O_WRONLY | O_NOFOLLOW`. The
name-based checks still run first, because a walk can only refuse and they are
what say which end resolved where. A test races two thousand copies against a
thread swapping a parent directory for a symlink outside both roots. Against a
walk that follows such a component it goes red in a hundredth of a second.

**The cleared environment bounds what a build inherits from this process.** A
build gets `PATH`, `HOME`, `LANG`, `LC_ALL`, `TZ`, whatever `passthrough` names
in `[deploy]` in `dogs.toml`, and the release's own `[dog.deploy.build]`
env. Nothing else. Dropping uid does not unsee environment variables, because
they are copied into the child before the drop happens, so a dog started with
a registry token in its environment would otherwise hand it to every build.

`SSH_AUTH_SOCK` is excluded from that base set deliberately. A forwarded agent
reaching a build lets that build authenticate as the operator anywhere the
agent is trusted. Fetching happens in this process and keeps its own
environment, so a private repository still clones without it.

Excluded from the base set is not the same as refused, and this document said
only the first half. Naming it in `passthrough` hands it to every build, which
is what `passthrough` is: an operator's explicit allowlist, and README.md tells
them to reach for it when a build genuinely needs the socket. Nothing rejects
the key, on purpose, because an operator who writes it has said what they mean.
What it costs is the whole of the paragraph above, so it is worth being sure
the build command is one you would hand your agent to.

**Five more fields make the shepherd act at its own uid, and a committed
Flockfile is refused for setting any of them.** Read out of shep-daemon on
2026-09-04. A `readiness_probe` or `liveness_probe` of kind `exec` is run by
the daemon through `sh -c` on the probe's interval, forever, with no uid drop
and no regard for `user`: committed, it is a shell at the daemon's uid every
ten seconds, and root's under the arrangement this crate assumes.
`out_file` and `err_file` are opened, created and appended to by the daemon's
log pump and truncated by `shep flush`, at the daemon's uid, with a symlink
check on the final component and nothing on where the path points. An `http`
or `tcp` probe is a connection the daemon makes from its own network
position, so a committed target has to be on loopback. `watch` has the daemon
walk and watch the working directory recursively as itself, following
symlinks, so a committed link out of the release reaches whatever it points
at. All of them stay available in `Flockfile.override.toml`, which is the
operator's.

`interpreter`, `script`, `args` and `cwd` are not on that list because they
are the app's own command line. The daemon sets the child's gid and uid
before it changes directory and before it execs, so a committed interpreter
runs as `user` exactly as the script would, and a bare name is looked up on
the daemon's own `PATH` by a process that has already dropped. What remains
is small: the daemon stats the program at its own uid before spawning, to
refuse a batch whose program is provably absent, so a committed absolute
path learns whether that path exists on the host, one bit, through the
refusal, and nothing else.

**Nothing bounds what a build reads out of its own release.** Gitignored files
are symlinked into every release by design, which is what makes a `.env` work
at runtime. A build command can therefore read every secret the app itself can
read, whatever the environment allowlist says. The bound that matters there is
`user`: it confines the build and the app it builds to the same one account.

## What is not built

No sandbox. A build is bounded by uid, gid and its environment, and by nothing
else: it may reach any path that uid can reach. `docs/specs/deferred.md`
records Landlock as the thing that would close the class rather than
individual routes out of it, and why it is deferred.

No credential handling of any kind. Git auth is inherited from the user the dog
runs as, so a private repository behaves as it does in that user's own shell,
and no token passes through any URL or argument this crate builds.

One file of this crate's own holds secrets. `deploy.toml` records the app as
the shepherd had it before adoption, `env` included, so that removal can put
it back whole. It is written owner-only, the same mode shep uses for its own
roll, which holds the same values.

## Reporting

Open a private security advisory on
[the repository](https://github.com/shep-pm/shep-deploy/security/advisories).
