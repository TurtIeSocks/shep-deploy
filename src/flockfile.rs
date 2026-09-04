//! Reading the release's own Flockfile, merging the operator's override over
//! it, and producing the [`AppConfig`] for one sheep.
//!
//! Two files, and it matters which of them is trusted:
//!
//! - `Flockfile.toml`, committed in the release. Upstream owns it - it is
//!   whatever the tracked branch happened to contain at this release's sha.
//! - `Flockfile.override.toml`, gitignored, never committed. The operator
//!   owns it, and it deep merges over the committed file with the override
//!   winning on every key it names. A sheep with no override simply follows
//!   upstream on every deploy; a sheep whose operator pins `script` or `cwd`
//!   in an override keeps that value even when a pull changes upstream's
//!   own copy.
//!
//! ## The one real boundary here
//!
//! Everything above is defence in depth, not a boundary: a compromised
//! upstream already runs arbitrary code the moment this dog runs `bun
//! install`'s postinstall or `make build`, and pinning `script` in an
//! override does not stop that - it only stops a *later* pull from moving
//! the pointer. `user` and `group` are different in kind, not degree. A
//! build cannot escalate to another unix user on its own; the daemon
//! choosing which user a sheep's process runs as is the one privilege a
//! Flockfile can grant that the build itself never could. So a *committed*
//! Flockfile is refused outright the moment it sets either field on any app
//! it declares - see [`refuse_repo_privilege`] - and the check runs against
//! the committed document alone, before the override is even read, so an
//! override cannot make the refusal disappear by also touching the same
//! key. The override itself is under no such restriction: it is the
//! operator's own file, and pinning `user`/`group` there is exactly the
//! "pin it so upstream can never change it" mechanism every other field
//! already gets.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use shep_client::shep_core::prelude::AppConfig;
use toml::{Table, Value};

use crate::build::BuildSpec;
use crate::error::Error;
use crate::shared::{is_eloop, o_nofollow, printable};

/// The most a Flockfile may be, in bytes.
///
/// A mebibyte, which is a thousand times any real one. Both files are read
/// whole and merged in memory, and both come from the deployed repository,
/// so without a bound a commit could hand this dog a file large enough to
/// hold the poll loop, and the tree's lock, for as long as it liked.
const MAX_FLOCKFILE_BYTES: u64 = 1024 * 1024;

/// The two Flockfiles of one release, read once and merged.
///
/// [`Merged::app_config`] and [`Merged::build_spec`] used to be free functions
/// that each read and merged both files again; a deploy calls both, so every
/// release was parsed twice. Reading
/// through this type parses once and answers both questions from the same
/// document, which is also the only way the two answers cannot disagree.
#[derive(Debug)]
pub struct Merged {
    /// The committed document with the override merged over it.
    doc: Value,
    /// The release both were read from, for messages.
    release: PathBuf,
}

/// Reads and merges `release`'s Flockfiles, refusing first if the committed
/// one sets `user` or `group`.
///
/// # Errors
/// [`Error::Io`], naming the failing path, if `Flockfile.toml` cannot be
/// read or `Flockfile.override.toml` exists but cannot be read.
/// [`Error::Config`] if either file is not valid TOML, is a symlink the
/// repository committed, or is past [`MAX_FLOCKFILE_BYTES`], or if the
/// committed file sets `user` or `group` on any app.
pub fn read(release: &Path, shared: &[PathBuf]) -> Result<Merged, Error> {
    Ok(Merged {
        doc: merged_document(release, shared)?,
        release: release.to_owned(),
    })
}

impl Merged {
    /// The [`AppConfig`] for `sheep`: the merged entry named `sheep`,
    /// deserialised.
    ///
    /// # Errors
    /// [`Error::Config`] if no app named `sheep` exists after merging, or if
    /// that app's table does not match [`AppConfig`]'s schema.
    pub fn app_config(&self, sheep: &str) -> Result<AppConfig, Error> {
        let app = select_app(&self.doc, sheep)?;
        app.try_into().map_err(|source: toml::de::Error| {
            Error::Config(format!(
                "app {sheep:?} does not match shep's app schema: {}",
                printable(source.message())
            ))
        })
    }

    /// The `[dog.deploy.build]` block, or the default (no command, which
    /// [`crate::build::run`] treats as a no-op) if neither file declares
    /// one.
    ///
    /// # Errors
    /// [`Error::Config`] if the block does not match [`BuildSpec`]'s schema -
    /// an unknown key, or a value of the wrong type - or if either file still
    /// carries a top-level `[build]`.
    pub fn build_spec(&self) -> Result<BuildSpec, Error> {
        let doc = self.doc.as_table();

        // Refused rather than ignored, because ignoring it builds nothing and
        // says nothing: a release whose build never ran, swapped in and
        // reported as deployed. The old spelling is in this crate's own
        // published README, so whoever meets this message is following
        // instructions that were right at the time. Both files are named
        // because the key can be in either: the override merges over the
        // committed file before this looks.
        if doc.is_some_and(|doc| doc.contains_key("build")) {
            return Err(Error::Config(format!(
                "{}: `[build]` moved to `[dog.deploy.build]`, in Flockfile.toml or \
                 Flockfile.override.toml, whichever carries it. shep refuses a Flockfile with \
                 a top-level `build` key, so the old spelling could not be registered with \
                 `shep start` at all; `[dog]` is the table shep keeps for a dog's own config. \
                 Rename the block.",
                self.release.display()
            )));
        }

        let Some(build) = doc
            .and_then(|doc| doc.get("dog"))
            .and_then(Value::as_table)
            .and_then(|dogs| dogs.get("deploy"))
            .and_then(Value::as_table)
            .and_then(|deploy| deploy.get("build"))
        else {
            return Ok(BuildSpec::default());
        };

        build.clone().try_into().map_err(|source: toml::de::Error| {
            Error::Config(format!(
                "{}: `[dog.deploy.build]` does not match the build schema: {}",
                self.release.display(),
                printable(source.message())
            ))
        })
    }
}

