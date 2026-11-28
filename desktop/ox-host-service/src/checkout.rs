//! Transfer a worker checkout between hosts without touching the user's index,
//! stash or refs. The caller supplies the authenticated transport and the
//! receiving host's local mirror. The mirror refresh is the only upstream fetch.
mod files;
mod identity;
pub use identity::repository_id;
mod lfs;
pub use files::changed_private_inputs;
mod mirror;
pub mod results;
mod shallow;
mod submodules;
pub use mirror::{Mirror, negotiate};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::TempDir;

const MAGIC: &[u8] = b"OX checkout\0\x01";
const SNAPSHOT_REF: &str = "refs/heads/ox-snapshot";

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    // A host can itself have been started from a worktree or Git hook.
    for key in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_QUARANTINE_PATH",
    ] {
        cmd.env_remove(key);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .args([
            "--no-replace-objects",
            "-c",
            "core.hooksPath=/dev/null",
            "-C",
        ])
        .arg(dir)
        .stdin(Stdio::null());
    cmd
}
fn run(cmd: &mut Command) -> io::Result<Vec<u8>> {
    let output = cmd.output()?;
    if !output.status.success() {
        // Git diagnostics may contain a credentialed URL, a filename or filter output.
        let mut args = cmd.get_args().skip(5);
        let operation = loop {
            match args.next().and_then(OsStr::to_str) {
                Some("-c") => {
                    args.next();
                }
                Some(op) if !op.starts_with('-') => break op,
                _ => break "command",
            }
        };
        return Err(io::Error::other(format!(
            "Git {operation} failed ({})",
            output.status
        )));
    }
    Ok(output.stdout)
}
fn line(mut output: Vec<u8>) -> io::Result<OsString> {
    if output.pop() != Some(b'\n') {
        return Err(io::Error::other("Incomplete Git reply"));
    }
    Ok(OsString::from_vec(output))
}
fn git_path(dir: &Path, args: &[&str]) -> io::Result<PathBuf> {
    Ok(PathBuf::from(line(run(git(dir).args(args))?)?))
}
fn oid(id: &[u8]) -> io::Result<&str> {
    if matches!(id.len(), 40 | 64) && id.iter().all(u8::is_ascii_hexdigit) {
        return Ok(std::str::from_utf8(id).expect("hexadecimal ASCII"));
    }
    Err(io::Error::other("Invalid Git object ID"))
}
fn object(dir: &Path, name: &str) -> io::Result<String> {
    let id = line(run(git(dir).args(["rev-parse", "--verify", name]))?)?;
    Ok(oid(id.as_bytes())?.to_owned())
}
/// Canonical repository root and the project path captured inside it.
pub fn project_location(project: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let root = git_path(project, &["rev-parse", "--show-toplevel"])?;
    let relative = project
        .canonicalize()?
        .strip_prefix(&root)
        .map_err(|_| io::Error::other("Project moved outside its repository"))?
        .to_owned();
    Ok((root, relative))
}

fn head(repo: &Path) -> io::Result<Option<String>> {
    let out = git(repo)
        .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .output()?;
    if out.status.success() {
        return Ok(Some(oid(line(out.stdout)?.as_bytes())?.to_owned()));
    }
    let branch = line(run(git(repo).args(["symbolic-ref", "--quiet", "HEAD"]))?)?;
    if git(repo)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(branch)
        .status()?
        .code()
        == Some(1)
    {
        return Ok(None);
    }
    Err(io::Error::other("Cannot read the source HEAD commit"))
}
fn object_format(dir: &Path) -> io::Result<usize> {
    match line(run(git(dir).args(["rev-parse", "--show-object-format"]))?)?.as_bytes() {
        b"sha1" => Ok(40),
        b"sha256" => Ok(64),
        _ => Err(io::Error::other("Unsupported Git object format")),
    }
}
fn bare(dir: &Path, hash_size: usize) -> io::Result<()> {
    let format = if hash_size == 64 { "sha256" } else { "sha1" };
    run(git(dir)
        .args(["init", "--bare", "-q"])
        .arg(format!("--object-format={format}")))?;
    Ok(())
}
fn alternate(repo: &Path, objects: &Path) -> io::Result<()> {
    // Git alternates accept C-quoted paths. Octal also preserves embedded
    // newlines and non-UTF8 directory names without adding another alternate.
    let mut entry = String::from("\"");
    for &b in objects.as_os_str().as_bytes() {
        use std::fmt::Write;
        write!(entry, "\\{b:03o}").expect("string write");
    }
    entry.push_str("\"\n");
    fs::write(repo.join("objects/info/alternates"), entry)
}

