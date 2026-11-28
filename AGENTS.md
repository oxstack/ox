# Working on OX

Read this before changing anything. It is the same for every agent and every
person. `CLAUDE.md` only points here.

## What OX is, and what follows from it

OX is a power tool for developers. The model is the user's advisor and the user
decides.

- **Defaults match stock Claude Code and Codex in a terminal.** OX adds no
  prompt, confirmation or restriction of its own by default.
- **Do not design against a misbehaving model.** We do not make the product
  worse to protect the user from the tool they chose to run.
- **A guardrail is a setting.** If something like YOLO mode deserves a guard,
  it is an option in Settings that the user controls. It is not hard-coded.
- **Before adding a block, confirmation or limit**, name the irreversible harm
  it prevents. If there is none, do not add it. If there is one, add the
  override with it. Example: a signed install refuses unsigned updates, and
  `OX_ALLOW_UNSIGNED=1` turns that off.

## External tool versions

- Use the executable already installed on the host when it supports the
  requested operation. Check the capability or protocol the operation needs.
  Use version strings for diagnostics and report missing capabilities clearly.
- When OX manages a dependency download, default to the newest official stable
  release whose release and matching platform artifact have been available for
  at least 24 hours. Verify the published integrity metadata before using it.
  Keep a working installation until its replacement is verified.
- Keep exact tool versions out of runtime defaults and requirements. CI may use
  a fixed compatibility reference alongside a lane that tracks the latest
  stable release. Record the actual tool version in each lane.

## How a change reaches users

1. **Every push to a pull request** runs the scoped fast checks, about 5
   minutes. `desktop/ci/pr_scope.py` picks extra lanes from the files changed;
   a change to workflows or packaging adds desktop packages, which take a
   quarter of an hour or more.
2. **The `full-ci` label** runs the legs `main` builds: four release binaries,
   three native test legs, four desktop packages. About 25 minutes.
3. **The merge queue** tests the pull request on top of current `main` and
   merges it. A branch does not have to be current with `main`, so do not
   update one just to catch up.
4. **`main`** builds, signs with Developer ID, notarizes and publishes the
   rolling `dev` release, about 50 minutes. Its builds run one at a time and
   are never cancelled: a merge during a build waits for it, and several
   merges while one build runs are built together afterwards. A push builds
   and publishes nothing when everything since the released commit is
   documentation.
5. **A `v*` tag** publishes a stable release. Only the maintainer creates tags.

## Rules for agents

- **Push per finished step, not per edit.** Each push cancels the run before
  it and starts the review over. Fix everything one review found in one push,
  and wait for the review of that push before the next.
- **Add `full-ci` last, and only when you would merge the change yourself.**
  On a pull request from the maintainer's account that is not a draft, the
  label also queues it for merge once it is green and Codex has reviewed the
  head commit with no unresolved P0 or P1 finding and no open security
  finding (`auto-queue.yml`). A push takes it out of line until the new
  commit is reviewed. Keep the pull request a draft if you want the full run
  without the merge. It costs another full run on every later
  push. Use it when code, packaging or workflows changed; documentation does
  not need it. Adding the label cancels the scoped run and starts the full
  one, so add it once the review of the head commit is clean; waiting for the
  scoped run first only wastes it.
- **Do not merge, enqueue, tag or change repository settings** unless the
  maintainer asked for that action.
- **A red check is yours until you have shown otherwise.** Read the log. If the
  failure is in code you did not touch, say so on the pull request with the
  job, the test and the evidence.
- **One re-run per job, at most.** "Flaky" is not a cause. A test that fails
  and then passes on the same code has a bug: find it and fix the test or the
  product. Reproduce by running the test a few hundred times on the platform
  that failed: a loop on Linux says nothing about a failure on macOS Intel.
  A throwaway branch with a workflow that runs on its own push can loop a
  small reproduction on any runner in minutes; delete the branch after.
- **Never skip, ignore or quarantine a test to get green.** Every test is there
  for a reason, or it is removed with that reason written down.
- **Say what you did not verify.** A workflow change is unverified until the
  path it changes has actually run.

