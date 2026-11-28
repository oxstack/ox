//! The untracked portion is a path, mode and byte stream per file. Neither
//! side follows symlinks when traversing a working tree.
use super::{Untracked, git, run};
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, fstat, mkdirat, openat, readlinkat, statat, symlinkat,
};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read, Seek, Write},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::Path,
};

pub(super) fn validate(path: &Path) -> io::Result<()> {
    if path.as_os_str().as_bytes().split(|b| *b == b'/').any(|p| {
        p.is_empty() || p.contains(&0) || p == b"." || p == b".." || p.eq_ignore_ascii_case(b".git")
    }) {
        return Err(io::Error::other("Invalid path in OX checkout"));
    }
    Ok(())
}
pub(super) fn write_field(w: &mut impl Write, field: &[u8]) -> io::Result<()> {
    let len: u32 = field
        .len()
        .try_into()
        .map_err(|_| io::Error::other("Checkout path is too long"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(field)
}
pub(super) fn read_field(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    // Both supported host operating systems limit paths to a few KiB. Also
    // applies to symlink targets, never to file contents.
    if len > 65536 {
        return Err(io::Error::other("Checkout path is too long"));
    }
    let mut data = vec![0; len];
    r.read_exact(&mut data)?;
    Ok(data)
}
pub(super) fn copy_exact(r: &mut impl Read, w: &mut impl Write, len: u64) -> io::Result<()> {
    if io::copy(&mut r.take(len), w)? != len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Incomplete OX checkout",
        ));
    }
    Ok(())
}

pub(super) struct Tree {
    root: File,
    gitdir: rustix::fs::Stat,
}
impl Tree {
    pub(super) fn open(root: &Path) -> io::Result<Self> {
        let root = File::open(root)?;
        let gitdir = statat(&root, ".git", AtFlags::SYMLINK_NOFOLLOW)?;
        Ok(Self { root, gitdir })
    }
    pub(super) fn parent<'a>(&self, path: &'a Path, create: bool) -> io::Result<(File, &'a OsStr)> {
        validate(path)?;
        let mut dir = self.root.try_clone()?;
        for part in path.parent().expect("validated path").iter() {
            if create
                && let Err(e) = mkdirat(&dir, part, Mode::from_raw_mode(0o700))
                && e != rustix::io::Errno::EXIST
            {
                return Err(e.into());
            }
            dir = openat(
                &dir,
                part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?
            .into();
            let info = fstat(&dir)?;
            if self.gitdir.st_dev == info.st_dev && self.gitdir.st_ino == info.st_ino {
                return Err(io::Error::other(
                    "A checkout path resolves inside Git metadata",
                ));
            }
        }
        Ok((dir, path.file_name().expect("validated filename")))
    }
}

/// Preserve absent sparse paths from the index, but snapshot edits that are
/// physically present even when the developer has set assume-unchanged.
pub(super) fn refresh_tracked_flags(
    root: &Path,
    source: impl Fn() -> std::process::Command,
) -> io::Result<()> {
    let listing = run(source().args(["ls-files", "-v", "-z"]))?;
    let mut assumed = tempfile::tempfile()?;
    let mut sparse = tempfile::tempfile()?;
    for entry in listing.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let Some((&tag, rest)) = entry.split_first() else {
            continue;
        };
        let bytes = rest
            .strip_prefix(b" ")
            .ok_or_else(|| io::Error::other("Invalid tracked path record"))?;
        if tag.is_ascii_lowercase() {
            assumed.write_all(bytes)?;
            assumed.write_all(&[0])?;
        }
        if tag.eq_ignore_ascii_case(&b'S') {
            let path = Path::new(OsStr::from_bytes(bytes));
            match root.join(path).symlink_metadata() {
                Ok(_) => {
                    sparse.write_all(bytes)?;
                    sparse.write_all(&[0])?;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => (),
                Err(err) => return Err(err),
            }
        }
    }
    for (mut paths, flag) in [
        (assumed, "--no-assume-unchanged"),
        (sparse, "--no-skip-worktree"),
    ] {
        if paths.stream_position()? != 0 {
            paths.rewind()?;
            run(source()
                .args(["update-index", flag, "-z", "--stdin"])
                .stdin(paths))?;
        }
    }
    Ok(())
}

