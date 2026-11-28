use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

fn repo(dir: &Path) {
    fs::create_dir(dir).unwrap();
    run(git(dir).args(["init", "-q"])).unwrap();
    run(git(dir).args(["config", "user.name", "Fixture"])).unwrap();
    run(git(dir).args(["config", "user.email", "fixture@localhost"])).unwrap();
}
#[test]
fn a_nested_project_transfers_the_working_tree_without_changing_the_index_or_stash() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join("source");
    repo(&source);
    fs::create_dir(source.join("project")).unwrap();
    fs::write(source.join("project/tracked"), "base").unwrap();
    fs::write(source.join("removed"), "delete me").unwrap();
    fs::write(source.join(".gitignore"), "*.secret\n").unwrap();
    run(git(&source).args(["add", "."])).unwrap();
    run(git(&source).args(["commit", "-qm", "Base"])).unwrap();
    let base = object(&source, "HEAD").unwrap();
    let mirror = home.path().join("target.git");
    run(git(home.path())
        .args(["clone", "--mirror"])
        .arg(&source)
        .arg(&mirror))
    .unwrap();
    fs::write(source.join("project/tracked"), "staged").unwrap();
    fs::write(source.join("new-tracked"), "staged new").unwrap();
    run(git(&source).args(["add", "."])).unwrap();
    fs::write(source.join("project/tracked"), "working").unwrap();
    fs::remove_file(source.join("removed")).unwrap();
    fs::write(source.join("local [x]*.txt"), "only here").unwrap();
    fs::write(source.join("api.secret"), "skip").unwrap();
    fs::write(source.join("script"), "exit 0").unwrap();
    fs::set_permissions(source.join("script"), fs::Permissions::from_mode(0o750)).unwrap();
    symlink("../outside.secret", source.join("link")).unwrap();
    let index = fs::read(source.join(".git/index")).unwrap();
    let refs = run(git(&source).args(["show-ref"])).unwrap();
    let snapshot = Snapshot::capture(
        home.path(),
        &source.join("project"),
        std::slice::from_ref(&base),
    )
    .unwrap();
    assert_eq!(snapshot.untracked.ignored_entries, 1);
    assert_ne!(snapshot.commit, base);
    assert_eq!(fs::read(source.join(".git/index")).unwrap(), index);
    assert_eq!(run(git(&source).args(["show-ref"])).unwrap(), refs);
    let project = receive(home.path(), 42, &mirror, snapshot.open().unwrap()).unwrap();
    assert_eq!(project, home.path().join("42/project"));
    let work = project.parent().unwrap();
    assert_eq!(
        run(git(work).args(["branch", "--show-current"])).unwrap(),
        b"ox/42\n"
    );
    assert_eq!(fs::read(project.join("tracked")).unwrap(), b"working");
    assert_eq!(fs::read(work.join("new-tracked")).unwrap(), b"staged new");
    assert!(!work.join("removed").exists() && !work.join("api.secret").exists());
    assert_eq!(fs::read(work.join("local [x]*.txt")).unwrap(), b"only here");
    assert_eq!(
        fs::read_link(work.join("link")).unwrap(),
        Path::new("../outside.secret")
    );
    assert_eq!(
        fs::metadata(work.join("script"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    run(git(work).args(["add", "-A"])).unwrap();
    assert!(
        run(git(work).args(["diff", "--cached", "--name-only"]))
            .unwrap()
            .is_empty()
    );
    assert!(receive(home.path(), 42, &mirror, snapshot.open().unwrap()).is_err());
    assert!(changed_private_inputs(work).unwrap().is_empty());
    fs::write(work.join("local [x]*.txt"), "worker edit to private input").unwrap();
    fs::remove_file(work.join("script")).unwrap();
    assert_eq!(
        changed_private_inputs(work).unwrap(),
        [PathBuf::from("local [x]*.txt"), PathBuf::from("script")]
    );
    assert_eq!(object(work, "HEAD").unwrap(), snapshot.commit);
    assert_eq!(object(&mirror, "HEAD").unwrap(), base);
}

#[test]
fn both_git_hash_formats_work_from_a_cold_mirror_and_a_warm_one() {
    for format in ["sha1", "sha256"] {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("source");
        fs::create_dir(&source).unwrap();
        run(git(&source)
            .args(["init", "-q"])
            .arg(format!("--object-format={format}")))
        .unwrap();
        fs::write(source.join("tracked"), "base").unwrap();
        run(git(&source).args(["add", "."])).unwrap();
        run(git(&source).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fix@localhost",
            "commit",
            "-qm",
            "base",
        ]))
        .unwrap();
        let mirror = home.path().join("mirror");
        fs::create_dir(&mirror).unwrap();
        let id = object(&source, "HEAD").unwrap();
        bare(&mirror, id.len()).unwrap();
        let cold = Snapshot::capture(home.path(), &source, &[]).unwrap();
        assert_eq!(cold.commit, id);
        assert!(cold.bundle_bytes > 0);
        let dst = receive(home.path(), 1, &mirror, cold.open().unwrap()).unwrap();
        assert_eq!(object(&dst, "HEAD").unwrap(), id);
        run(git(&mirror)
            .arg("fetch")
            .arg(&source)
            .args(["HEAD:refs/heads/main"]))
        .unwrap();
        let warm = Snapshot::capture(home.path(), &source, std::slice::from_ref(&id)).unwrap();
        assert_eq!(warm.bundle_bytes, 0);
        let dst = receive(home.path(), 2, &mirror, warm.open().unwrap()).unwrap();
        fs::remove_dir_all(&mirror).unwrap();
        assert_eq!(
            object(&dst, "HEAD^{tree}").unwrap(),
            object(&source, "HEAD^{tree}").unwrap()
        );
        run(git(&dst).args(["fsck", "--full", "--no-dangling"])).unwrap();
    }
}