## Waiting for CI and reviews

Waiting costs nothing; looking costs tokens every time. A `full-ci` run takes
about 25 minutes, so a look every two minutes is a dozen turns that each say
"still running".

Not this, repeated:

```sh
gh pr checks 40; sleep 120 && gh pr view 40 --json state,mergeStateStatus
```

This, once, in the background, and then start the next piece of work:

```sh
gh pr checks 40 --watch --fail-fast --interval 60
```

- **Wait with one blocking command.** It returns when the run ends and fails
  if the run failed. For a single run, use
  `gh run watch <run-id> --exit-status --interval 60`. Start it in the
  background, or with a timeout longer than the run: 5 minutes scoped, 20 with
  desktop packages, 30 for `full-ci`, 50 for `main`'s release.
- **If your harness delivers pull request events, end your turn.** A failed
  check, a review or a comment wakes you. Do not also poll.
- **While you wait, do not push to that branch.** Start the next task on its
  own branch, or stop. The command or the event tells you when to come back.
- **A red `OX merge check` on a commit that is no longer the head is the run
  your push or label cancelled.** Confirm with
  `gh run view <run-id> --json conclusion` (`cancelled`) and move on. Only a
  red check on the head commit is a failure to read.
- **Read a failure once, from the log of the failed job**
  (`gh run view <run-id> --log-failed`), not from the list of checks.
- **A labelled pull request says where it stands.** The merge check keeps one
  comment on it, starting "In line to merge" or "Not in line to merge", with
  the reason and what to do. Read that comment, not the workflow's log. It
  also says when GitHub removed the pull request from the merge queue; then
  read the failed job of the queue's run, fix it and push.
- **Codex's code review takes one to two minutes** and edits its summary
  comment when it starts and when it ends. Read the summary once it says the
  review of the head commit is completed.

## While `main` moves under you

Branches are not kept current, and several agents work at once. The queue
builds and tests your change on top of whatever `main` is at that moment. That
catches a broken build or a failing test. It does not catch two changes that
each work and do not belong together, because git merges them cleanly:

- the same feature built twice, in two places;
- the same setting, flag, protocol field or file format added with two
  meanings, or one side changing what the other relies on;
- two changes to the same screen or the same flow that were each designed as
  if the other did not exist;
- a rule or a doc that now contradicts one that landed meanwhile.

So, before you call work finished:

- Fetch `main` and read what landed since you branched in the areas you
  touched: `git fetch origin main && git log --oneline HEAD..origin/main -- <paths>`.
- If something there overlaps, merge `main` into your branch and reconcile it
  on purpose. That, and a branch stacked on a pull request that was then
  squash-merged, are the reasons to bring `main` in by hand. For the stacked
  branch, first check that `main` holds exactly what you built on
  (`git diff <old base tip> origin/main` is empty); then every conflict is
  resolved with your side.
- Say in the description which commit of `main` you checked against.

## Stacked pull requests

Splitting a large change into pull requests that build on each other is fine,
and each one gets its own review. Only the bottom one can merge.

- **Base each pull request on the one below it**, so its diff shows only its
  own change. Say in the description what it is stacked on.
- **Label only the one based on `main`.** `full-ci` on a pull request based
  on another branch does nothing: automatic merging holds it, because that
  branch has no required checks.
- **When the one below has merged**, bring the next one down to `main`:
  1. Check that its base is now `main`. Head branches are deleted on merge,
     and GitHub then moves the pull requests that were based on one to
     `main`. If it did not, change it: `gh pr edit <number> --base main`.
  2. Merge `main` into its branch. The squash rewrote the commits it was
     built on, so git reports conflicts; check that `main` holds what you
     built on (`git diff <old base tip> origin/main -- <paths>`) and resolve
     with your side.
  3. Push, wait for the review of that commit, then add `full-ci`.
- **Before deleting a branch by hand**, check that no open pull request is
  based on it: `gh pr list --base <branch>`. If one is, change that pull
  request's base to `main` first.
- **One at a time.** Do not retarget the whole stack at once: every pull
  request above the bottom would show the changes below it as its own.