pub(super) fn pack(
    root: &Path,
    stage: &Path,
    paths: &[u8],
    output: &mut impl Write,
) -> io::Result<Untracked> {
    let mut summary = Untracked::default();
    let stage = stage.canonicalize()?;
    let tree = Tree::open(root)?;
    for bytes in paths.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = Path::new(OsStr::from_bytes(bytes));
        if root.join(path).starts_with(&stage) {
            continue;
        }
        if bytes.ends_with(b"/") {
            summary.skipped.push(path.to_owned());
            continue;
        }
        let found = tree.parent(path, false).and_then(|(dir, name)| {
            let stat = statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            Ok((dir, name, stat))
        });
        let (dir, name, found) = match found {
            Ok(found) => found,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                summary.skipped.push(path.to_owned());
                continue;
            }
            Err(e) => return Err(e),
        };
        let kind = FileType::from_raw_mode(found.st_mode);
        if !matches!(kind, FileType::Symlink | FileType::RegularFile) {
            summary.skipped.push(path.to_owned());
            continue;
        }
        summary.files += 1;
        write_field(output, bytes)?;
        match kind {
            FileType::Symlink => {
                output.write_all(b"L")?;
                let link = readlinkat(&dir, name, Vec::new())?;
                summary.bytes += link.as_bytes().len() as u64;
                write_field(output, link.as_bytes())?;
            }
            FileType::RegularFile => {
                let mut file = File::from(openat(
                    &dir,
                    name,
                    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                let before = file.metadata()?;
                if !before.is_file() {
                    return Err(io::Error::other(format!(
                        "File {path:?} changed during checkout capture. Retry the launch."
                    )));
                }
                output.write_all(b"F")?;
                let permissions = (before.permissions().mode() & 0o777) as u16;
                output.write_all(&permissions.to_be_bytes())?;
                summary.bytes += before.len();
                output.write_all(&before.len().to_be_bytes())?;
                copy_exact(&mut file, output, before.len())?;
                let after = file.metadata()?;
                if after.len() != before.len() || after.modified()? != before.modified()? {
                    return Err(io::Error::other(format!(
                        "File {path:?} changed during checkout capture. Retry the launch."
                    )));
                }
            }
            _ => unreachable!("only files and symlinks reach the transfer"),
        }
    }
    write_field(output, &[])?;
    Ok(summary)
}

pub(super) fn exclude(file: &mut File, path: &[u8]) -> io::Result<()> {
    file.write_all(b"/")?;
    for &b in path {
        if b == b'\n' {
            return Err(io::Error::other(format!(
                "Git cannot exclude the newline in {:?}. Rename or track that file before launching remotely.",
                OsStr::from_bytes(path)
            )));
        }
        // A literal bracket expression preserves CR even at the end of a line.
        if b == b'\r' {
            file.write_all(b"[\r]")?;
            continue;
        }
        if b"\\*?[] #!".contains(&b) {
            file.write_all(b"\\")?;
        }
        file.write_all(&[b])?;
    }
    file.write_all(b"\n")
}

pub(super) const BASELINE: &str = ".git/ox-private-inputs-v1";