#[test]
fn staged_deletion_recreated_on_disk_stays_private_with_a_split_index_in_a_linked_worktree() {
    let home = tempfile::tempdir().unwrap();
    let parent = home.path().join("source");
    repo(&parent);
    fs::write(parent.join("file"), "tracked").unwrap();
    fs::write(parent.join("keep"), "keep").unwrap();
    run(git(&parent).args(["add", "."])).unwrap();
    run(git(&parent).args(["commit", "-qm", "base"])).unwrap();
    let source = home.path().join("linked");
    run(git(&parent)
        .args(["worktree", "add", "-qb", "topic"])
        .arg(&source))
    .unwrap();
    run(git(&source).args(["rm", "--cached", "file"])).unwrap();
    fs::write(source.join("file"), "private").unwrap();
    run(git(&source).args(["update-index", "--split-index"])).unwrap();
    let before = run(git(&source).args(["status", "--porcelain=v1", "-z"])).unwrap();
    let snapshot = Snapshot::capture(home.path(), &source, &[]).unwrap();
    let dst = receive(home.path(), 23, &parent, snapshot.open().unwrap()).unwrap();
    assert_eq!(fs::read(dst.join("file")).unwrap(), b"private");
    assert_eq!(run(git(&dst).args(["ls-files", "-z"])).unwrap(), b"keep\0");
    assert!(
        run(git(&dst).args(["status", "--porcelain=v1", "-z"]))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        run(git(&source).args(["status", "--porcelain=v1", "-z"])).unwrap(),
        before
    );
}

#[test]
fn invalid_or_interrupted_transfers_leave_no_job_and_cannot_overwrite_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("source");
    repo(&root);
    fs::write(root.join("file"), "tracked").unwrap();
    run(git(&root).args(["add", "."])).unwrap();
    run(git(&root).args(["commit", "-qm", "base"])).unwrap();
    let known = object(&root, "HEAD").unwrap();
    let mut header = Vec::new();
    Snapshot::capture(dir.path(), &root, &[known])
        .unwrap()
        .open()
        .unwrap()
        .read_to_end(&mut header)
        .unwrap();
    header.truncate(header.len() - 4); // empty untracked section terminator
    for path in [
        b"file".as_slice(),
        b"../escape",
        b".git/config",
        b"a/../../escape",
        b"/tmp/outside",
    ] {
        let mut archive = header.clone();
        files::write_field(&mut archive, path).unwrap();
        archive.extend_from_slice(b"F");
        archive.extend_from_slice(&0o600u16.to_be_bytes());
        archive.extend_from_slice(&1u64.to_be_bytes());
        archive.extend_from_slice(b"x\0\0\0\0");
        assert!(
            receive(dir.path(), 4, &root, archive.as_slice()).is_err(),
            "{path:?}"
        );
        assert!(!dir.path().join("4").exists());
    }
    let mut archive = header;
    files::write_field(&mut archive, b"dir").unwrap();
    archive.extend_from_slice(b"L");
    files::write_field(&mut archive, root.as_os_str().as_bytes()).unwrap();
    files::write_field(&mut archive, b"dir/file").unwrap();
    archive.extend_from_slice(b"F");
    archive.extend_from_slice(&0o600u16.to_be_bytes());
    archive.extend_from_slice(&2u64.to_be_bytes());
    archive.extend_from_slice(b"xx\0\0\0\0");
    assert!(receive(dir.path(), 4, &root, archive.as_slice()).is_err());
    assert!(!dir.path().join("4").exists());
    assert_eq!(fs::read(root.join("file")).unwrap(), b"tracked");
    assert!(receive(dir.path(), 4, &root, &archive[..MAGIC.len() + 9]).is_err());
    assert!(!dir.path().join("4").exists());
}