## What a pull request cannot test

These run for the first time on `main` or a tag. A green pull request says
nothing about them, so a change to any of them needs a run of that path before
you call it done.

| Path | Where it lives | How to exercise it before merge |
| --- | --- | --- |
| Developer ID signing and notarization | `sign-cli` in `ci.yml`, `sign-macos` in `desktop.yml`, `desktop/ci/macos-*.sh` | Manual OX Desktop run with `sign` on a ref the `apple-release` environment admits. Ask the maintainer to admit the branch for that run. |
| The release asset list | `desktop/ci/release-assets.sh`, used by both publish jobs | `full-ci` runs it on the pull request's unsigned artifacts (job `release-assets`). Signing itself still runs only on `main`. |
| Publishing | `publish-dev` and `publish` in `ci.yml` | The upload and the swap cannot be rehearsed. Keep changes small and watch the first `main` run. The "is a newer commit already released" test is one API call you can run by hand: `gh api repos/oxstack/ox/compare/<this commit>...<dev tag commit> --jq .status`. |
| Tagged release | `publish` in `ci.yml` | Has never run. Rehearse before the first `v*` tag. |
| Intel desktop package | `package (macos-26-intel)` | `full-ci`, or a manual OX Desktop run with `intel`. |
| Automatic merging | `auto-queue.yml`, `desktop/ci/auto-queue.sh` | Both triggers run `main`'s copy, never the pull request's. Before merge: `desktop/ci/test_auto_queue.py`, and the script against a real pull request with `AUTO_QUEUE_DRY_RUN=1`, which reads GitHub and changes nothing. After merge: watch the first labelled pull request. |

## Traps we have already paid for

- **A called workflow gets no secrets by default.** `ci.yml` calls
  `desktop.yml`; without `secrets: inherit` the signing job received an empty
  certificate and failed after 36 minutes of green builds.
- **Starting a workflow directly is not the same as having it called.** A
  manual run of `desktop.yml` resolved the secrets that the called run did not.
  Test the path that production uses.
- **`actions/download-artifact` with a pattern** unpacks one match straight
  into the path and several matches into a directory each. Find files, do not
  assume the layout.
- **Linux runners deny unprivileged user namespaces.** Stock Codex needs them
  for its sandbox. Every Linux leg that runs Codex lifts the restriction
  first; the probe must unshare the network, as Codex does.
- **Cancelling `main`'s build for the next merge starves the release.** With
  merges coming faster than a build, five merges in two hours published
  nothing. `main`'s runs are not cancelled, and a build publishes even when
  `main` has moved, unless a newer commit is already released.
- **Automatic merging is for pull requests into `main`.** Another branch has
  no required checks: a stacked pull request with auto-merge on would merge
  into its base at once and reach nobody. See "Stacked pull requests".
- **A platform a pull request skips is a platform it can break.** Linux ARM
  and macOS Intel only run with `full-ci` or on `main`.
- **Any failed job on `main` loses the release for every platform.**
  `publish-dev` needs every job. `retry-main.yml` re-runs failed jobs once;
  it does not make an intermittent test acceptable.
- **`cargo test -p one-package` is not a slice of `--workspace`.** Cargo
  resolves features for the packages it is asked for, so the narrower build
  compiles the shared crates and the test again. The package job builds every
  test once; its scripts get executables from `desktop/ci/test-bin.sh`.
- **On macOS, a pseudo-terminal can drop what a program printed.** A test
  that closes its own copy of the terminal's writing end and then reads loses
  the whole output of a program that prints and exits at once: 1 run in 1500
  on macOS Intel, none on macOS ARM or Linux. Keep that end open until the
  program has exited and the output is drained
  (`tests/fixtures/setup_selector.py`).
- **A test loop that reacts to every event must act once.** The desktop
  lists an approval until the host has taken the answer. A loop that answers
  "whenever an approval is listed" answers twice if any event arrives in
  between, and the host refuses the second as already resolved.