/// The [`AppConfig`] for `sheep`, built from `release`'s own Flockfile.
///
/// Reads `Flockfile.toml` (required) and `Flockfile.override.toml`
/// (optional - a sheep with none simply has nothing to merge), refuses if
/// the committed file sets `user` or `group` on any app, deep merges the
/// override over the committed document with arrays of apps matched by
/// `name` rather than position, then selects and deserialises the entry
/// named `sheep`.
///
/// # Errors
/// [`Error::Io`], naming the failing path, if `Flockfile.toml` cannot be
/// read or `Flockfile.override.toml` exists but cannot be read.
/// [`Error::Config`] if either file is not valid TOML, if the committed
/// file sets `user` or `group` on any app, if no app named `sheep` exists
/// after merging, or if that app's table does not match [`AppConfig`]'s
/// schema.
///
/// The deploy and the cutover read through [`read`] and ask both questions
/// of one document; this one-question form is what the tests drive.
#[cfg(test)]
pub fn app_config(release: &Path, sheep: &str, shared: &[PathBuf]) -> Result<AppConfig, Error> {
    read(release, shared)?.app_config(sheep)
}

/// The release's `[dog.deploy.build]` block, or the default (no command, which
/// [`crate::build::run`] treats as a no-op) if it declares none.
///
/// Read from the same merged document [`app_config`] reads, so the
/// operator's override wins here too. That is not incidental: the block's
/// `env` routinely names host-specific paths - a registry token or a
/// `NODE_ENV`, say - and those are exactly the values a committed file
/// cannot know and an operator has to pin locally.
///
/// Not a key on the app entry, because `AppConfig` refuses unknown fields: a
/// `build` key inside `[[app]]` would make shep's own parser reject the
/// entry. One block per Flockfile also matches what a release actually is,
/// one checkout built once, even when several sheep are deployed from the
/// same repository.
///
/// Under `[dog.deploy.build]` rather than a top-level `[build]`, and that move is
/// the whole reason this doc changed. shep's `RawFlockfile` denies unknown
/// fields at the top level too, so a Flockfile carrying `[build]` could not
/// be registered with shep at all: `shep start Flockfile.toml` answered
/// "unknown field `build`, expected `$schema` or `app`". An operator
/// following this crate's own README could not complete step one. shep
/// gained a `dog` table for exactly this in 0.1.10, and this is the key that
/// goes in it.
///
/// # Errors
/// As [`app_config`] for reading and merging the two files, plus
/// [`Error::Config`] if the block does not match [`BuildSpec`]'s schema - an
/// unknown key, or a value of the wrong type - or if the Flockfile still
/// carries a top-level `[build]`.
///
/// As [`app_config`]: the one-question form, for the tests.
#[cfg(test)]
pub fn build_spec(release: &Path, shared: &[PathBuf]) -> Result<BuildSpec, Error> {
    read(release, shared)?.build_spec()
}

/// The committed Flockfile with the operator's override merged over it,
/// refusing first if the committed file sets `user` or `group`.
///
/// Both public readers go through here, so the refusal applies whichever
/// one was called and neither can see a document the other could not.
///
/// # Errors
/// As [`read`].
fn merged_document(release: &Path, shared: &[PathBuf]) -> Result<Value, Error> {
    // The committed file is never followed through a symlink: it is the
    // repository's, and the repository does not get to choose which file on
    // this host the dog reads. See `read_flockfile`.
    let committed = read_required(&release.join("Flockfile.toml"), Follow::Never)?;
    refuse_repo_privilege(&committed)?;
    refuse_duplicate_apps(&committed)?;

    // The override is exempt from the privilege refusal only because it is the
    // OPERATOR's file, and that has to be established rather than assumed. A
    // repository that simply committed a `Flockfile.override.toml` had
    // `user = "root"` honoured, which is the one thing this module's own doc
    // calls the real boundary. Measured 2026-08-28.
    //
    // Asked of `shared`, the list of paths the caller just linked in from the
    // operator's checkout, because that is the only record of where a file
    // came from. See `is_operators` for the two filesystem-based versions of
    // this check that came before, and why neither could work.
    //
    // Decided BEFORE the read, because it also decides whether the read may
    // follow a symlink: the operator's override IS one, into their checkout,
    // and a committed one must not be.
    let operators = is_operators(shared);
    let override_path = release.join(OVERRIDE);
    let override_doc = read_optional(
        &override_path,
        if operators {
            Follow::Operators
        } else {
            Follow::Never
        },
    )?;
    if !operators {
        refuse_repo_privilege(&override_doc)?;
    }

    Ok(deep_merge(committed, override_doc))
}

