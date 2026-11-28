//! An initialized submodule is a separate working repository. Transfer its
//! local state from disk without fetching from the source's .gitmodules URL.
use super::{Purpose, Snapshot, Untracked, files, git, git_path, object, oid, receive_inner, run};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
};

pub(super) fn check_depth(depth: usize) -> io::Result<()> {
    if depth > 64 {
        return Err(io::Error::other(
            "Submodule nesting exceeds 64 repositories",
        ));
    }
    Ok(())
}

pub(super) fn gitlinks(cmd: &mut Command) -> io::Result<BTreeMap<PathBuf, String>> {
    let index = run(cmd.args(["ls-files", "--stage", "-z"]))?;
    let mut links = BTreeMap::new();
    for entry in index.split(|b| *b == 0) {
        let Some(entry) = entry.strip_prefix(b"160000 ") else {
            continue;
        };
        let (header, path) = entry.split_at(
            entry
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(|| io::Error::other("Invalid gitlink entry"))?,
        );
        let sha = header.strip_suffix(b" 0").ok_or_else(|| {
            io::Error::other("Resolve the submodule index conflict before transfer")
        })?;
        let path = Path::new(OsStr::from_bytes(&path[1..]));
        files::validate(path)?;
        links.insert(path.to_owned(), oid(sha)?.to_owned());
    }
    Ok(links)
}

pub(super) fn capture(
    root: &Path,
    staging: &Path,
    source: impl Fn() -> Command,
    depth: usize,
    returning: Option<&super::results::Inputs<'_>>,
) -> io::Result<Vec<(PathBuf, Snapshot)>> {
    let mut modules = Vec::new();
    let tree = files::Tree::open(root)?;
    for path in gitlinks(&mut source())?.into_keys() {
        let checkout = root.join(&path);
        match fs::symlink_metadata(&checkout) {
            Ok(meta) if meta.is_dir() => (),
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
            _ => {
                return Err(io::Error::other(
                    "A submodule checkout is no longer a directory",
                ));
            }
        }
        tree.parent(&path.join(".ox-directory"), false)?;
        if fs::read_dir(&checkout)?.next().transpose()?.is_none() {
            continue;
        }
        if !checkout.join(".git").try_exists()?
            || git_path(&checkout, &["rev-parse", "--show-toplevel"])? != checkout
        {
            return Err(io::Error::other(
                "A populated submodule has no working Git checkout. Restore it or move its files before transfer.",
            ));
        }
        let packet = Snapshot::capture_inner(
            staging,
            &checkout,
            &[],
            depth + 1,
            returning.map(|r| r.child(&path)),
        )
        .map_err(|e| io::Error::new(e.kind(), format!("Submodule {path:?}: {e}")))?;
        run(source()
            .args(["update-index", "--cacheinfo", "160000", &packet.commit])
            .arg(&path))?;
        modules.push((path, packet));
    }
    Ok(modules)
}

pub(super) fn pack(
    modules: Vec<(PathBuf, Snapshot)>,
    output: &mut impl Write,
) -> io::Result<Untracked> {
    let mut summary = Untracked::default();
    for (path, packet) in modules {
        files::write_field(output, path.as_os_str().as_bytes())?;
        let mut stream = packet.open()?;
        output.write_all(&stream.metadata()?.len().to_be_bytes())?;
        io::copy(&mut stream, output)?;
        summary.ignored_entries += packet.untracked.ignored_entries;
        summary.files += packet.untracked.files;
        summary.bytes += packet.untracked.bytes;
        summary.skipped.extend(
            packet
                .untracked
                .skipped
                .iter()
                .map(|subpath| path.join(subpath)),
        );
    }
    files::write_field(output, &[])?;
    Ok(summary)
}

pub(super) fn unpack(
    root: &Path,
    staging: &Path,
    job: u64,
    input: &mut impl Read,
    depth: usize,
    purpose: Purpose,
) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let tree = files::Tree::open(root)?;
    let mut links = gitlinks(&mut git(root))?;
    let mut prepared = Vec::new();
    loop {
        let bytes = files::read_field(input)?;
        if bytes.is_empty() {
            return Ok(prepared);
        }
        let path = Path::new(OsStr::from_bytes(&bytes));
        let expected = links
            .remove(path)
            .ok_or_else(|| io::Error::other("Unexpected or duplicate submodule in checkout"))?;
        tree.parent(&path.join(".ox-directory"), false)?;
        let mut size = [0; 8];
        input.read_exact(&mut size)?;
        let mut child = input.take(u64::from_be_bytes(size));
        let staging = staging.join(format!("module-{}", prepared.len()));
        fs::create_dir(&staging)?;
        let checkout = receive_inner(&staging, job, None, &mut child, depth + 1, None, purpose)?;
        if child.limit() != 0
            || checkout != staging.join(job.to_string())
            || object(&checkout, "HEAD")? != expected
        {
            return Err(io::Error::other(
                "Submodule snapshot does not match the parent gitlink",
            ));
        }
        prepared.push((checkout, path.to_owned()));
    }
}