#[test]
fn target_mirrors_use_the_targets_origin_and_survive_an_offline_refresh() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("target");
    repo(&target);
    fs::write(target.join("README"), "first").unwrap();
    run(git(&target).args(["add", "."])).unwrap();
    run(git(&target).args(["commit", "-qm", "first"])).unwrap();
    let origin = tmp.path().join("upstream.git");
    run(git(tmp.path())
        .args(["clone", "--bare"])
        .arg(&target)
        .arg(&origin))
    .unwrap();
    run(git(&target).args(["remote", "add", "origin", "../upstream.git"])).unwrap();
    run(git(&target).args(["update-ref", "refs/remotes/origin/second", "HEAD"])).unwrap();
    let cache = tmp.path().join("mirrors");
    fs::create_dir(&cache).unwrap();
    let mirror = Mirror::from_checkout(&cache, &target).unwrap();
    let same = Mirror::from_checkout(&cache, &target).unwrap();
    assert_eq!(mirror.path(), same.path());
    let first = object(&target, "HEAD").unwrap();
    assert_eq!(
        mirror.known(&[first.clone(), "1".repeat(40)]).unwrap(),
        std::slice::from_ref(&first)
    );
    let src = tmp.path().join("parent");
    run(git(tmp.path()).arg("clone").arg(&origin).arg(&src)).unwrap();
    fs::write(src.join("other"), "new").unwrap();
    run(git(&src).args(["add", "."])).unwrap();
    run(git(&src).args([
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@localhost",
        "commit",
        "-qm",
        "second",
    ]))
    .unwrap();
    run(git(&src).args(["push", "origin", "HEAD:refs/heads/second"])).unwrap();
    let second = object(&src, "HEAD").unwrap();
    assert_eq!(
        mirror.known(std::slice::from_ref(&second)).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        negotiate(&src, |ids| mirror.known(ids)).unwrap(),
        std::slice::from_ref(&first)
    );
    let incremental = Snapshot::capture(tmp.path(), &src, std::slice::from_ref(&first)).unwrap();
    assert!(incremental.bundle_bytes > 0);
    let early = receive(tmp.path(), 16, mirror.path(), incremental.open().unwrap()).unwrap();
    assert_eq!(object(&early, "HEAD").unwrap(), second);
    mirror.refresh().unwrap();
    assert_eq!(
        negotiate(&src, |ids| mirror.known(ids)).unwrap(),
        std::slice::from_ref(&second)
    );
    assert_eq!(
        mirror.known(std::slice::from_ref(&second)).unwrap(),
        std::slice::from_ref(&second)
    );
    Mirror::from_checkout(&cache, &target).unwrap();
    assert_eq!(
        object(mirror.path(), "refs/remotes/origin/second").unwrap(),
        second
    );
    run(git(&target).args(["fetch", "origin"])).unwrap();
    run(git(&target).args(["checkout", "--detach", "origin/second"])).unwrap();
    run(git(&target).args(["commit", "--allow-empty", "-qm", "third"])).unwrap();
    run(git(&target).args(["update-ref", "refs/remotes/origin/second", "HEAD"])).unwrap();
    let third = object(&target, "HEAD").unwrap();
    fs::rename(&origin, tmp.path().join("unavailable")).unwrap();
    assert!(mirror.refresh().is_err());
    Mirror::from_checkout(&cache, &target).unwrap();
    assert_eq!(
        mirror.known(std::slice::from_ref(&third)).unwrap(),
        std::slice::from_ref(&third)
    );
    let packet = Snapshot::capture(
        tmp.path(),
        &src,
        &mirror.known(std::slice::from_ref(&second)).unwrap(),
    )
    .unwrap();
    assert_eq!(packet.bundle_bytes, 0);
    let worker = receive(tmp.path(), 17, mirror.path(), packet.open().unwrap()).unwrap();
    assert_eq!(object(&worker, "HEAD").unwrap(), second);
    assert_eq!(
        object(&worker, "refs/remotes/origin/second").unwrap(),
        third
    );
    assert_eq!(
        line(run(git(&worker).args(["remote", "get-url", "origin"])).unwrap()).unwrap(),
        target
            .canonicalize()
            .unwrap()
            .join("../upstream.git")
            .into_os_string()
    );
}