/// Whether the override in this release is the operator's own file.
///
/// Answered from the list of paths the caller just shared, not from the
/// filesystem. Three versions of this check asked the filesystem and all three
/// were wrong, the last one subtly enough to be worth recording.
///
/// It required the override to resolve OUTSIDE the release, on the grounds
/// that the operator's arrives as a symlink into their checkout. A repository
/// can satisfy that in two commits. Release A ships an ordinary tracked file
/// holding `user = "root"` under some innocuous name, and goes live, which by
/// this crate's own invariant means `current` points at it. Release B then
/// commits `Flockfile.override.toml` as a symlink to
/// `../../current/.deploy-payload.toml`. That resolves into release A, which
/// is outside release B, so the check passed and the refusal was skipped.
/// Demonstrated 2026-08-28.
///
/// The lesson is not that the rule needed another clause. It is that
/// provenance cannot be recovered from a path once the path exists: whoever
/// can write the tree can write the evidence. `crate::shared::link_into` is
/// the only thing that knows which files came from the operator's checkout,
/// because it is what put them there, so the answer travels from its caller
/// instead of being reconstructed here.
fn is_operators(shared: &[PathBuf]) -> bool {
    shared.iter().any(|p| p == Path::new(OVERRIDE))
}

/// The override's name, in the one place both the check and the read use it.
///
/// `crate::shared::to_link` needs it too, to keep a repo-committed
/// `.shepignore` from filtering the operator's own file out of the list this
/// module then treats as proof of provenance.
pub(crate) const OVERRIDE: &str = "Flockfile.override.toml";

/// Whether a Flockfile read may follow a symlink.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Follow {
    /// The file is the operator's, linked in from their checkout, so the
    /// link is the whole point.
    Operators,
    /// The file is the repository's, and a link is refused: see
    /// [`read_flockfile`].
    Never,
}

/// Reads and parses a Flockfile that must exist.
///
/// # Errors
/// [`Error::Io`] if `path` cannot be read at all. [`Error::Config`] if its
/// contents are not valid TOML, if it is a symlink and `follow` says not to
/// follow one, or if it is past [`MAX_FLOCKFILE_BYTES`].
fn read_required(path: &Path, follow: Follow) -> Result<Value, Error> {
    let text = read_flockfile(path, follow)?;
    parse(path, &text)
}

/// As [`read_required`], but a missing file is not an error - it is treated
/// as an empty document, the same "absent means zero configuration"
/// precedent [`crate::shared::shepignore_patterns`] already sets for
/// `.shepignore`. `Flockfile.override.toml` is optional by design.
///
/// # Errors
/// As [`read_required`], except that an absent file is an empty document.
fn read_optional(path: &Path, follow: Follow) -> Result<Value, Error> {
    match read_flockfile(path, follow) {
        Ok(text) => parse(path, &text),
        Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(Value::Table(Table::new()))
        }
        Err(err) => Err(err),
    }
}

/// Reads one Flockfile whole, bounded, and without following a symlink
/// unless the file is the operator's.
///
/// The symlink refusal is a security boundary rather than tidiness. This
/// read runs in the dog's own process at its own uid, which under the
/// arrangement shep's docs recommend is root, and it runs BEFORE any build
/// drops to `user`. `git worktree add` materialises a committed symlink
/// exactly as committed, so `Flockfile.toml -> /root/.ssh/id_rsa` in the
/// tracked branch had this read the key, fail to parse it, and print its
/// first line into the log through the parser's own error. `user` is the
/// one bound this crate names as covering what a build can READ, and a
/// commit walked around it without running a build at all. Found in review
/// on 2026-09-03.
///
/// `O_NOFOLLOW` makes the kernel refuse the open when the last component is
/// a link. A component above it is not this function's concern: those are
/// the release directory and the tree, which only this dog writes.
///
/// # Errors
/// [`Error::Config`] naming `path` if it is a symlink this read may not
/// follow, or if it is larger than [`MAX_FLOCKFILE_BYTES`]. [`Error::Io`]
/// naming `path` for anything else, `NotFound` included.
fn read_flockfile(path: &Path, follow: Follow) -> Result<String, Error> {
    let at = |source| Error::Io {
        path: path.to_owned(),
        source,
    };

    let mut options = fs::OpenOptions::new();
    options.read(true);
    if follow == Follow::Never {
        options.custom_flags(o_nofollow());
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(source) if is_eloop(&source) => {
            return Err(Error::Config(format!(
                "{} is a symlink, and a Flockfile the repository commits has to be a regular \
                 file: this dog reads it at its own uid before any build drops privilege, so a \
                 link could point it at any file on the host. Commit the file itself.",
                path.display()
            )));
        }
        Err(source) => return Err(at(source)),
    };

    let len = file.metadata().map_err(at)?.len();
    if len > MAX_FLOCKFILE_BYTES {
        return Err(Error::Config(format!(
            "{} is {len} bytes, past the {MAX_FLOCKFILE_BYTES}-byte limit for a Flockfile. A \
             real one is a few hundred bytes; this dog reads and merges both files in memory \
             on every deploy.",
            path.display()
        )));
    }

    let mut text = String::with_capacity(usize::try_from(len).unwrap_or(0));
    file.read_to_string(&mut text).map_err(at)?;
    Ok(text)
}

