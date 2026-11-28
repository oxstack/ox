//! The destination's portable, paged copy of a moved session's readable history.
use crate::move_ledger::Plan;
use ox_desk_core::handoff::{self, SourceItem};
use ox_desk_core::host_protocol::{Timeline, TimelineItem};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, Write},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Header {
    plan: Plan,
    items: usize,
}

pub(crate) struct Archive {
    file: BufReader<File>,
    offsets: Vec<u64>,
}

impl Archive {
    pub(crate) fn capture(
        file: &mut File,
        plan: &Plan,
        timeline: &Timeline,
    ) -> io::Result<[u8; 32]> {
        if file.metadata()?.len() != 0 {
            return Err(io::Error::other(
                "The transfer archive already contains data.",
            ));
        }
        file.rewind()?;
        Self::write(&mut *file, plan, timeline)?;
        file.sync_all()?;
        checksum(file)
    }

    pub(crate) fn fingerprint(plan: &Plan, timeline: &Timeline) -> io::Result<[u8; 32]> {
        let mut hash = Sha256::new();
        Self::write(&mut hash, plan, timeline)?;
        Ok(hash.finalize().into())
    }

    /// Capture the complete projection without carrying live approval or runtime state.
    fn write(mut file: impl Write, plan: &Plan, timeline: &Timeline) -> io::Result<()> {
        if timeline.detail["ready"] != true
            || timeline.detail["older"] == true
            || timeline.detail["loading"] == true
            || !timeline.detail["active"].is_null()
        {
            return Err(io::Error::other(
                "Wait for the full session history and current turn.",
            ));
        }
        if plan.source.native.is_empty() || plan.source.host == plan.target_host {
            return Err(io::Error::other("The transcript does not match the move."));
        }
        let header = Header {
            plan: plan.clone(),
            items: timeline.items.len(),
        };
        serde_json::to_writer(&mut file, &header)?;
        file.write_all(b"\n")?;
        for item in &timeline.items {
            // Reopening a stock journal restores final items directly, so
            // its per-connection streaming revision can differ from the live
            // timeline. The archived item is immutable on either path.
            let mut settled = item.clone();
            settled.revision = 1;
            serde_json::to_writer(&mut file, &settled)?;
            file.write_all(b"\n")?;
        }
        Ok(())
    }

    /// Verify the staged bytes and their binding before making the archive readable.
    pub(crate) fn open(mut file: File, plan: &Plan, digest: [u8; 32]) -> io::Result<Self> {
        if checksum(&mut file)? != digest {
            return Err(io::Error::other(
                "The moved history failed its integrity check.",
            ));
        }
        file.rewind()?;
        let mut file = BufReader::new(file);
        let mut line = String::new();
        file.read_line(&mut line)?;
        let header = serde_json::from_str::<Header>(&line)?;
        if &header.plan != plan {
            return Err(io::Error::other("The history belongs to a different move."));
        }
        let mut position = line.len() as u64;
        let mut offsets = Vec::new();
        loop {
            line.clear();
            let bytes = file.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            let _: TimelineItem = serde_json::from_str(&line)?;
            offsets.push(position);
            position += bytes as u64;
        }
        if offsets.len() != header.items {
            return Err(io::Error::other("The history transfer is incomplete."));
        }
        Ok(Self { file, offsets })
    }

    /// Return up to `count` items before a zero-based exclusive cursor.
    pub(crate) fn previous(
        &mut self,
        before: Option<usize>,
        count: usize,
    ) -> io::Result<(Vec<TimelineItem>, Option<usize>)> {
        let end = before.unwrap_or(self.offsets.len());
        if end > self.offsets.len() || count == 0 {
            return Err(io::Error::other("Invalid moved history page."));
        }
        let start = end.saturating_sub(count);
        if start == end {
            return Ok((Vec::new(), None));
        }
        self.file.seek(io::SeekFrom::Start(self.offsets[start]))?;
        let mut page = Vec::with_capacity(end - start);
        let mut line = String::new();
        for _ in start..end {
            line.clear();
            self.file.read_line(&mut line)?;
            page.push(serde_json::from_str(&line)?);
        }
        Ok((page, (start > 0).then_some(start)))
    }