#[test]
#[ignore = "requires Git LFS configured in a disposable test home"]
fn materialized_lfs_changes_arrive_offline_and_keep_their_pointer_on_git_add() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    repo(&source);
    fs::write(
        source.join(".gitattributes"),
        "*.raw filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();
    fs::write(source.join("asset.raw"), "first").unwrap();
    run(git(&source).args(["add", "."])).unwrap();
    run(git(&source).args(["commit", "-qm", "base"])).unwrap();
    let base = object(&source, "HEAD").unwrap();
    let mirror = tmp.path().join("mirror");
    run(git(tmp.path())
        .args(["clone", "--bare"])
        .arg(&source)
        .arg(&mirror))
    .unwrap();
    run(git(&mirror).args([
        "remote",
        "set-url",
        "origin",
        "https://127.0.0.1:1/unreachable",
    ]))
    .unwrap_or_else(|_| {
        run(git(&mirror).args(["remote", "add", "origin", "https://127.0.0.1:1/unreachable"]))
            .unwrap()
    });
    fs::write(source.join("asset.raw"), "modified offline").unwrap();
    let packet = Snapshot::capture(tmp.path(), &source, &[base]).unwrap();
    let dst = receive(tmp.path(), 18, &mirror, packet.open().unwrap()).unwrap();
    assert_eq!(
        fs::read(dst.join("asset.raw")).unwrap(),
        b"modified offline"
    );
    run(git(&dst).args(["add", "-A"])).unwrap();
    assert!(
        run(git(&dst).args(["diff", "--cached", "--name-only"]))
            .unwrap()
            .is_empty()
    );
    assert!(
        run(git(&dst).args(["show", "HEAD:asset.raw"]))
            .unwrap()
            .starts_with(b"version https://git-lfs.github.com/spec/v1\n")
    );
}

#[test]
fn moved_checkouts_pass_untracked_files_to_the_next_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("original");
    repo(&source);
    fs::write(source.join("kept.txt"), "include this in each move").unwrap();
    let snapshot = Snapshot::capture(temp.path(), &source, &[]).unwrap();
    let studio = receive_move(temp.path(), 1, snapshot.open().unwrap(), &snapshot.commit).unwrap();
    assert_eq!(
        run(git(&studio).args(["ls-files", "--others", "--exclude-standard"])).unwrap(),
        b"kept.txt\n"
    );
    let snapshot = Snapshot::capture(temp.path(), &studio, &[]).unwrap();
    let office = receive_move(temp.path(), 2, snapshot.open().unwrap(), &snapshot.commit).unwrap();
    assert_eq!(
        fs::read_to_string(office.join("kept.txt")).unwrap(),
        "include this in each move"
    );
}

#[test]
fn an_unborn_repo_and_ignored_only_project_can_open_an_empty_worker_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("repo");
    repo(&source);
    fs::create_dir(source.join("scratch")).unwrap();
    fs::write(source.join(".git/info/exclude"), "/scratch/\n").unwrap();
    fs::write(source.join("scratch/data"), "ignored").unwrap();
    let snapshot = Snapshot::capture(tmp.path(), &source.join("scratch"), &[]).unwrap();
    assert_eq!(snapshot.untracked.files, 0);
    let work = receive(tmp.path(), 7, &source, snapshot.open().unwrap()).unwrap();
    assert!(work.is_dir() && !work.join("data").exists());
    assert_eq!(object(&work, "HEAD").unwrap(), snapshot.commit);
    assert_eq!(head(&source).unwrap(), None);
}