/// Parses `text` as a generic TOML document, naming `path` and the line in
/// the error if it does not parse.
///
/// The parser's own `Display` quotes the offending line back, which is
/// right for a file the reader wrote and wrong for one the repository did:
/// the read above refuses a symlink so that no file on the host can reach
/// this parser, and this keeps the text out of the log in case one ever
/// does. The message and the line number are what an operator needs; the
/// line's text is in the file.
fn parse(path: &Path, text: &str) -> Result<Value, Error> {
    toml::from_str(text).map_err(|source| {
        // Counted over bytes, because a span's start is a byte offset and
        // slicing a `str` at a byte that is not a character boundary panics,
        // on text the repository wrote.
        let line = source.span().map(|span| {
            let upto = span.start.min(text.len());
            text.as_bytes()[..upto]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + 1
        });
        // `printable`, because serde's own message carries the offending
        // key by name, and a TOML key can be any quoted string.
        let message = printable(source.message());
        Error::Config(match line {
            Some(line) => format!("{}: {message}, at line {line}", path.display()),
            None => format!("{}: {message}", path.display()),
        })
    })
}

/// The one real boundary this module enforces: refuses if any app declared
/// in `committed` sets `user` or `group` at all, regardless of what value it
/// names.
///
/// Takes the *committed* document specifically, never the merged one. If
/// this ran against the merged document instead, a legitimate use of the
/// mechanism - an operator pinning `user` in their own override - would
/// look identical to the thing being refused, since the merged document
/// would carry the key either way. Checking the committed document alone,
/// before the override is even read, keeps the two apart: the committed
/// file can never carry the key, full stop, and the override remains free
/// to set it precisely because it is the operator's own file and never came
/// from the repo.
///
/// # Errors
/// [`Error::Config`] naming the field and the app, if any app in `committed`
/// sets `user` or `group`.
fn refuse_repo_privilege(committed: &Value) -> Result<(), Error> {
    for app in apps(committed) {
        let Some(table) = app.as_table() else {
            continue;
        };
        for field in ["user", "group"] {
            if table.contains_key(field) {
                let name = table
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("<unnamed>");
                return Err(Error::Config(format!(
                    "Flockfile.toml sets `{field}` on app {name:?} - a committed Flockfile can \
                     never set user or group, on any app, because privilege is the one thing a \
                     compromised build cannot escalate to on its own. Pin `{field}` in \
                     Flockfile.override.toml instead - it is gitignored and never comes from the \
                     repo."
                )));
            }
        }
    }
    Ok(())
}

/// Refuses a committed Flockfile that declares one app name twice.
///
/// Two entries named `web` are two answers to "which one is the sheep", and
/// every reader here has to pick the same one. `select_app` takes the first;
/// `merge_apps` used to index by name with the last winning, so the
/// operator's override, `user` pin included, was merged onto an entry
/// nobody then read. Refusing the shape is simpler than making every reader
/// agree, and shep itself would not accept the file either.
///
/// # Errors
/// [`Error::Config`] naming the app, if any name appears more than once.
fn refuse_duplicate_apps(committed: &Value) -> Result<(), Error> {
    let mut seen = std::collections::BTreeSet::new();
    for app in apps(committed) {
        if let Some(name) = app_name(app)
            && !seen.insert(name)
        {
            return Err(Error::Config(format!(
                "Flockfile.toml declares an app named {:?} more than once; one entry per name",
                printable(name)
            )));
        }
    }
    Ok(())
}