    pub(crate) fn continuation_items(&mut self) -> io::Result<Vec<SourceItem>> {
        let mut cursor = Some(self.offsets.len());
        let mut pages = Vec::new();
        while let Some(before) = cursor {
            let (items, previous) = self.previous(Some(before), 128)?;
            pages.push(handoff::items(&Timeline {
                items,
                detail: serde_json::Value::Null,
            }));
            cursor = previous;
        }
        Ok(pages.into_iter().rev().flatten().collect())
    }
}

pub(crate) fn checksum(file: &mut File) -> io::Result<[u8; 32]> {
    file.rewind()?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            return Ok(digest.finalize().into());
        }
        digest.update(&buffer[..bytes]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::move_ledger::{ArchivePin, Ledger, Owner};
    use ox_desk_core::host_protocol::Scope;
    use ox_desk_core::runtime::Route;
    use serde_json::json;
    use uuid::Uuid;

    fn fixture() -> (Header, Timeline) {
        let items = (0..6)
            .map(|i| TimelineItem {
                turn: format!("turn-{i}"),
                id: format!("reply-{i}"),
                role: "assistant".into(),
                text: format!("Original reply {i}\n"),
                revision: 1,
                detail: json!({}),
            })
            .collect::<Vec<_>>();
        let header = Header {
            plan: Plan {
                id: Uuid::new_v4(),
                session: Uuid::new_v4(),
                source: Owner {
                    host: Uuid::new_v4(),
                    generation: 0,
                    scope: Scope {
                        project: "/laptop/ox".into(),
                        route: Route::CODEX_SUBSCRIPTION,
                    },
                    native: "source-thread".into(),
                },
                target_host: Uuid::new_v4(),
                target_scope: Scope {
                    project: "/studio/ox".into(),
                    route: Route::CODEX_SUBSCRIPTION,
                },
            },
            items: items.len(),
        };
        (
            header,
            Timeline {
                items,
                detail: json!({"ready":true}),
            },
        )
    }

    #[test]
    fn history_pages_need_only_the_destination_file() {
        let (header, timeline) = fixture();
        let mut file = tempfile::tempfile().unwrap();
        let digest = Archive::capture(&mut file, &header.plan, &timeline).unwrap();
        let mut archive = Archive::open(file, &header.plan, digest).unwrap();
        let (last, before) = archive.previous(None, 4).unwrap();
        assert_eq!(last, timeline.items[2..]);
        let (first, before) = archive.previous(before, 4).unwrap();
        assert_eq!(first, timeline.items[..2]);
        assert_eq!(before, None);
    }

    #[test]
    fn live_and_reopened_items_have_the_same_archive_fingerprint() {
        let (header, mut timeline) = fixture();
        let original = Archive::fingerprint(&header.plan, &timeline).unwrap();
        timeline.items[0].revision = 17;
        timeline.items[1].revision = 0;
        assert_eq!(
            Archive::fingerprint(&header.plan, &timeline).unwrap(),
            original
        );
        timeline.items[0].text.push('!');
        assert_ne!(
            Archive::fingerprint(&header.plan, &timeline).unwrap(),
            original
        );
    }

    #[test]
    fn unread_history_and_changed_transfer_bindings_do_not_stage() {
        let (mut header, mut timeline) = fixture();
        let mut file = tempfile::tempfile().unwrap();
        timeline.detail["older"] = true.into();
        assert!(Archive::capture(&mut file, &header.plan, &timeline).is_err());
        timeline.detail["older"] = false.into();
        let digest = Archive::capture(&mut file, &header.plan, &timeline).unwrap();
        assert!(Archive::open(file.try_clone().unwrap(), &header.plan, [0; 32]).is_err());
        header.plan.target_host = Uuid::new_v4();
        assert!(Archive::open(file, &header.plan, digest).is_err());
    }

    #[test]
    fn the_completed_move_retains_history_and_working_files_without_the_source() {
        use crate::{
            move_bundle::{Bundle, Presentation, plan_digest},
            move_target::{Files, VerifiedTarget},
        };
        use ox_host_web::move_manifest::TRANSCRIPT;
        let (mut header, timeline) = fixture();
        let laptop = tempfile::tempdir().unwrap();
        let studio = tempfile::tempdir().unwrap();
        let project = laptop.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let git = |args: &[&str]| {
            let run = std::process::Command::new("git")
                .current_dir(&project)
                .args(args)
                .output()
                .unwrap();
            assert!(
                run.status.success(),
                "{}",
                String::from_utf8_lossy(&run.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(project.join("tracked"), "committed").unwrap();
        git(&["add", "tracked"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "Base",
        ]);
        std::fs::write(project.join("tracked"), "working edit").unwrap();
        std::fs::write(project.join("scratch"), "untracked input").unwrap();
        header.plan.source.scope.project = project;
        let jobs = studio.path().join("moves");
        std::fs::create_dir(&jobs).unwrap();
        let checkout = jobs
            .canonicalize()
            .unwrap()
            .join(header.plan.id.to_string())
            .join("checkout/project");
        header.plan.target_scope.project = checkout.clone();
        let plan = &header.plan;
        let mut source = Ledger::open(laptop.path(), plan.source.host).unwrap();
        let mut receiver = Ledger::open(studio.path(), plan.target_host).unwrap();
        source.prepare_source(plan).unwrap();
        let presentation = Presentation {
            title: "Review the release plan".into(),
            updated_at: 42,
            last_result: None,
            model_provider: "openai".into(),
        };
        let original_manifest =
            Bundle::capture(laptop.path(), plan, presentation.clone(), &timeline)
                .unwrap()
                .manifest;
        std::fs::write(plan.source.scope.project.join("tracked"), "later edit").unwrap();
        let bundle = Bundle::capture(laptop.path(), plan, presentation.clone(), &timeline).unwrap();
        assert_eq!(bundle.manifest, original_manifest);
        let manifest_sha256 = bundle.manifest.sha256().unwrap();
        let files = || Files {
            directory: bundle.directory(),
            manifest: &bundle.manifest,
            manifest_sha256,
            expected_plan: Some(plan_digest(plan).unwrap()),
        };
        let mut other_plan = files();
        other_plan.expected_plan = Some([0; 32]);
        assert!(VerifiedTarget::install(&jobs, &receiver, plan.source.host, other_plan).is_err());
        let transcript = bundle.directory().join(TRANSCRIPT);
        let transcript_bytes = std::fs::read(&transcript).unwrap();
        std::fs::write(&transcript, b"changed during upload").unwrap();
        assert!(VerifiedTarget::install(&jobs, &receiver, plan.source.host, files()).is_err());
        assert!(!checkout.exists());
        std::fs::write(transcript, transcript_bytes).unwrap();
        let target = VerifiedTarget::install(&jobs, &receiver, plan.source.host, files()).unwrap();
        let staged = target
            .prepare(&mut receiver, plan.source.host, "target-thread".into())
            .unwrap();
        assert!(!receiver.can_write(&plan.target_scope, "target-thread"));
        assert!(
            target
                .history_for(&receiver, &plan.target_scope, "target-thread")
                .is_err()
        );
        source
            .bind_source_manifest(
                plan,
                ArchivePin {
                    manifest: manifest_sha256,
                    transcript: bundle.manifest.transcript.sha256,
                },
            )
            .unwrap();
        source
            .fence_source(plan.target_host, &staged, None)
            .unwrap();
        source.writer_released(plan).unwrap();
        let proof = source.commit_source(plan).unwrap();
        let activated = receiver
            .activate_receiver(plan.source.host, &proof)
            .unwrap();
        source.confirm_source(plan.target_host, &activated).unwrap();
        drop(source);
        drop(bundle);
        laptop.close().unwrap();

        let receiver = Ledger::open(studio.path(), plan.target_host).unwrap();
        assert!(receiver.can_write(&plan.target_scope, "target-thread"));
        let target = VerifiedTarget::load(&jobs, plan).unwrap();
        let checkpoint = target.codex_checkpoint(None).unwrap();
        assert!(
            checkpoint[0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(&plan.session.to_string())
        );
        assert!(
            checkpoint
                .iter()
                .any(|item| item["content"][0]["text"] == "Original reply 5\n")
        );
        assert_eq!(
            target
                .presentation_for(&receiver, &plan.target_scope, "target-thread")
                .unwrap(),
            &presentation
        );
        assert!(
            target
                .history_for(&receiver, &plan.target_scope, "another-thread")
                .is_err()
        );
        assert!(
            target
                .history_for(&receiver, &plan.source.scope, "target-thread")
                .is_err()
        );
        let mut archive = target
            .history_for(&receiver, &plan.target_scope, "target-thread")
            .unwrap();
        assert_eq!(
            archive.previous(None, timeline.items.len()).unwrap(),
            (timeline.items, None)
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked")).unwrap(),
            "working edit"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("scratch")).unwrap(),
            "untracked input"
        );
    }
}