#[test]
fn a_populated_submodule_transfers_local_edits_and_private_inputs_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let library = tmp.path().join("library");
    repo(&library);
    fs::write(library.join("src"), "base").unwrap();
    run(git(&library).args(["add", "."])).unwrap();
    run(git(&library).args(["commit", "-qm", "library"])).unwrap();
    fs::write(library.join("src"), "second revision").unwrap();
    run(git(&library).args(["commit", "-qam", "second revision"])).unwrap();
    let source = tmp.path().join("source");
    repo(&source);
    run(git(&source)
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--depth=1",
            "--",
        ])
        .arg(format!("file://{}", library.display()))
        .arg("deps/library"))
    .unwrap();
    run(git(&source).args(["commit", "-qm", "parent"])).unwrap();
    let sub = source.join("deps/library");
    assert_eq!(
        run(git(&sub).args(["rev-parse", "--is-shallow-repository"])).unwrap(),
        b"true\n"
    );
    let original = object(&sub, "HEAD").unwrap();
    let base = object(&source, "HEAD").unwrap();
    fs::write(sub.join("src"), "working edits").unwrap();
    fs::write(sub.join("private"), "local input").unwrap();
    fs::remove_dir_all(&library).unwrap();
    let before = run(git(&source).args(["ls-files", "--stage", "-z"])).unwrap();
    let packet = Snapshot::capture(tmp.path(), &source, std::slice::from_ref(&base)).unwrap();
    assert_ne!(packet.commit, base);
    assert_eq!(packet.untracked.files, 1);
    assert_eq!(object(&sub, "HEAD").unwrap(), original);
    assert_eq!(
        run(git(&source).args(["ls-files", "--stage", "-z"])).unwrap(),
        before
    );
    let job = receive(tmp.path(), 91, &source, packet.open().unwrap()).unwrap();
    let installed = job.join("deps/library");
    assert_eq!(fs::read(installed.join("src")).unwrap(), b"working edits");
    assert_eq!(fs::read(installed.join("private")).unwrap(), b"local input");
    assert_ne!(object(&installed, "HEAD").unwrap(), original);
    run(git(&installed).args(["add", "-A"])).unwrap();
    assert!(
        run(git(&job).args(["status", "--porcelain"]))
            .unwrap()
            .is_empty()
    );
    assert!(run(git(&installed).arg("remote")).unwrap().is_empty());
    fs::write(installed.join("private"), "changed").unwrap();
    assert_eq!(
        changed_private_inputs(&installed).unwrap(),
        [PathBuf::from("private")]
    );
}

#[test]
fn a_private_input_must_remain_excluded_after_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    repo(&source);
    fs::write(source.join(".gitignore"), "!explicit\n").unwrap();
    run(git(&source).args(["add", ".gitignore"])).unwrap();
    run(git(&source).args(["commit", "-qm", "ignore rules"])).unwrap();
    for input in ["explicit", "line\nfeed"] {
        fs::write(source.join(input), b"private").unwrap();
        let packet = Snapshot::capture(tmp.path(), &source, &[]).unwrap();
        let err = receive(tmp.path(), 92, &source, packet.open().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("exclude") || err.to_string().contains("ignore rules"),
            "{err}"
        );
        assert!(!tmp.path().join("92").exists());
        fs::remove_file(source.join(input)).unwrap();
    }
}