fn index_version(path: &Path) -> io::Result<Option<(u64, u64, i64, i64)>> {
    match fs::metadata(path) {
        Ok(m) => Ok(Some((m.dev(), m.ino(), m.ctime(), m.ctime_nsec()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// A self-contained staged transfer, backed by a private file. Drop removes it.
pub struct Snapshot {
    _stage: TempDir,
    result_objects: Option<TempDir>,
    archive: PathBuf,
    pub commit: String,
    pub bundle_bytes: u64,
    pub untracked: Untracked,
    /// Working conflict files included without the source merge index.
    pub unresolved_paths: Vec<PathBuf>,
}
#[derive(Default)]
pub struct Untracked {
    pub files: u64,
    pub bytes: u64,
    /// Git reports an entire ignored directory as one entry.
    pub ignored_entries: u64,
    /// Nested repositories and OS special files that cannot be copied as ordinary files.
    pub skipped: Vec<PathBuf>,
}

impl Snapshot {
    /// `known` are commit IDs already present in the target's mirror.
    pub fn capture(staging: &Path, project: &Path, known: &[String]) -> io::Result<Self> {
        Self::capture_inner(staging, project, known, 0, None)
    }

    fn capture_inner(
        staging: &Path,
        project: &Path,
        known: &[String],
        depth: usize,
        returning: Option<results::Inputs<'_>>,
    ) -> io::Result<Self> {
        submodules::check_depth(depth)?;
        let (root, relative) = project_location(project)?;
        let base = head(&root)?;
        let index = git_path(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?;
        let objects = git_path(
            &root,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "objects",
            ],
        )?;
        let stage = tempfile::Builder::new()
            .prefix(".sending-")
            .tempdir_in(staging)?;
        let builder = tempfile::tempdir_in(stage.path())?;
        let repo = builder.path().join("repo");
        fs::create_dir(&repo)?;
        bare(&repo, object_format(&root)?)?;
        alternate(&repo, &objects)?;
        if let Some(inputs) = &returning {
            // Result capture runs on the host after the worker stops. Only the host's
            // own Git config/filter definitions are eligible to execute here.
            if !project.symlink_metadata()?.is_dir()
                || root != project.canonicalize()?
                || objects.join("info/alternates").try_exists()?
                || git_path(&root, &["rev-parse", "--absolute-git-dir"])? != root.join(".git")
                || objects.canonicalize()? != root.join(".git/objects")
                || index != root.join(".git/index")
            {
                return Err(io::Error::other(
                    "The worker checkout Git directory moved. Its files remain on the target.",
                ));
            }
            if let Some(head) = &base {
                run(git(&repo).args(["update-ref", "HEAD", head]))?;
            }
            let mut excludes = File::create(repo.join("info/exclude"))?;
            let exclude_path = root.join(".git/info/exclude");
            if exclude_path.try_exists()? {
                if exclude_path.canonicalize()? != exclude_path || !exclude_path.is_file() {
                    return Err(io::Error::other(
                        "Worker ignore rules must be a regular file in the checkout.",
                    ));
                }
                io::copy(&mut File::open(exclude_path)?, &mut excludes)?;
                writeln!(excludes)?;
            }
            for name in inputs.paths() {
                files::exclude(&mut excludes, name.as_os_str().as_bytes())?;
            }
        }
        let boundaries = shallow::prepare(&root, &repo)?;
        let private_index = builder.path().join("index");
        let before = index_version(&index)?;
        if before.is_some() {
            fs::copy(&index, &private_index)?;
        }
        let source = || {
            let mut cmd = git(&root);
            if returning.is_some() {
                cmd.arg("--git-dir")
                    .arg(&repo)
                    .arg("--work-tree")
                    .arg(&root);
            }
            cmd.env("GIT_INDEX_FILE", &private_index)
                .env("GIT_OBJECT_DIRECTORY", repo.join("objects"))
                .args(["-c", "core.splitIndex=false", "-c", "core.fsmonitor=false"]);
            cmd
        };
        if before.is_none()
            && let Some(base) = &base
        {
            run(source().args(["read-tree", base]))?;
        }
        // Start with the actual index, so staged new files stay tracked and
        // staged deletions that were recreated stay untracked.
        let unmerged = run(source().args(["ls-files", "--unmerged", "-z"]))?;
        let mut unresolved_paths = unmerged
            .split(|b| *b == 0)
            .filter_map(|row| {
                row.iter()
                    .position(|b| *b == b'\t')
                    .map(|tab| PathBuf::from(OsStr::from_bytes(&row[tab + 1..])))
            })
            .collect::<Vec<_>>();
        unresolved_paths.dedup();
        files::refresh_tracked_flags(&root, source)?;
        run(source().args(["add", "--update", "--sparse"]))?;
        let private = returning.as_ref().map(|r| r.paths()).unwrap_or_default();
        let modules =
            submodules::capture(&root, builder.path(), source, depth, returning.as_ref())?;
        let tree = line(run(source().arg("write-tree"))?)?;
        let commit = if let Some(base) = &base
            && tree.as_bytes() == object(&root, &format!("{base}^{{tree}}"))?.as_bytes()
        {
            base.clone()
        } else {
            let mut make = git(&repo);
            make.args([
                "-c",
                "user.name=OX",
                "-c",
                "user.email=ox@localhost",
                "commit-tree",
                "--no-gpg-sign",
            ])
            .arg(&tree);
            if let Some(parent) = &base {
                make.args(["-p", parent]);
            }
            let message = if returning.is_some() {
                "OX worker result"
            } else {
                "OX worker checkout"
            };
            let id = line(run(make.args(["-m", message]))?)?;
            oid(id.as_bytes())?.to_owned()
        };
        let bundle = builder.path().join("snapshot.bundle");
        let mut size = 0;
        if !known.contains(&commit) {
            run(git(&repo).args(["update-ref", SNAPSHOT_REF, &commit]))?;
            let mut revisions = tempfile::tempfile()?;
            writeln!(revisions, "{SNAPSHOT_REF}")?;
            for id in mirror::known_commits(&repo, known)? {
                writeln!(revisions, "^{id}")?;
            }
            use io::Seek;
            revisions.rewind()?;
            run(git(&repo)
                .args(["bundle", "create"])
                .arg(&bundle)
                .arg("--stdin")
                .stdin(revisions)).map_err(|_| io::Error::other("Git could not bundle the snapshot. For a shallow or partial clone, refresh the target mirror or fetch the missing source history and objects, then retry."))?;
            size = fs::metadata(&bundle)?.len();
        }
        let archive = stage.path().join("checkout");
        let mut output = io::BufWriter::new(File::create(&archive)?);
        output.write_all(MAGIC)?;
        files::write_field(&mut output, commit.as_bytes())?;
        files::write_field(&mut output, relative.as_os_str().as_bytes())?;
        output.write_all(&size.to_be_bytes())?;
        shallow::pack(&repo, &boundaries, &commit, &mut output)?;
        if size != 0 {
            io::copy(&mut File::open(&bundle)?, &mut output)?;
        }
        lfs::pack(&root, source, &mut output)?;
        for (name, module) in &modules {
            unresolved_paths.extend(module.unresolved_paths.iter().map(|path| name.join(path)));
        }
        let nested = submodules::pack(modules, &mut output)?;
        let mut paths = run(source().args(["ls-files", "-z", "--others", "--exclude-standard"]))?;
        if !private.is_empty() {
            let tracked = run(source().args(["ls-files", "-z", "--cached"]))?;
            let tracked: std::collections::HashSet<_> = tracked.split(|b| *b == 0).collect();
            let already: std::collections::HashSet<Vec<u8>> =
                paths.split(|b| *b == 0).map(<[u8]>::to_vec).collect();
            for path in private {
                let name = path.as_os_str().as_bytes();
                if !already.contains(name)
                    && !tracked.contains(name)
                    && root
                        .join(path)
                        .symlink_metadata()
                        .is_ok_and(|m| m.is_file() || m.is_symlink())
                {
                    paths.extend_from_slice(name);
                    paths.push(0);
                }
            }
        }
        let mut untracked = files::pack(&root, stage.path(), &paths, &mut output)?;
        let ignored = run(source().args([
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ]))?;
        untracked.ignored_entries = ignored
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty() && !root.join(OsStr::from_bytes(p)).starts_with(stage.path()))
            .count() as u64
            + nested.ignored_entries;
        untracked.files += nested.files;
        untracked.bytes += nested.bytes;
        untracked.skipped.extend(nested.skipped);
        output.flush()?;
        if head(&root)? != base || index_version(&index)? != before {
            return Err(io::Error::other(
                "The source branch or index changed during capture. Retry the launch.",
            ));
        }
        Ok(Self {
            _stage: stage,
            result_objects: returning.is_some().then_some(builder),
            archive,
            commit,
            bundle_bytes: size,
            untracked,
            unresolved_paths,
        })
    }

    /// Retain the already captured bytes in an owner-only queue on the same filesystem.
    pub fn retain_archive(&self, destination: &Path) -> io::Result<()> {
        self.open()?.sync_all()?;
        fs::hard_link(&self.archive, destination)
    }

    pub fn open(&self) -> io::Result<File> {
        File::open(&self.archive)
    }
}

/// The archive must physically fit on the receiving filesystem. Git may also
/// need space for the unpacked worktree; its normal write errors still apply.
pub fn check_archive_space(file: &File, bytes: u64) -> io::Result<()> {
    if let Ok(space) = rustix::fs::fstatvfs(file) {
        let unit = if space.f_frsize == 0 {
            space.f_bsize
        } else {
            space.f_frsize
        };
        let available = (space.f_bavail as u128) * (unit as u128);
        if available < u128::from(bytes) {
            return Err(io::Error::other(format!(
                "Worker checkout needs {bytes} archive bytes; only {available} bytes are available."
            )));
        }
    }
    Ok(())
}

/// Install into a fresh, owner-controlled job directory on the same device as
/// `jobs`. Only a fully prepared worktree is made visible at the final job path.
pub fn receive(jobs: &Path, job: u64, mirror: &Path, mut input: impl Read) -> io::Result<PathBuf> {
    receive_inner(
        jobs,
        job,
        Some(mirror),
        &mut input,
        0,
        None,
        Purpose::Worker,
    )
}

/// Receive an archive only if its embedded commit matches the announced snapshot.
pub fn receive_commit(
    jobs: &Path,
    job: u64,
    mirror: &Path,
    mut input: impl Read,
    commit: &str,
) -> io::Result<PathBuf> {
    receive_inner(
        jobs,
        job,
        Some(mirror),
        &mut input,
        0,
        Some(commit),
        Purpose::Worker,
    )
}

/// A result archive includes its own base history and never contacts an upstream.
pub fn receive_result(
    jobs: &Path,
    job: u64,
    mut input: impl Read,
    tip: &str,
) -> io::Result<PathBuf> {
    receive_inner(jobs, job, None, &mut input, 0, Some(tip), Purpose::Worker)
}

/// A moved session continues working on the copied files. Leave its untracked
/// files visible as ordinary Git inputs, including on later moves.
pub fn receive_move(jobs: &Path, job: u64, mut input: impl Read, tip: &str) -> io::Result<PathBuf> {
    receive_inner(
        jobs,
        job,
        None,
        &mut input,
        0,
        Some(tip),
        Purpose::SessionMove,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Worker,
    SessionMove,
}

fn receive_inner(
    jobs: &Path,
    job: u64,
    mirror: Option<&Path>,
    mut input: &mut dyn Read,
    depth: usize,
    expected: Option<&str>,
    purpose: Purpose,
) -> io::Result<PathBuf> {
    submodules::check_depth(depth)?;
    let mut magic = [0; MAGIC.len()];
    input.read_exact(&mut magic)?;
    if magic != MAGIC {
        return Err(io::Error::other("Unrecognized OX checkout"));
    }
    let commit = files::read_field(&mut input)?;
    let commit = oid(&commit)?;
    if expected.is_some_and(|id| id != commit) {
        return Err(io::Error::other(
            "Checkout commit differs from the announced snapshot.",
        ));
    }
    let relative = files::read_field(&mut input)?;
    let relative = Path::new(OsStr::from_bytes(&relative));
    if !relative.as_os_str().is_empty() {
        files::validate(relative)?;
    }
    let mut size = [0; 8];
    input.read_exact(&mut size)?;
    let size = u64::from_be_bytes(size);
    let stage = tempfile::Builder::new()
        .prefix(".preparing-")
        .tempdir_in(jobs)?;
    let work = stage.path().join("work");
    let mirror = mirror.map(Path::canonicalize).transpose()?;
    if let Some(mirror) = &mirror {
        // Local clone hard-links immutable objects so pruning the mirror does
        // not strand workers. No remote URL is accepted from the source.
        run(git(stage.path())
            .args(["clone", "--local", "--no-checkout", "--no-tags", "--"])
            .arg(mirror)
            .arg(&work))?;
        run(git(&work).args(["remote", "remove", "origin"]))?;
        mirror::copy_tracking_refs(mirror, &work)?;
    } else {
        // Populated submodules carry a full snapshot. Never follow URLs from
        // the transferred .gitmodules or inherit credentials from its source.
        fs::create_dir(&work)?;
        let format = if commit.len() == 64 { "sha256" } else { "sha1" };
        run(git(&work)
            .args(["init", "-q"])
            .arg(format!("--object-format={format}")))?;
    }
    shallow::unpack(&work, &mut input, commit.len())?;
    if size != 0 {
        let bundle = stage.path().join("snapshot.bundle");
        files::copy_exact(&mut input, &mut File::create(&bundle)?, size)?;
        let heads = run(git(&work).args(["bundle", "list-heads"]).arg(&bundle))?;
        if heads != format!("{commit} {SNAPSHOT_REF}\n").as_bytes() {
            return Err(io::Error::other("Git bundle does not match this checkout"));
        }
        run(git(&work).args(["bundle", "verify"]).arg(&bundle)).map_err(|_| {
            io::Error::other(
                "The target cannot verify the bundle prerequisites. Retry with a full checkout.",
            )
        })?;
        run(git(&work).args(["bundle", "unbundle"]).arg(&bundle))?;
        fs::remove_file(bundle)?;
    }
    // This job's .git/info/exclude is private. Existing checkouts, other jobs
    // and the mirror keep their own Git ignore rules.
    run(git(&work).env("GIT_LFS_SKIP_SMUDGE", "1").args([
        "checkout",
        "-q",
        "-B",
        &format!("ox/{job}"),
        commit,
    ]))?;
    // A user-supplied mirror may itself borrow objects from a different
    // repository. Materialize that exceptional case before dropping its link.
    let alternates = work.join(".git/objects/info/alternates");
    if alternates.try_exists()? {
        run(git(&work).args(["repack", "-a", "-d"]))?;
        fs::remove_file(alternates)?;
    }
    lfs::unpack(&work, &mut input)?;
    let modules = submodules::unpack(&work, stage.path(), job, &mut input, depth, purpose)?;
    files::unpack(&work, &mut input, purpose == Purpose::Worker)?;
    for (prepared, path) in modules {
        // rename accepts an empty directory and fails on any transferred
        // parent-level inputs that would collide with the populated module.
        fs::rename(prepared, work.join(path))?;
    }
    if let Some(mirror) = &mirror {
        let upstream = git(mirror)
            .args(["config", "--get", "remote.origin.url"])
            .output()?;
        if upstream.status.success() {
            run(git(&work)
                .args(["remote", "add", "origin"])
                .arg(mirror::local_origin(&line(upstream.stdout)?, mirror)))?;
        } else if upstream.status.code() != Some(1) {
            return Err(io::Error::other("Cannot read the mirror's upstream"));
        }
    }
    // Git does not record empty or ignored-only directories, including a
    // legitimate selected project inside a larger repo.
    if !relative.as_os_str().is_empty() {
        files::Tree::open(&work)?.parent(&relative.join(".ox-directory"), true)?;
    }
    let destination = jobs.join(job.to_string());
    // A published job directory is always non-empty. rename cannot replace a
    // previous successful job, including one installed concurrently.
    fs::rename(&work, &destination)?;
    Ok(destination.join(relative))
}

#[cfg(test)]
mod tests;
