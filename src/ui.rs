//! Small presentation helpers shared by setup, doctor and launch errors.
//!
//! Everything here is pure or reads only the terminal state, so the module
//! can be included by path from integration tests.

use std::io::IsTerminal;

/// Whether stdout should receive ANSI colour: a terminal, `NO_COLOR` unset or
/// empty, and `TERM` not `dumb`.
pub(crate) fn stdout_color() -> bool {
    color_allowed(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
        std::env::var("TERM").ok().as_deref(),
    )
}

/// Same rule for stderr, where launch errors are printed.
pub(crate) fn stderr_color() -> bool {
    color_allowed(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
        std::env::var("TERM").ok().as_deref(),
    )
}

pub(crate) fn color_allowed(is_terminal: bool, no_color: bool, term: Option<&str>) -> bool {
    is_terminal && !no_color && term != Some("dumb")
}

pub(crate) const GREEN: &str = "32";
pub(crate) const YELLOW: &str = "33";
pub(crate) const RED: &str = "31";
pub(crate) const BOLD: &str = "1";
pub(crate) const DIM: &str = "2";

/// Wraps `text` in one SGR code when `color` is true; returns it unchanged
/// otherwise.
pub(crate) fn paint(text: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Returns the candidate closest to `input` when it is a plausible typo:
/// at most one edit for inputs up to three characters, two edits up to
/// seven, three beyond that. Comparison ignores ASCII case. An exact match is
/// returned as is. Ties keep the first candidate.
pub(crate) fn closest<'a>(
    input: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let input = input.to_ascii_lowercase();
    let length = input.chars().count();
    if length == 0 {
        return None;
    }
    let limit = match length {
        1..=3 => 1,
        4..=7 => 2,
        _ => 3,
    };
    let mut best: Option<(usize, &'a str)> = None;
    for candidate in candidates {
        let distance = edit_distance(&input, &candidate.to_ascii_lowercase());
        if distance <= limit && best.is_none_or(|(current, _)| distance < current) {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (row, a) in left.iter().enumerate() {
        let mut current = vec![row + 1];
        for (column, b) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(a != b);
            let insertion = current[column] + 1;
            let deletion = previous[column + 1] + 1;
            current.push(substitution.min(insertion).min(deletion));
        }
        previous = current;
    }
    previous[right.len()]
}

/// The one command that installs a stock client on this platform.
pub(crate) fn install_command(client: &str) -> &'static str {
    match client {
        "claude" => "curl -fsSL https://claude.ai/install.sh | bash",
        "opencode" => "npm install -g opencode-ai@latest",
        _ if cfg!(target_os = "macos") => "brew install --cask codex",
        _ => "npm install -g @openai/codex",
    }
}

/// Display name for a stock client binary.
pub(crate) fn client_name(client: &str) -> &'static str {
    match client {
        "claude" => "Claude Code",
        "opencode" => "OpenCode",
        _ => "Codex",
    }
}

/// A "fix" line for errors whose remedy is always the same command. Matching
/// is by message prefix so every call site that produces the error benefits.
pub(crate) fn fix_hint(message: &str) -> Option<&'static str> {
    const HINTS: &[(&str, &str)] = &[
        (
            "could not parse codeox.toml",
            "ox doctor --fix   (keeps a backup of the file, then run ox setup)",
        ),
        (
            "codeox.toml needs a [providers.",
            "ox doctor --fix   (keeps a backup of the file, then run ox setup)",
        ),
        (
            "codeox.toml must configure at least one provider",
            "ox doctor --fix   (keeps a backup of the file, then run ox setup)",
        ),
        (
            "could not parse ox-api-keys.json",
            "ox doctor --fix   (keeps a backup of the file, then paste your keys again in ox setup)",
        ),
        (
            "ox-api-keys.json must be",
            "ox doctor   (it explains what is wrong with the saved key file)",
        ),
        (
            "no models are ready to launch",
            "ox doctor   (it names the provider or key that needs attention)",
        ),
    ];
    HINTS
        .iter()
        .find(|(prefix, _)| message.starts_with(prefix))
        .map(|(_, hint)| *hint)
}

/// Formats an error for stderr as `<prefix>: <message>`, then a `fix:` line
/// when one is known and the message does not already carry one.
pub(crate) fn render_error(prefix: &str, message: &str, color: bool) -> String {
    let mut out = format!("{} {message}", paint(&format!("{prefix}:"), RED, color));
    if !message.contains("\n  fix: ")
        && let Some(hint) = fix_hint(message)
    {
        out.push_str(&format!("\n  fix: {hint}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_needs_a_terminal_and_respects_no_color_and_dumb_terminals() {
        assert!(color_allowed(true, false, Some("xterm-256color")));
        assert!(color_allowed(true, false, None));
        assert!(!color_allowed(false, false, Some("xterm-256color")));
        assert!(!color_allowed(true, true, Some("xterm-256color")));
        assert!(!color_allowed(true, false, Some("dumb")));
    }

    #[test]
    fn paint_is_plain_without_color() {
        assert_eq!(paint("ok", GREEN, false), "ok");
        assert_eq!(paint("ok", GREEN, true), "\x1b[32mok\x1b[0m");
    }

    #[test]
    fn closest_finds_plausible_typos_only() {
        let commands = ["setup", "doctor", "provider", "serve", "update"];
        assert_eq!(closest("docter", commands), Some("doctor"));
        assert_eq!(closest("setpu", commands), Some("setup"));
        assert_eq!(closest("sevre", commands), Some("serve"));
        assert_eq!(closest("Doctor", commands), Some("doctor"));
        assert_eq!(closest("providr", commands), Some("provider"));
        assert_eq!(closest("exec", commands), None);
        assert_eq!(closest("resume", commands), None);
        assert_eq!(closest("review", commands), None);
        assert_eq!(closest("debug", commands), None);
        assert_eq!(closest("logout", ["login"]), None);
        assert_eq!(closest("", commands), None);
        assert_eq!(
            closest("openroutr", ["anthropic", "openrouter"]),
            Some("openrouter")
        );
    }

    #[test]
    fn known_errors_get_one_fix_line() {
        let rendered = render_error("ox", "could not parse codeox.toml", false);
        assert_eq!(
            rendered,
            "ox: could not parse codeox.toml\n  fix: ox doctor --fix   (keeps a backup of the file, then run ox setup)"
        );
        assert_eq!(
            render_error("ox setup", "anything else", false),
            "ox setup: anything else"
        );
        let already = "port 8787 is already in use\n  fix: ox serve --port 8788";
        assert_eq!(render_error("ox", already, false), format!("ox: {already}"));
    }

    #[test]
    fn install_commands_are_copyable() {
        assert!(install_command("claude").contains("claude.ai/install.sh"));
        assert!(install_command("codex").contains("codex"));
        assert_eq!(client_name("claude"), "Claude Code");
        assert_eq!(client_name("codex"), "Codex");
    }
}