#[test]
fn shallow_history_crosses_a_cold_link_and_retains_a_full_targets_history() {
    let tmp = tempfile::tempdir().unwrap();
    let history = tmp.path().join("history");
    repo(&history);
    for text in ["first", "second"] {
        fs::write(history.join("tracked"), text).unwrap();
        run(git(&history).args(["add", "tracked"])).unwrap();
        run(git(&history).args(["commit", "-qm", text])).unwrap();
    }
    let source = tmp.path().join("shallow");
    run(git(tmp.path())
        .args(["clone", "--depth=1", "--"])
        .arg(format!("file://{}", history.display()))
        .arg(&source))
    .unwrap();
    fs::write(source.join("tracked"), "shallow dirty").unwrap();
    let base = object(&source, "HEAD").unwrap();
    let warm = Snapshot::capture(tmp.path(), &source, &[base]).unwrap();
    let job = receive(tmp.path(), 38, &history, warm.open().unwrap()).unwrap();
    assert_eq!(fs::read(job.join("tracked")).unwrap(), b"shallow dirty");
    let cold = Snapshot::capture(tmp.path(), &source, &[]).unwrap();
    let empty = tmp.path().join("mirror.git");
    fs::create_dir(&empty).unwrap();
    bare(&empty, 40).unwrap();
    let job = receive(tmp.path(), 39, &empty, cold.open().unwrap()).unwrap();
    assert_eq!(fs::read(job.join("tracked")).unwrap(), b"shallow dirty");
    assert_eq!(
        run(git(&job).args(["rev-parse", "--is-shallow-repository"])).unwrap(),
        b"true\n"
    );
    run(git(&job).args(["fsck", "--full", "--no-dangling"])).unwrap();
    let full = receive(tmp.path(), 40, &history, cold.open().unwrap()).unwrap();
    assert_eq!(
        run(git(&full).args(["rev-parse", "--is-shallow-repository"])).unwrap(),
        b"false\n"
    );
}

#[test]
fn sparse_and_assume_unchanged_files_keep_their_actual_state_and_source_flags() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    repo(&src);
    for dir in ["app", "other"] {
        fs::create_dir(src.join(dir)).unwrap();
        fs::write(src.join(dir).join("data"), b"base").unwrap();
    }
    run(git(&src).args(["add", "."])).unwrap();
    run(git(&src).args(["commit", "-qm", "base"])).unwrap();
    run(git(&src).args(["sparse-checkout", "set", "--cone", "--sparse-index", "app"])).unwrap();
    run(git(&src).args(["update-index", "--assume-unchanged", "app/data"])).unwrap();
    fs::write(src.join("app/data"), "local").unwrap();
    assert!(!src.join("other/data").exists());
    let index = fs::read(src.join(".git/index")).unwrap();
    let packet = Snapshot::capture(tmp.path(), &src, &[]).unwrap();
    let job = receive(tmp.path(), 15, &src, packet.open().unwrap()).unwrap();
    assert_eq!(fs::read(job.join("app/data")).unwrap(), b"local");
    assert_eq!(fs::read(job.join("other/data")).unwrap(), b"base");
    assert_eq!(fs::read(src.join(".git/index")).unwrap(), index);
}