- **A fake of an outside service proves your reading of it, not the
  service.** The merge gate's tests passed while it required a field that
  Codex leaves empty; every labelled pull request would have been held. Read
  one real answer before you write the code that parses it.
- **Codex answers `@codex security review` with a review of the head
  commit**, posted as "Codex Review". The security row of its summary can
  stay at an earlier commit, and a finished security review with no findings
  leaves its findings fields empty, not false.
- **Tests that share a binary share a process.** A test that depends on a
  file descriptor being closed can be broken by a sibling test that spawns a
  child at the same moment.
- **A test that writes a script and runs it can get "Text file busy".** The
  sibling's child holds the script's write descriptor until its exec. Write
  it with `executable::write` from `tests/common/executable.rs`.

## Secrets

- The certificate and the App Store Connect key live only in the
  `apple-release` environment. Only the two signing jobs enter it.
- Never reference a secret in a job that compiles third-party crates or
  installs third-party packages. That is why signing is a separate job.
- Which refs may enter the environment is a repository setting (`main` and
  `v*` tags), not something the workflow files enforce.

## Review guidelines

For anyone reviewing a pull request, human or automated. Security review has
its own rules in `SECURITY.md` and `docs/threat-model.md`.

Blocking:

- A test that is skipped, ignored, quarantined, retried or has a weakened
  assertion, without a root cause written down.
- A timeout made longer without a measurement showing the wait is slow.
- A secret referenced in a job that compiles or installs third-party code, or
  reachable from a pull request that does not come from this repository.
- A change to a path only `main` or a tag runs (signing, publishing, the
  release asset list) with no statement of how it was exercised.
- A new prompt, confirmation, block or limit on what the user or the agent may
  do, without a setting to turn it off and a named irreversible harm.
- A default that is stricter than stock Claude Code or Codex.
- A claim in the description that the diff or a run does not support.
- A change that duplicates or contradicts something on `main` today: the same
  feature built a second time, or a setting, flag, protocol field or format
  given a different meaning. Review against current `main`, not against the
  commit the branch started from; a clean merge does not show this.

Not worth a comment:

- Style the formatter and clippy already accept.
- A missing guard against the model or the user doing what they asked for.
- A request for more abstraction, more configuration or more comments.

Say plainly when a finding is a guess. Give the file, the line and the input
that goes wrong.

## Before asking for a merge

- Open a pull request ready for review, not as a draft: reviews run when it is
  opened, marked ready and on every push. A draft is only for holding a
  `full-ci` pull request back from merging by itself.
- Codex reviews the code of a ready pull request by itself, when it is opened
  and on every push. Wait for the review of the head commit and answer or fix
  every finding. A review of an earlier commit does not cover a later push.
- The security review runs by itself only when the pull request opens. The
  `full-ci` label asks for one of the head commit, and again after each later
  push, and holds the merge until Codex has answered. Do not ask by hand as
  well: a request the workflow did not write is not counted. Ask by hand
  (`@codex security review`) only after resolving a security finding.
- Ask by hand (`@codex review`) in two cases only: no review of the head
  commit has appeared, or you resolved a P0 or P1 thread. Resolving alone
  does not put the pull request back in line.
- **Close every finding you act on.** Reply on its thread with the commit
  that fixes it, or with why it is wrong, and then resolve the thread. A fix
  that is pushed with the thread left open still counts as an open finding:
  an unresolved P0 or P1 thread holds the merge, and nobody reading the pull
  request can tell it was dealt with.
- Resolve a P0 or P1 thread only when it is fixed or shown to be wrong, never
  to get the pull request moving.
- Then add `full-ci`.

## Local checks before you push

```sh
cargo fmt --all -- --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked
cargo fmt --manifest-path desktop/Cargo.toml --all -- --check
sh -n install.sh && bash -n install.sh && shellcheck install.sh
python3 -m unittest discover -s desktop/ci -p 'test_*.py'
```

`desktop/ci/linux-deps.sh shellcheck` installs shellcheck on Ubuntu.

## Keeping this file true

When CI teaches you something that would have saved an hour, add one line here
with the reason. When a line stops being true, delete it. Keep the file short
enough to read in full.