/// `doc`'s `[[app]]` array, or an empty slice if `doc` is not a table or has
/// no `app` key at all - an empty Flockfile has no apps to iterate rather
/// than being a shape error at this layer.
fn apps(doc: &Value) -> &[Value] {
    doc.as_table()
        .and_then(|table| table.get("app"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Deep merges `over` onto `base`, `over` winning on every key it names.
///
/// Two tables merge key by key, recursing into any key both sides declare
/// as a table so a partial override only replaces the fields it actually
/// names - an override setting only `script` leaves every other field of
/// that app, and every other app, untouched. The `app` key is special-cased
/// to [`merge_apps`] rather than falling into that same table-of-tables
/// recursion, because it is not a table keyed by name - it is an array, and
/// the two Flockfiles are under no obligation to list their apps in the
/// same order. Anything else - two scalars, or a table on one side and a
/// scalar on the other - has `over` win outright, which is the base case
/// every recursive branch above eventually bottoms out at.
fn deep_merge(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Table(mut base), Value::Table(over)) => {
            for (key, over_value) in over {
                let merged = if key == "app" {
                    merge_apps(
                        base.remove(&key).unwrap_or(Value::Array(Vec::new())),
                        over_value,
                    )
                } else {
                    match base.remove(&key) {
                        Some(base_value) => deep_merge(base_value, over_value),
                        None => over_value,
                    }
                };
                base.insert(key, merged);
            }
            Value::Table(base)
        }
        (_, over) => over,
    }
}

/// Merges two `[[app]]` arrays by matching each entry's `name`, not its
/// position.
///
/// An override entry whose `name` matches a committed entry deep-merges
/// onto it in place, at the committed entry's own position, so a partial
/// override still leaves every field it does not name untouched - the same
/// property [`deep_merge`] gives every other table in the document. An
/// override entry whose `name` matches nothing in `base` is appended as a
/// new app. Position carries no meaning: the committed file and the
/// override are free to list the same apps in different orders and still
/// merge onto the same entries.
fn merge_apps(base: Value, over: Value) -> Value {
    let mut base_apps = match base {
        Value::Array(apps) => apps,
        _ => Vec::new(),
    };
    let over_apps = match over {
        Value::Array(apps) => apps,
        _ => Vec::new(),
    };

    // Indexed once rather than scanned per override entry. Both arrays come
    // from the repository, and a scan per entry made the merge quadratic in
    // files nothing else bounds the length of.
    //
    // First wins on a repeated name, the way `select_app` reads the array.
    // `collect` into a map keeps the LAST, which put an override onto an
    // entry nothing then read; `refuse_duplicate_apps` stops the shape at the
    // door, and this keeps the two readers agreeing even so.
    let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
    for (index, app) in base_apps.iter().enumerate() {
        if let Some(name) = app_name(app) {
            by_name.entry(name.to_owned()).or_insert(index);
        }
    }

    for over_app in over_apps {
        let existing = app_name(&over_app).and_then(|name| by_name.get(name).copied());

        match existing {
            Some(index) => {
                let base_app = std::mem::replace(&mut base_apps[index], Value::Table(Table::new()));
                base_apps[index] = deep_merge(base_app, over_app);
            }
            None => {
                if let Some(name) = app_name(&over_app) {
                    by_name.entry(name.to_owned()).or_insert(base_apps.len());
                }
                base_apps.push(over_app);
            }
        }
    }

    Value::Array(base_apps)
}

/// `app`'s `name` field, if it has one and it is a string.
fn app_name(app: &Value) -> Option<&str> {
    app.as_table()?.get("name")?.as_str()
}

/// The merged document's app entry named `sheep`.
///
/// # Errors
/// [`Error::Config`] if no app in `merged` is named `sheep`.
fn select_app(merged: &Value, sheep: &str) -> Result<Value, Error> {
    apps(merged)
        .iter()
        .find(|app| app_name(app) == Some(sheep))
        .cloned()
        .ok_or_else(|| Error::Config(format!("no app named {sheep:?} in Flockfile.toml")))
}

#[cfg(test)]
mod tests {
    /// fails if a committed Flockfile that is a symlink is read at all.
    ///
    /// `git worktree add` materialises a committed symlink exactly as
    /// committed, and this read runs at the dog's own uid before any build
    /// drops privilege. `Flockfile.toml -> <any file root can read>` then had
    /// the parser fail on that file and quote its first line into the log.
    /// The assertion is on the secret NOT being in the message: an error
    /// alone is what the vulnerable version returned too, after reading it.
    #[test]
    fn a_committed_flockfile_that_is_a_symlink_is_refused_unread() {
        let rel = tempfile::tempdir().expect("tempdir");
        let secret = rel.path().join("outside.txt");
        std::fs::write(&secret, "-----BEGIN SECRET-----\nnot toml\n").expect("secret");
        std::os::unix::fs::symlink(&secret, rel.path().join("Flockfile.toml")).expect("link");

        let err = app_config(rel.path(), "web", &[]).expect_err("a link must be refused");

        let shown = err.to_string();
        assert!(matches!(err, Error::Config(_)), "{shown}");
        assert!(shown.contains("symlink"), "must say why: {shown}");
        assert!(
            !shown.contains("SECRET"),
            "must not quote the file: {shown}"
        );
    }

    /// fails if a committed Flockfile naming one app twice is accepted.
    ///
    /// Found in round two of the review: the indexed merge kept the LAST
    /// entry for a repeated name while `select_app` read the first, so the
    /// operator's override, `user` pin included, landed on an entry nobody
    /// read and the sheep started at the dog's own uid. Refused by name now,
    /// and the index keeps the first regardless.
    #[test]
    fn a_committed_flockfile_naming_an_app_twice_is_refused() {
        let rel = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./a\"\n[[app]]\nname = \"web\"\nscript = \"./b\"\n",
        )
        .expect("committed");

        let err = app_config(rel.path(), "web", &[]).expect_err("two apps named web");
        let shown = err.to_string();
        assert!(
            shown.contains("web") && shown.contains("more than once"),
            "{shown}"
        );
    }

    /// fails if a message quotes a repository-chosen key with its control
    /// characters intact. serde names an unknown field in its message, and
    /// a TOML key can be any quoted string, so a committed
    /// `"\u001b[2J" = 1` used to write a terminal escape into the log.
    #[test]
    fn a_parser_message_never_carries_a_control_character() {
        let rel = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./a\"\n[dog.deploy.build]\n\"a\\u001bb\" = 1\n",
        )
        .expect("committed");

        let err = build_spec(rel.path(), &[]).expect_err("an unknown build key");
        let shown = err.to_string();
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert!(
            shown.contains("a?b"),
            "the key is still recognisable: {shown}"
        );
    }

    /// fails if the operator's own override stops resolving through the
    /// symlink `shared::link_into` made for it. Refusing every link would
    /// have broken the one legitimate case, which is the whole mechanism.
    #[test]
    fn the_operators_override_is_still_followed() {
        let rel = tempfile::tempdir().expect("tempdir");
        let checkout = tempfile::tempdir().expect("checkout");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\n",
        )
        .expect("committed");
        std::fs::write(
            checkout.path().join(OVERRIDE),
            "[[app]]\nname = \"web\"\nuser = \"svc\"\n",
        )
        .expect("the operator's override");
        std::os::unix::fs::symlink(checkout.path().join(OVERRIDE), rel.path().join(OVERRIDE))
            .expect("linked in, as link_into does");

        let app = app_config(rel.path(), "web", &[PathBuf::from(OVERRIDE)])
            .expect("the operator's link resolves");
        assert_eq!(app.user.as_deref(), Some("svc"));
    }

    /// fails if a parse error quotes the offending line. The read refuses a
    /// symlink so no file on the host can reach the parser, and this is the
    /// second wall: even a file that does reach it is described by line
    /// number, not by content.
    #[test]
    fn a_parse_error_names_the_line_and_not_its_text() {
        let rel = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\n-----BEGIN OPENSSH PRIVATE KEY-----\n",
        )
        .expect("committed");

        let err = app_config(rel.path(), "web", &[]).expect_err("not toml");

        let shown = err.to_string();
        assert!(shown.contains("line 3"), "must name the line: {shown}");
        assert!(!shown.contains("OPENSSH"), "must not quote it: {shown}");
    }

    /// fails if a Flockfile of any size is read whole. Both files come from
    /// the repository and are merged in memory on every deploy, with the
    /// tree's lock held.
    #[test]
    fn a_flockfile_past_the_size_limit_is_refused() {
        let rel = tempfile::tempdir().expect("tempdir");
        let mut big = String::from("[[app]]\nname = \"web\"\nscript = \"./run\"\n");
        big.push_str(&"# padding\n".repeat(120_000));
        assert!(
            big.len() as u64 > MAX_FLOCKFILE_BYTES,
            "the fixture must exceed the limit"
        );
        std::fs::write(rel.path().join("Flockfile.toml"), big).expect("committed");

        let err = app_config(rel.path(), "web", &[]).expect_err("too big");
        assert!(err.to_string().contains("limit"), "{err}");
    }

    /// fails if a committed override can grant a unix user.
    ///
    /// The override is exempt from `refuse_repo_privilege` because it is the
    /// operator's file. Nothing checked that it was, and the exemption is
    /// worthless without the check: measured 2026-08-28, a repository that
    /// simply committed `Flockfile.override.toml` had `user = "root"`
    /// honoured, straight past the boundary this module's own doc calls the
    /// one real one.
    ///
    /// `link_into` refuses when something is already in the way, so a
    /// committed file cannot displace an override the operator does share.
    /// This is the case it cannot cover: an operator with no override at all,
    /// where nothing collides and the committed file simply arrives.
    #[test]
    fn a_committed_override_cannot_grant_a_user() {
        let rel = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\n",
        )
        .expect("committed");
        std::fs::write(
            rel.path().join("Flockfile.override.toml"),
            "[[app]]\nname = \"web\"\nuser = \"root\"\n",
        )
        .expect("committed override");

        let err = app_config(rel.path(), "web", &[])
            .expect_err("a committed override must not grant a user");
        assert!(
            format!("{err}").contains("user"),
            "the refusal must name the field: {err}"
        );
    }

    /// fails if a repository can buy the exemption with a symlink out.
    ///
    /// The escape the previous version of `is_operators` allowed, in two
    /// commits and needing nothing from the operator. Release A ships an
    /// ordinary tracked file holding `user = "root"` and goes live, so
    /// `current` points at it. Release B commits `Flockfile.override.toml` as
    /// a symlink to `../../current/.deploy-payload.toml`, which resolves into
    /// release A. That is outside release B, so the old "resolves outside the
    /// release" test passed and the refusal was skipped.
    ///
    /// It is the reason provenance now comes from the share list rather than
    /// from the filesystem: whoever can write the tree can write the evidence.
    /// Since 2026-09-03 the link is refused before its target is read at all,
    /// because a committed override is read without following symlinks; the
    /// privilege refusal behind it is pinned by
    /// `a_committed_override_cannot_grant_a_user`.
    #[test]
    fn a_committed_symlink_out_of_the_release_buys_no_exemption() {
        let tree = tempfile::tempdir().expect("tempdir");
        let a = tree.path().join("releases/shaA");
        let b = tree.path().join("releases/shaB");
        std::fs::create_dir_all(&a).expect("a");
        std::fs::create_dir_all(&b).expect("b");

        std::fs::write(
            a.join(".deploy-payload.toml"),
            "[[app]]\nname = \"web\"\nuser = \"root\"\n",
        )
        .expect("payload");
        std::os::unix::fs::symlink(&a, tree.path().join("current")).expect("current");

        std::fs::write(
            b.join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\n",
        )
        .expect("committed");
        std::os::unix::fs::symlink(
            "../../current/.deploy-payload.toml",
            b.join("Flockfile.override.toml"),
        )
        .expect("committed symlink");

        // Nothing was shared, so nothing is the operator's.
        let err =
            app_config(&b, "web", &[]).expect_err("a committed symlink must not buy the exemption");
        let shown = format!("{err}");
        assert!(
            shown.contains("symlink"),
            "the link is refused before it is followed: {shown}"
        );
        assert!(
            !shown.contains("root"),
            "and nothing of the payload is read: {shown}"
        );
    }

    /// fails if the operator's own override stops being able to pin a user.
    ///
    /// The counterpart. Theirs reaches a release as a symlink into their
    /// checkout, so it resolves outside the release and keeps the exemption,
    /// which is the whole "pin it so upstream cannot change it" mechanism.
    #[test]
    fn the_operators_own_override_still_pins_a_user() {
        let rel = tempfile::tempdir().expect("tempdir");
        let checkout = tempfile::tempdir().expect("checkout");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\n",
        )
        .expect("committed");
        let theirs = checkout.path().join("Flockfile.override.toml");
        std::fs::write(&theirs, "[[app]]\nname = \"web\"\nuser = \"svc\"\n").expect("theirs");
        std::os::unix::fs::symlink(&theirs, rel.path().join("Flockfile.override.toml"))
            .expect("shared in");

        // The share list is what makes it theirs, exactly as `link_into`
        // would have reported it.
        let shared = [PathBuf::from("Flockfile.override.toml")];
        let app =
            app_config(rel.path(), "web", &shared).expect("the operator's override is honoured");
        assert_eq!(app.user.as_deref(), Some("svc"));
    }

    use crate::fixtures;

    use super::*;

    /// fails if the override stops winning. The override is the user's file
    /// and the committed one is upstream's; a user who pins script must not
    /// have it changed underneath them by a pull.
    #[test]
    fn the_override_wins_on_merge() {
        let rel = fixtures::fixture_release(&[
            (
                "Flockfile.toml",
                "[[app]]\nname='web'\nscript='upstream.js'\n",
            ),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='web'\nscript='mine.js'\n",
            ),
        ]);
        let app = app_config(rel.path(), "web", &[]).expect("merges");
        assert_eq!(app.script, "mine.js");
    }

    /// fails if a committed Flockfile can set the user a process runs as.
    /// Privilege is not a recommendation, and it is the one thing a
    /// compromised build genuinely cannot escalate on its own. This refusal
    /// is the boundary.
    #[test]
    fn a_committed_flockfile_cannot_set_user() {
        let rel = fixtures::fixture_release(&[(
            "Flockfile.toml",
            "[[app]]\nname='web'\nscript='x.js'\nuser='root'\n",
        )]);
        let err = app_config(rel.path(), "web", &[]).expect_err("refuses");
        assert!(err.to_string().contains("user"));
    }

    /// fails if `group` is not checked as its own clause. `user` and `group`
    /// are two separate `contains_key` checks in `refuse_repo_privilege`,
    /// not one - the test above only proves the first field is guarded, and
    /// a version of this function that checked `user` and forgot `group`
    /// would still pass it.
    #[test]
    fn a_committed_flockfile_cannot_set_group() {
        let rel = fixtures::fixture_release(&[(
            "Flockfile.toml",
            "[[app]]\nname='web'\nscript='x.js'\ngroup='wheel'\n",
        )]);
        let err = app_config(rel.path(), "web", &[]).expect_err("refuses");
        assert!(err.to_string().contains("group"));
    }

    /// fails if the presence of an override makes the committed-file
    /// refusal disappear. The refusal must fire because it was decided by
    /// reading the committed file alone, before the override was opened.
    ///
    /// The override deliberately does NOT name `user`. It did, set to
    /// `nobody`, on the theory that a laundered merge would be the shape
    /// worth catching. That made the test vacuous: `shared` is empty here, so
    /// `is_operators` is false, so `merged_document` runs
    /// `refuse_repo_privilege` against the override document too, and THAT
    /// check produced the error being asserted on. Deleting the
    /// committed-document check left the test passing. Found in round 8 of
    /// the founder's review, by deleting that line and re-running.
    ///
    /// `script` is the right field precisely because nothing refuses it, so
    /// the only remaining route to an error is the check this test is named
    /// for. `a_committed_flockfile_cannot_set_user` covers the other one.
    #[test]
    fn an_override_present_does_not_launder_a_committed_user_field() {
        let rel = fixtures::fixture_release(&[
            (
                "Flockfile.toml",
                "[[app]]\nname='web'\nscript='x.js'\nuser='root'\n",
            ),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='web'\nscript='mine.js'\n",
            ),
        ]);
        let err = app_config(rel.path(), "web", &[]).expect_err("still refuses");
        assert!(err.to_string().contains("user"));
    }

    /// fails if apps stop merging by `name` and fall back to merging by
    /// position instead. The committed file and the override list the same
    /// two apps in opposite orders; a positional merge would splice `web`'s
    /// committed entry with `worker`'s override entry (and vice versa),
    /// producing a merged app with a mismatched name/script pairing instead
    /// of leaving each app's own fields untouched aside from what its own
    /// override actually names.
    #[test]
    fn apps_in_different_files_merge_by_name_not_position() {
        let rel = fixtures::fixture_release(&[
            (
                "Flockfile.toml",
                "[[app]]\nname='web'\nscript='web.js'\n\n[[app]]\nname='worker'\nscript='worker.js'\n",
            ),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='worker'\nscript='worker-mine.js'\n\n[[app]]\nname='web'\nscript='web-mine.js'\n",
            ),
        ]);
        let web = app_config(rel.path(), "web", &[]).expect("merges web");
        let worker = app_config(rel.path(), "worker", &[]).expect("merges worker");
        assert_eq!(web.script, "web-mine.js");
        assert_eq!(worker.script, "worker-mine.js");
    }

    /// fails if `merge_apps` stops appending an override app that names
    /// nothing in the committed file - the other half of "matched by name":
    /// a name match merges in place, and a name miss must still make the
    /// app available rather than being silently dropped.
    #[test]
    fn an_override_can_add_a_new_app_not_in_the_committed_file() {
        let rel = fixtures::fixture_release(&[
            ("Flockfile.toml", "[[app]]\nname='web'\nscript='web.js'\n"),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='sidecar'\nscript='sidecar.js'\n",
            ),
        ]);
        let sidecar =
            app_config(rel.path(), "sidecar", &[]).expect("the override's own app merges");
        assert_eq!(sidecar.script, "sidecar.js");
    }

    /// fails if asking for an app nobody declared silently produces
    /// something instead of a named refusal.
    #[test]
    fn app_config_refuses_an_unknown_sheep_name() {
        let rel = fixtures::fixture_release(&[(
            "Flockfile.toml",
            "[[app]]\nname='web'\nscript='x.js'\n",
        )]);
        let err = app_config(rel.path(), "ghost", &[]).expect_err("no such app");
        assert!(err.to_string().contains("ghost"));
    }

    /// fails if the old top-level `[build]` spelling is silently ignored.
    ///
    /// It has to be refused rather than skipped. Skipping it builds nothing
    /// and says nothing: the release is swapped in unbuilt and reported as
    /// deployed. And whoever meets this is following this crate's own
    /// published README, which documented `[build]` while that spelling made
    /// the Flockfile unregisterable with `shep start`.
    #[test]
    fn the_old_top_level_build_block_is_refused_by_name() {
        let rel = fixtures::fixture_release(&[(
            "Flockfile.toml",
            "[[app]]\nname='web'\nscript='x'\n\n[build]\ncommand = 'make build'\n",
        )]);

        let err = build_spec(rel.path(), &[]).expect_err("the old spelling must be refused");

        let text = format!("{err}");
        assert!(
            text.contains("[dog.deploy.build]"),
            "must name the new home: {text}"
        );
        assert!(text.contains("shep start"), "must say why it moved: {text}");
    }

    /// fails if a declared `[dog.deploy.build]` block does not reach the build step.
    /// `env` and `artifacts` both matter to a real build - a pinned
    /// registry token, and a binary the release can't see the build
    /// producing - so a block that parsed its command and dropped either
    /// of those would still break rollback.
    #[test]
    fn a_build_block_parses_into_a_spec() {
        let rel = fixtures::fixture_release(&[(
            "Flockfile.toml",
            "[[app]]\nname='web'\nscript='x'\n\n[dog.deploy.build]\ncommand = 'make build'\nenv = {              CARGO_TARGET_DIR = '/srv/cache' }\nartifacts = ['target/release/koji']\n",
        )]);
        let spec = build_spec(rel.path(), &[]).expect("parses");
        assert_eq!(spec.command.as_deref(), Some("make build"));
        assert_eq!(
            spec.env.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("/srv/cache")
        );
        assert_eq!(
            spec.artifacts,
            vec![std::path::PathBuf::from("target/release/koji")]
        );
    }

    /// fails if a Flockfile with no `[dog.deploy.build]` block becomes an error
    /// rather than the no-op spec. ReactMap run as `bun .` declares no
    /// build at all, which is one of the three worked examples this design
    /// has to cover.
    #[test]
    fn an_absent_build_block_is_the_default_spec() {
        let rel =
            fixtures::fixture_release(&[("Flockfile.toml", "[[app]]\nname='web'\nscript='x'\n")]);
        assert_eq!(
            build_spec(rel.path(), &[]).expect("parses"),
            BuildSpec::default()
        );
    }

    /// fails if the operator's override stops winning on the build block.
    /// `build.env` names host-specific paths a committed file cannot know,
    /// so pinning them locally is the whole reason the override reaches
    /// this block at all.
    #[test]
    fn the_override_wins_on_the_build_block() {
        let rel = fixtures::fixture_release(&[
            (
                "Flockfile.toml",
                "[[app]]\nname='web'\nscript='x'\n\n[dog.deploy.build]\ncommand = 'make build'\n",
            ),
            (
                "Flockfile.override.toml",
                "[dog.deploy.build]\nenv = { CARGO_TARGET_DIR = '/srv/cache' }\n",
            ),
        ]);
        let spec = build_spec(rel.path(), &[]).expect("parses");
        assert_eq!(spec.command.as_deref(), Some("make build"));
        assert_eq!(
            spec.env.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("/srv/cache")
        );
    }

    /// fails if a typo in the build block is ignored instead of refused. A
    /// silently-dropped `commands` key means a build that never runs, and
    /// a deploy that swaps in a release nothing built.
    #[test]
    fn an_unknown_build_key_is_refused() {
        let rel = fixtures::fixture_release(&[(
            "Flockfile.toml",
            "[[app]]\nname='web'\nscript='x'\n\n[dog.deploy.build]\ncommands = 'make build'\n",
        )]);
        let err = build_spec(rel.path(), &[]).expect_err("refuses");
        assert!(err.to_string().contains("commands"));
    }
}