#[test]
fn write_results_keep_dirty_parent_and_private_inputs_and_publish_create_only() {
    use results::{Baseline, Destination, Frozen, import, publish_checkout};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let parent = root.join("parent");
    repo(&parent);
    fs::write(parent.join("src"), "base\n").unwrap();
    run(git(&parent).args(["add", "src"])).unwrap();
    run(git(&parent).args(["commit", "-qm", "base"])).unwrap();
    let base = object(&parent, "HEAD").unwrap();
    let remote = root.join("bare origin.git");
    run(git(root)
        .args(["clone", "--bare", "--"])
        .arg(&parent)
        .arg(&remote))
    .unwrap();
    run(git(&parent).args(["remote", "add", "origin"]).arg(&remote)).unwrap();
    fs::write(parent.join("src"), "dirty source\n").unwrap();
    fs::write(parent.join("private input"), "local content").unwrap();
    let sent = Snapshot::capture(root, &parent, std::slice::from_ref(&base)).unwrap();
    let work = receive(root, 73, &remote, sent.open().unwrap()).unwrap();
    let baseline = Baseline::read_checkout(&work, &sent.commit).unwrap();
    fs::write(work.join("src"), "worker edit\n").unwrap();
    run(git(&work).args(["add", "src"])).unwrap();
    run(git(&work).args([
        "-c",
        "user.name=Worker",
        "-c",
        "user.email=w@localhost",
        "commit",
        "-qm",
        "work",
    ]))
    .unwrap();
    fs::write(work.join("new file"), "also return this").unwrap();
    run(git(&work).args(["add", "new file"])).unwrap();
    fs::write(work.join("unstaged file"), "only over link").unwrap();
    use std::io::Write;
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(work.join(".git/info/exclude"))
            .unwrap(),
        "/scratch.log"
    )
    .unwrap();
    fs::write(work.join("scratch.log"), "ignore me").unwrap();
    fs::write(work.join("private input"), "private edit").unwrap();
    let frozen = Frozen::capture(root, &work, &baseline, &[]).unwrap();
    assert_eq!(frozen.summary.snapshot, sent.commit);
    assert!(
        frozen.summary.diffstat.contains("2 files changed"),
        "{:?}",
        frozen.summary
    );
    let clean = receive_result(
        root,
        74,
        frozen.packet.open().unwrap(),
        &frozen.summary.commit,
    )
    .unwrap();
    assert_eq!(
        fs::read(clean.join("private input")).unwrap(),
        b"private edit"
    );
    assert_eq!(
        fs::read(clean.join("unstaged file")).unwrap(),
        b"only over link"
    );
    assert!(!clean.join("scratch.log").exists());
    let checkout = run(git(&clean).args(["ls-tree", "--name-only", "HEAD"])).unwrap();
    assert_eq!(checkout, b"new file\nsrc\n");
    for dest in [Destination::Branch, Destination::Ref] {
        let name = dest.reference(73).unwrap();
        let mut fingerprint = String::new();
        for _ in 0..2 {
            fingerprint =
                publish_checkout(&clean, &parent, &baseline, 73, dest, &frozen.summary).unwrap();
            assert_eq!(object(&remote, &name).unwrap(), frozen.summary.commit);
        }
        assert!(
            results::retire(
                &parent,
                &remote,
                73,
                dest,
                &frozen.summary.commit,
                "changed-url"
            )
            .is_err()
        );
        results::retire(
            &parent,
            &remote,
            73,
            dest,
            &frozen.summary.commit,
            &fingerprint,
        )
        .unwrap();
        assert!(object(&remote, &name).is_err());
        results::retire(
            &parent,
            &remote,
            73,
            dest,
            &frozen.summary.commit,
            &fingerprint,
        )
        .unwrap();
        run(git(&remote).args(["update-ref", &name, &base])).unwrap();
        assert!(publish_checkout(&clean, &parent, &baseline, 73, dest, &frozen.summary).is_err());
        results::retire(
            &parent,
            &remote,
            73,
            dest,
            &frozen.summary.commit,
            &fingerprint,
        )
        .unwrap();
        assert_eq!(object(&remote, &name).unwrap(), base);
    }
    let index = fs::read(parent.join(".git/index")).unwrap();
    fs::write(parent.join(".git/FETCH_HEAD"), "preserve").unwrap();
    for _ in 0..2 {
        let r = import(&parent, &clean, 73, &frozen.summary).unwrap();
        assert_eq!(object(&parent, &r).unwrap(), frozen.summary.commit);
    }
    let reference = "refs/ox/jobs/73/result";
    run(git(&parent).args(["update-ref", reference, &base])).unwrap();
    let collision = import(&parent, &clean, 73, &frozen.summary).unwrap_err();
    assert_eq!(collision.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(object(&parent, reference).unwrap(), base);
    assert_eq!(fs::read(parent.join(".git/index")).unwrap(), index);
    assert_eq!(object(&parent, "HEAD").unwrap(), base);
    assert_eq!(
        fs::read(parent.join(".git/FETCH_HEAD")).unwrap(),
        b"preserve"
    );
    assert_eq!(fs::read(parent.join("src")).unwrap(), b"dirty source\n");
    assert_eq!(
        fs::read(parent.join("private input")).unwrap(),
        b"local content"
    );
    run(git(&work).args(["add", "-f", "private input"])).unwrap();
    run(git(&work).args([
        "-c",
        "user.name=Worker",
        "-c",
        "user.email=w@localhost",
        "commit",
        "-qm",
        "forced",
    ]))
    .unwrap();
    run(git(&work).args(["rm", "private input"])).unwrap();
    run(git(&work).args([
        "-c",
        "user.name=Worker",
        "-c",
        "user.email=w@localhost",
        "commit",
        "-qm",
        "removed",
    ]))
    .unwrap();
    let private = Frozen::capture(root, &work, &baseline, &[]).unwrap();
    let clean = receive_result(
        root,
        75,
        private.packet.open().unwrap(),
        &private.summary.commit,
    )
    .unwrap();
    let err = publish_checkout(
        &clean,
        &parent,
        &baseline,
        75,
        Destination::Branch,
        &private.summary,
    )
    .unwrap_err();
    assert!(err.to_string().contains("private input"), "{err}");
    assert!(object(&remote, "refs/heads/ox/75").is_err());
}
