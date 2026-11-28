//! One permission model for workers and handoffs. Every native preset belongs to
//! one of four ordered levels. A worker or handoff target starts at its source's
//! level, mapped to the nearest preset of the target runtime that is not more
//! permissive. An agent can lower that level and never raise it. YOLO and bypass
//! always need a person: an explicit acceptance or a grant on an ancestor.
//!
//! | Level        | Codex       | Claude Code              | Antigravity              |
//! | ------------ | ----------- | ------------------------ | ------------------------ |
//! | `read`       | Read only   | Plan (Default also reads) | Plan (Default also reads) |
//! | `write`      | Workspace   | Accept edits             | Accept edits             |
//! | `classifier` | Auto-review | Auto                     |                          |
//! | `yolo`       | YOLO        | bypass permissions       | skip tool permissions    |
//!
//! A preset of the other runtime, or no confirmed preset, is unverified and
//! grants nothing. A Codex thread shown as `Configured` still has a ceiling
//! when its effective settings verify one (`codex::access::Permissions`); a
//! user-defined Codex profile or an unknown policy never does.
use crate::runtime::{Access, Runtime};
use serde::{Deserialize, Serialize};

/// Ordered permission levels. `Ord` is "more permissive than".
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Reads the project. Anything else asks a person.
    Read,
    /// Edits the workspace without asking about each edit.
    Write,
    /// A classifier reviews actions instead of a person.
    Classifier,
    /// Nothing reviews actions.
    Yolo,
}

impl Level {
    pub const ALL: [Self; 4] = [Self::Read, Self::Write, Self::Classifier, Self::Yolo];

    /// The level of a confirmed native preset. `None` means unverified.
    pub fn of(runtime: Runtime, access: Access) -> Option<Self> {
        Some(match (runtime, access) {
            (Runtime::Codex, Access::ReadOnly) => Self::Read,
            (Runtime::Codex, Access::Workspace) => Self::Write,
            (Runtime::Codex, Access::AutoReview) => Self::Classifier,
            (Runtime::Codex, Access::Yolo) => Self::Yolo,
            (Runtime::Claude | Runtime::Antigravity, Access::Plan | Access::Default) => Self::Read,
            (Runtime::Claude | Runtime::Antigravity, Access::AcceptEdits) => Self::Write,
            (Runtime::Claude, Access::Auto) => Self::Classifier,
            (Runtime::Claude, Access::BypassPermissions) => Self::Yolo,
            (Runtime::Antigravity, Access::Yolo) => Self::Yolo,
            _ => return None,
        })
    }
    /// Map a level to the closest supported preset at or below it.
    pub fn for_runtime(self, runtime: Runtime) -> Self {
        match (runtime, self) {
            (Runtime::Antigravity, Self::Classifier) => Self::Write,
            _ => self,
        }
    }
    /// The preset a child of this level starts in on `runtime`.
    pub fn preset(self, runtime: Runtime) -> Access {
        match (runtime, self) {
            (Runtime::Codex, Self::Read) => Access::ReadOnly,
            (Runtime::Codex, Self::Write) => Access::Workspace,
            (Runtime::Codex, Self::Classifier) => Access::AutoReview,
            (Runtime::Codex, Self::Yolo) => Access::Yolo,
            (Runtime::Claude, Self::Read) => Access::Plan,
            (Runtime::Claude, Self::Write) => Access::AcceptEdits,
            (Runtime::Claude, Self::Classifier) => Access::Auto,
            (Runtime::Claude, Self::Yolo) => Access::BypassPermissions,
            (Runtime::Antigravity, Self::Read) => Access::Plan,
            (Runtime::Antigravity, Self::Yolo) => Access::Yolo,
            (Runtime::Antigravity, _) => Access::AcceptEdits,
            (Runtime::OpenCode, _) => Access::Configured,
        }
    }
    /// Whether a native preset stays within this level. Unverified presets never do.
    pub fn allows(self, runtime: Runtime, access: Access) -> bool {
        Self::of(runtime, access).is_some_and(|level| level <= self)
    }
    /// Legacy `workspace_write`: false is read, true is write.
    pub fn of_write(write: bool) -> Self {
        if write { Self::Write } else { Self::Read }
    }
    pub fn needs_person(self) -> bool {
        self == Self::Yolo
    }
    /// The highest level below YOLO within `self`.
    pub fn below_yolo(self) -> Self {
        self.min(Self::Classifier)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Classifier => "classifier",
            Self::Yolo => "yolo",
        }
    }
    /// The native label on `runtime`, such as "Accept edits".
    pub fn label(self, runtime: Runtime) -> &'static str {
        self.preset(runtime).label()
    }
}

/// What a launcher asks for. `Inherit` is the default. On the wire it is a
/// string: `inherit`, a level name or a native preset name.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(with = "String"))]
#[serde(try_from = "String", into = "String")]
pub enum ModeRequest {
    #[default]
    Inherit,
    Level(Level),
}