struct HashingWriter<W> {
    output: W,
    hash: Sha256,
}
impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.output.write(data)?;
        self.hash.update(&data[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

/// Changes to private inputs are delivered over the host link separately
/// from Git results. Paths can include non-UTF-8 names and symlinks.
pub fn changed_private_inputs(root: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    let tree = Tree::open(root)?;
    let mut baseline = io::BufReader::new(File::open(root.join(BASELINE))?);
    let mut changes = Vec::new();
    loop {
        let bytes = read_field(&mut baseline)?;
        if bytes.is_empty() {
            return Ok(changes);
        }
        let path = Path::new(OsStr::from_bytes(&bytes));
        let mut mode = [0];
        let mut expected = [0; 32];
        baseline.read_exact(&mut mode)?;
        let mut permissions = [0; 2];
        baseline.read_exact(&mut permissions)?;
        baseline.read_exact(&mut expected)?;
        let current = || -> io::Result<Option<[u8; 32]>> {
            let (dir, name) = tree.parent(path, false)?;
            let found = statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            match (FileType::from_raw_mode(found.st_mode), mode[0]) {
                (FileType::Symlink, b'L') => Ok(Some(
                    Sha256::digest(readlinkat(dir, name, Vec::new())?.as_bytes()).into(),
                )),
                (FileType::RegularFile, b'F')
                    if (found.st_mode & 0o777) as u16 == u16::from_be_bytes(permissions) =>
                {
                    let mut f = File::from(openat(
                        dir,
                        name,
                        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?);
                    if !f.metadata()?.is_file() {
                        return Ok(None);
                    }
                    let mut hash = Sha256::new();
                    io::copy(&mut f, &mut hash)?;
                    Ok(Some(hash.finalize().into()))
                }
                _ => Ok(None),
            }
        };
        match current() {
            Ok(Some(hash)) if hash == expected => (),
            Ok(_) => changes.push(path.to_owned()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) || e.raw_os_error() == Some(libc::ELOOP) =>
            {
                changes.push(path.to_owned())
            }
            Err(e) => return Err(e),
        }
    }
}

pub(super) fn unpack(root: &Path, input: &mut impl Read, private_inputs: bool) -> io::Result<()> {
    let tree = Tree::open(root)?;
    let mut private = if private_inputs {
        let mut excludes = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join(".git/info/exclude"))?;
        excludes.write_all(b"\n")?;
        Some((
            excludes,
            tempfile::tempfile()?,
            io::BufWriter::new(File::create_new(root.join(BASELINE))?),
        ))
    } else {
        None
    };
    loop {
        let bytes = read_field(input)?;
        if bytes.is_empty() {
            break;
        }
        let (dir, name) = tree.parent(Path::new(OsStr::from_bytes(&bytes)), true)?;
        let mut mode = [0];
        input.read_exact(&mut mode)?;
        let mut permissions = [0; 2];
        let digest: [u8; 32] = match mode[0] {
            b'L' => {
                let target = read_field(input)?;
                symlinkat(OsStr::from_bytes(&target), &dir, name)?;
                Sha256::digest(&target).into()
            }
            b'F' => {
                input.read_exact(&mut permissions)?;
                let perm = u32::from(u16::from_be_bytes(permissions));
                if perm > 0o777 {
                    return Err(io::Error::other("Invalid private file permissions"));
                }
                let mut len = [0; 8];
                input.read_exact(&mut len)?;
                let fd = openat(
                    &dir,
                    name,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(0o600),
                )?;
                let mut writer = HashingWriter {
                    output: File::from(fd),
                    hash: Sha256::new(),
                };
                copy_exact(input, &mut writer, u64::from_be_bytes(len))?;
                writer
                    .output
                    .set_permissions(fs::Permissions::from_mode(perm))?;
                writer.hash.finalize().into()
            }
            _ => return Err(io::Error::other("Unknown checkout file type")),
        };
        if let Some((excludes, names, baseline)) = &mut private {
            write_field(baseline, &bytes)?;
            baseline.write_all(&mode)?;
            baseline.write_all(&permissions)?;
            baseline.write_all(&digest)?;
            exclude(excludes, &bytes)?;
            names.write_all(&bytes)?;
            names.write_all(&[0])?;
        }
    }
    if input.read(&mut [0])? != 0 {
        return Err(io::Error::other("Unexpected trailing checkout data"));
    }
    let Some((_, mut names, mut baseline)) = private else {
        return Ok(());
    };
    write_field(&mut baseline, &[])?;
    baseline.flush()?;
    if names.stream_position()? != 0 {
        names.rewind()?;
        let ignored = git(root)
            .args(["check-ignore", "--stdin", "-z"])
            .stdin(names.try_clone()?)
            .output()?;
        if !matches!(ignored.status.code(), Some(0 | 1)) {
            return Err(io::Error::other(
                "Git could not verify the worker's private input excludes",
            ));
        }
        names.rewind()?;
        use std::io::BufRead;
        let mut expected = io::BufReader::new(names);
        let mut found = ignored.stdout.split(|b| *b == 0);
        let mut path = Vec::new();
        while expected.read_until(0, &mut path)? != 0 {
            path.pop();
            if found.next() != Some(path.as_slice()) {
                return Err(io::Error::other(format!(
                    "The repository’s ignore rules would stage {:?}. Review its .gitignore rule before launching remotely.",
                    OsStr::from_bytes(&path)
                )));
            }
            path.clear();
        }
    }
    Ok(())
}
