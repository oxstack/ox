use crate::transport::Packet;
use ox_desk_core::host_protocol::{Cursor, Scope, supports_route};
use std::collections::VecDeque;

pub(crate) struct Replay {
    pub(crate) cursor: Cursor,
    floor: u64,
    bytes: usize,
    frames: VecDeque<(u64, Scope, Packet)>,
}
impl Replay {
    pub(crate) fn new(seq: u64, epoch: String) -> Self {
        Self {
            cursor: Cursor { epoch, seq },
            floor: seq,
            bytes: 0,
            frames: Default::default(),
        }
    }
    pub(crate) fn append(&mut self, scope: Scope, packet: Packet) {
        self.bytes += packet.len();
        self.frames.push_back((self.cursor.seq, scope, packet));
        while self.bytes > 8 * 1024 * 1024 || self.frames.len() > 4096 {
            let (seq, _, data) = self.frames.pop_front().expect("nonempty replay");
            self.floor = seq;
            self.bytes -= data.len();
        }
    }
    pub(crate) fn after<'a>(
        &'a self,
        scope: Option<&'a Scope>,
        cursor: &Cursor,
        version: u32,
    ) -> Option<impl Iterator<Item = Packet> + 'a> {
        if cursor.epoch != self.cursor.epoch
            || cursor.seq < self.floor
            || cursor.seq > self.cursor.seq
        {
            return None;
        }
        let seq = cursor.seq;
        Some(
            self.frames
                .iter()
                .filter(move |(i, s, _)| {
                    *i > seq
                        && match scope {
                            Some(wanted) => wanted == s,
                            None => s.route.runtime != ox_desk_core::runtime::Runtime::OpenCode,
                        }
                        && supports_route(version, &s.route)
                })
                .map(|(_, _, p)| p.clone()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::runtime::Route;
    fn scope(name: &str) -> Scope {
        Scope {
            project: name.into(),
            route: Route::CODEX_SUBSCRIPTION,
        }
    }
    #[test]
    fn scopes_resume_without_duplication_and_expired_cursors_require_a_new_snapshot() {
        let a = scope("/a");
        let b = scope("/b");
        let mut replay = Replay::new(40, "boot-1".into());
        let start = replay.cursor.clone();
        for (scope, value) in [(&a, 10), (&b, 20), (&a, 30)] {
            replay.cursor.seq += 1;
            replay.append(scope.clone(), vec![value].into());
        }
        assert_eq!(
            replay
                .after(Some(&a), &start, 1)
                .unwrap()
                .map(|b| b[0])
                .collect::<Vec<_>>(),
            [10, 30]
        );
        assert_eq!(
            replay
                .after(
                    None,
                    &Cursor {
                        epoch: "boot-1".into(),
                        seq: 42
                    },
                    1,
                )
                .unwrap()
                .map(|b| b[0])
                .collect::<Vec<_>>(),
            [30]
        );
        assert_eq!(replay.after(None, &replay.cursor, 1).unwrap().count(), 0);
        assert!(
            replay
                .after(
                    None,
                    &Cursor {
                        epoch: "boot-2".into(),
                        seq: 40
                    },
                    1,
                )
                .is_none()
        );
        for _ in 0..4097 {
            replay.cursor.seq += 1;
            replay.append(a.clone(), vec![1].into());
        }
        assert!(replay.after(None, &start, 1).is_none());
    }

    #[test]
    fn v1_replay_omits_antigravity_without_losing_later_frames() {
        use ox_desk_core::runtime::Runtime;
        let mut replay = Replay::new(0, "boot".into());
        let start = replay.cursor.clone();
        let codex = scope("/project");
        let antigravity = Scope {
            project: codex.project.clone(),
            route: Runtime::Antigravity.route(None),
        };
        for (scope, marker) in [(antigravity, 1), (codex, 2)] {
            replay.cursor.seq += 1;
            replay.append(scope, vec![marker].into());
        }
        let markers = |version| {
            replay
                .after(None, &start, version)
                .unwrap()
                .map(|packet| packet[0])
                .collect::<Vec<_>>()
        };
        assert_eq!(markers(1), [2]);
        assert_eq!(markers(2), [1, 2]);
    }
}