impl TryFrom<String> for ModeRequest {
    type Error = String;
    fn try_from(text: String) -> Result<Self, String> {
        Self::parse(&text).ok_or_else(|| format!("Unknown permission mode {text:?}."))
    }
}
impl From<ModeRequest> for String {
    fn from(request: ModeRequest) -> Self {
        match request {
            ModeRequest::Inherit => "inherit".into(),
            ModeRequest::Level(level) => level.name().into(),
        }
    }
}

impl ModeRequest {
    /// Accepts `inherit`, a level name, or a native preset name of either runtime.
    pub fn parse(text: &str) -> Option<Self> {
        let level = match text
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "inherit" | "" => return Some(Self::Inherit),
            "read" | "read_only" | "readonly" | "plan" | "default" => Level::Read,
            "write" | "workspace" | "workspace_write" | "accept_edits" => Level::Write,
            "classifier" | "auto_review" | "auto" => Level::Classifier,
            "yolo" | "bypass" | "bypass_permissions" => Level::Yolo,
            _ => return None,
        };
        Some(Self::Level(level))
    }
    /// The request of an old caller that sent only `workspace_write`.
    pub fn legacy(workspace_write: Option<bool>) -> Self {
        workspace_write.map_or(Self::Inherit, |w| Self::Level(Level::of_write(w)))
    }
}

/// How a child's starting mode was decided. Recorded in its origin stamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// The receiving host set the starting level within the caller’s ceiling.
    HostPolicy,
    /// The source's level, mapped to the target runtime.
    Inherited,
    /// The launcher asked for a lower level.
    Lowered,
    /// An operator chose this level in a client sheet.
    Operator,
    /// Use the highest available level allowed under the request or runtime.
    Capped,
}

/// Who allowed a YOLO or bypass child. Recorded in its origin stamp.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum YoloAllowance {
    /// The host owner selected Always YOLO.
    HostPolicy,
    /// The worker inherited the verified active parent’s existing bypass mode.
    Inherited { session: String },
    /// The operator accepted this child's YOLO request on a device.
    Accepted { request: String, device: String },
    /// A grant stored with an ancestor session (or the project default) covered it.
    Grant {
        project: std::path::PathBuf,
        session: Option<String>,
    },
    /// The operator started the child in YOLO and confirmed it in the sheet.
    Operator { device: String },
}

/// The resolved start of a child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Resolved {
    pub level: Level,
    pub access: Access,
    pub decision: Decision,
}

impl Resolved {
    pub fn new(level: Level, runtime: Runtime, decision: Decision) -> Self {
        let supported = level.for_runtime(runtime);
        Self {
            level: supported,
            access: supported.preset(runtime),
            decision: if supported == level {
                decision
            } else {
                Decision::Capped
            },
        }
    }
}

/// Resolve an agent's request against the source's level. Refuses an
/// unverified source and any request above it before anything starts.
pub fn resolve(
    ceiling: Option<Level>,
    request: ModeRequest,
    target: Runtime,
) -> Result<Resolved, String> {
    let ceiling = ceiling
        .ok_or("The source session's permission mode is not verified, so it grants nothing.")?;
    let (level, decision) = match request {
        ModeRequest::Inherit => (ceiling, Decision::Inherited),
        ModeRequest::Level(l) if l > ceiling => {
            return Err(format!(
                "Requested mode {} is above the source's mode {}. A child can only lower it.",
                l.name(),
                ceiling.name()
            ));
        }
        ModeRequest::Level(l) if l == ceiling => (l, Decision::Inherited),
        ModeRequest::Level(l) => (l, Decision::Lowered),
    };
    Ok(Resolved::new(level, target, decision))
}

/// Resolve an operator's choice. A person may raise as well as lower; YOLO
/// still needs the confirmation the client shows for any YOLO change.
pub fn resolve_operator(
    source: Option<Level>,
    request: ModeRequest,
    target: Runtime,
) -> Result<Resolved, String> {
    match request {
        ModeRequest::Inherit => resolve(source, request, target),
        ModeRequest::Level(level) => Ok(Resolved::new(
            level,
            target,
            if Some(level) == source {
                Decision::Inherited
            } else {
                Decision::Operator
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Runtime::{Claude, Codex};

    #[test]
    fn the_mode_table_maps_both_ways_without_widening() {
        let table = [
            (Access::ReadOnly, Access::Plan, Level::Read),
            (Access::Workspace, Access::AcceptEdits, Level::Write),
            (Access::AutoReview, Access::Auto, Level::Classifier),
            (Access::Yolo, Access::BypassPermissions, Level::Yolo),
        ];
        for (codex, claude, level) in table {
            assert_eq!(Level::of(Codex, codex), Some(level));
            assert_eq!(Level::of(Claude, claude), Some(level));
            assert_eq!(level.preset(Codex), codex);
            assert_eq!(level.preset(Claude), claude);
            // Inheriting across runtimes lands on the mapped preset.
            let to_claude = resolve(Level::of(Codex, codex), ModeRequest::Inherit, Claude).unwrap();
            assert_eq!(
                (to_claude.access, to_claude.decision),
                (claude, Decision::Inherited)
            );
            let to_codex = resolve(Level::of(Claude, claude), ModeRequest::Inherit, Codex).unwrap();
            assert_eq!(to_codex.access, codex);
        }
        // Claude Default reads with prompts, so it maps to Codex Read only.
        assert_eq!(Level::of(Claude, Access::Default), Some(Level::Read));
        assert_eq!(
            resolve(Some(Level::Read), ModeRequest::Inherit, Codex)
                .unwrap()
                .access,
            Access::ReadOnly
        );
        // Every preset of each runtime is at or below its own level and no other.
        for runtime in Runtime::ALL {
            for access in Access::choices(runtime) {
                let level = Level::of(runtime, *access).expect("every choice is verified");
                for other in Level::ALL {
                    assert_eq!(other.allows(runtime, *access), level <= other, "{access:?}");
                }
                assert!(Level::of(runtime, level.preset(runtime)) == Some(level));
            }
        }
    }

    #[test]
    fn antigravity_starts_only_in_its_supported_modes() {
        let runtime = Runtime::Antigravity;
        let plan = resolve(Some(Level::Read), ModeRequest::Inherit, runtime).unwrap();
        assert_eq!(plan.access, Access::Plan);
        let capped = resolve(Some(Level::Classifier), ModeRequest::Inherit, runtime).unwrap();
        assert_eq!(capped.level, Level::Write);
        assert_eq!(capped.access, Access::AcceptEdits);
        assert_eq!(capped.decision, Decision::Capped);
        assert!(!capped.level.needs_person());
        let yolo = resolve_operator(None, ModeRequest::Level(Level::Yolo), runtime).unwrap();
        assert_eq!(yolo.level, Level::Yolo);
        assert_eq!(yolo.access, Access::Yolo);
        assert!(yolo.level.needs_person());
        assert!(Level::of(runtime, Access::BypassPermissions).is_none());
        assert!(resolve(Some(Level::Read), ModeRequest::Level(Level::Write), runtime).is_err());
    }

    #[test]
    fn unverified_modes_grant_nothing() {
        assert_eq!(Level::of(Codex, Access::Configured), None);
        // A preset reported by the other runtime is not a verified mode here.
        assert_eq!(Level::of(Codex, Access::AcceptEdits), None);
        assert_eq!(Level::of(Claude, Access::Workspace), None);
        for request in [ModeRequest::Inherit, ModeRequest::Level(Level::Read)] {
            assert!(resolve(None, request, Codex).is_err());
            assert!(resolve(None, request, Claude).is_err());
        }
        for level in Level::ALL {
            assert!(!level.allows(Codex, Access::Configured));
        }
    }

    #[test]
    fn requests_can_lower_and_never_raise() {
        for ceiling in Level::ALL {
            for wanted in Level::ALL {
                let r = resolve(Some(ceiling), ModeRequest::Level(wanted), Codex);
                if wanted > ceiling {
                    assert!(r.is_err(), "{wanted:?} above {ceiling:?}");
                } else {
                    let r = r.unwrap();
                    assert_eq!(r.level, wanted);
                    assert_eq!(
                        r.decision,
                        if wanted == ceiling {
                            Decision::Inherited
                        } else {
                            Decision::Lowered
                        }
                    );
                }
            }
        }
        // An operator may raise; the client confirms YOLO separately.
        let raised = resolve_operator(Some(Level::Read), ModeRequest::Level(Level::Yolo), Claude);
        assert_eq!(raised.unwrap().access, Access::BypassPermissions);
        assert!(resolve_operator(None, ModeRequest::Inherit, Claude).is_err());
    }

    #[test]
    fn legacy_workspace_write_and_names_parse() {
        assert_eq!(ModeRequest::legacy(None), ModeRequest::Inherit);
        assert_eq!(
            ModeRequest::legacy(Some(false)),
            ModeRequest::Level(Level::Read)
        );
        assert_eq!(
            ModeRequest::legacy(Some(true)),
            ModeRequest::Level(Level::Write)
        );
        for (text, level) in [
            ("read-only", Level::Read),
            ("Plan", Level::Read),
            ("accept_edits", Level::Write),
            ("workspace", Level::Write),
            ("auto-review", Level::Classifier),
            ("auto", Level::Classifier),
            ("bypass", Level::Yolo),
        ] {
            assert_eq!(ModeRequest::parse(text), Some(ModeRequest::Level(level)));
        }
        assert_eq!(ModeRequest::parse("inherit"), Some(ModeRequest::Inherit));
        assert_eq!(ModeRequest::parse("root"), None);
        // Old stamps recorded "read" and "write" and still decode.
        assert_eq!(
            serde_json::from_str::<Level>("\"write\"").unwrap(),
            Level::Write
        );
        assert_eq!(Level::Yolo.below_yolo(), Level::Classifier);
        assert_eq!(Level::Write.below_yolo(), Level::Write);
    }
}
