//! Bound layout work even for generated single-line patches. Copy uses the original data.
use ox_desk_core::transcript::item::FileChange;
use std::{
    fmt::Write,
    ops::Range,
    sync::{Arc, Weak},
};

#[derive(Clone, Default)]
pub(super) struct Cursor {
    file: usize,
    byte: usize,
    source: Option<Weak<str>>,
    hunk: Option<Hunk>,
    numbers: Option<Numbers>,
}

#[derive(Clone, Copy)]
struct Hunk {
    start: usize,
    before: RangeCursor,
    after: RangeCursor,
}

#[derive(Clone, Copy)]
struct RangeCursor {
    next: usize,
    left: usize,
}

impl RangeCursor {
    fn parse(s: &str) -> Option<Self> {
        let (start, count) = s.split_once(',').unwrap_or((s, "1"));
        let next: usize = start.parse().ok()?;
        let left: usize = count.parse().ok()?;
        if left > 0 {
            next.checked_add(left)?;
        }
        Some(Self { next, left })
    }

    fn take(&mut self) -> Option<usize> {
        if self.left == 0 {
            return None;
        }
        let number = self.next;
        self.left -= 1;
        self.next += 1;
        Some(number)
    }
}

impl Hunk {
    fn parse(line: &str, start: usize) -> Option<Self> {
        let mut fields = line.strip_prefix("@@ ")?.split_ascii_whitespace();
        let before = RangeCursor::parse(fields.next()?.strip_prefix('-')?)?;
        let after = RangeCursor::parse(fields.next()?.strip_prefix('+')?)?;
        (fields.next() == Some("@@")).then_some(Self {
            start,
            before,
            after,
        })
    }

    fn numbers(&mut self, prefix: u8) -> Option<Numbers> {
        let (before, after) = match prefix {
            b' ' if self.before.left > 0 && self.after.left > 0 => {
                (self.before.take(), self.after.take())
            }
            b'-' => (Some(self.before.take()?), None),
            b'+' => (None, Some(self.after.take()?)),
            _ => return None,
        };
        Some(Numbers { before, after })
    }
}

#[derive(Clone, Copy)]
struct Numbers {
    before: Option<usize>,
    after: Option<usize>,
}

#[derive(Clone)]
pub(super) struct Location {
    file: usize,
    hunk: usize,
    bytes: Range<usize>,
    numbers: Numbers,
}

impl Location {
    pub fn source<'a>(&self, patches: &[&'a FileChange]) -> &'a Arc<str> {
        &patches[self.file].diff
    }

    pub fn label(&self) -> String {
        match (self.numbers.before, self.numbers.after) {
            (Some(before), None) => format!("−{before}"),
            (None, Some(after)) => format!("+{after}"),
            (_, Some(after)) => after.to_string(),
            (None, None) => unreachable!("a known location has line numbers"),
        }
    }

    pub fn description(&self) -> String {
        let Numbers { before, after } = self.numbers;
        match (before, after) {
            (Some(before), Some(after)) => {
                format!("Select context line {before} before, {after} after")
            }
            (Some(before), None) => format!("Select removed line {before}"),
            (None, Some(after)) => format!("Select added line {after}"),
            (None, None) => unreachable!("a known location has line numbers"),
        }
    }
}

pub(super) struct Line {
    pub text: String,
    pub location: Option<Location>,
}

pub(super) struct Page {
    pub lines: Vec<Line>,
    pub next: Option<Cursor>,
}

pub(super) fn page(patches: &[&FileChange], mut at: Cursor) -> Page {
    const LINE_LIMIT: usize = 64;
    const BYTES_PER_LINE: usize = 512;
    let mut lines = Vec::new();
    if patches.get(at.file).is_none_or(|patch| {
        at.source
            .as_ref()
            .is_some_and(|source| !Weak::ptr_eq(source, &Arc::downgrade(&patch.diff)))
            || at.byte > patch.diff.len()
            || !patch.diff.is_char_boundary(at.byte)
    }) {
        at = Cursor::default();
    }
    while let Some(patch) = patches.get(at.file) {
        if at.byte == patch.diff.len() {
            at = Cursor {
                file: at.file + 1,
                ..Default::default()
            };
            continue;
        }
        if lines.len() == LINE_LIMIT {
            at.source = Some(Arc::downgrade(&patch.diff));
            return Page {
                lines,
                next: Some(at),
            };
        }
        let remaining = &patch.diff[at.byte..];
        let bounded =
            &remaining[..remaining.floor_char_boundary(remaining.len().min(BYTES_PER_LINE))];
        let chunk = bounded.split_inclusive('\n').next().unwrap();
        if at.byte == 0 || patch.diff.as_bytes()[at.byte - 1] == b'\n' {
            if chunk.starts_with("@@") {
                at.hunk = Hunk::parse(chunk, at.byte);
            } else if chunk.starts_with("diff --git ") {
                at.hunk = None;
            }
            at.numbers = at
                .hunk
                .as_mut()
                .and_then(|hunk| hunk.numbers(chunk.as_bytes()[0]));
        }
        lines.push(Line {
            text: chunk.trim_end_matches(['\r', '\n']).to_owned(),
            location: at.numbers.zip(at.hunk).map(|(numbers, hunk)| Location {
                file: at.file,
                hunk: hunk.start,
                bytes: at.byte..at.byte + chunk.len(),
                numbers,
            }),
        });
        at.byte += chunk.len();
    }
    Page { lines, next: None }
}

/// The anchors belong to one displayed patch revision. The original text stays shareable
/// with the transcript until the user adds or discards the selection.
pub(super) struct Selection {
    source: Arc<str>,
    anchor: Location,
    end: Location,
}

pub(super) struct Excerpt {
    pub label: String,
    file: usize,
    source: Arc<str>,
    bytes: Range<usize>,
}

impl Excerpt {
    pub fn matches(&self, patches: &[&FileChange], range: Option<&Selection>) -> bool {
        patches
            .get(self.file)
            .is_some_and(|patch| Arc::ptr_eq(&patch.diff, &self.source))
            && range.is_some_and(|range| {
                Arc::ptr_eq(&range.source, &self.source)
                    && range.anchor.file == self.file
                    && range.bytes() == self.bytes
            })
    }

    pub fn append_to(&self, mut draft: String, path: &str) -> String {
        if !draft.is_empty() && !draft.ends_with("\n\n") {
            if !draft.ends_with('\n') {
                draft.push('\n');
            }
            draft.push('\n');
        }
        let excerpt = &self.source[self.bytes.clone()];
        // A wrapped row can start with a fence in the middle of its original line.
        let fence = "`".repeat(
            excerpt
                .split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or_default()
                .saturating_add(1)
                .max(3),
        );
        let _ = writeln!(draft, "Review context for {path:?} ({})", self.label);
        let _ = writeln!(draft, "{fence}diff");
        draft.push_str(excerpt);
        if !draft.ends_with('\n') {
            draft.push('\n');
        }
        let _ = writeln!(draft, "{fence}\n");
        draft
    }
}

impl Selection {
    pub fn select(previous: Option<Self>, source: Arc<str>, line: Location) -> Self {
        let anchor = previous
            .filter(|previous| previous.same_hunk(&source, &line))
            .map_or_else(|| line.clone(), |previous| previous.anchor);
        Self {
            source,
            anchor,
            end: line,
        }
    }

    fn same_hunk(&self, source: &Arc<str>, line: &Location) -> bool {
        Arc::ptr_eq(&self.source, source)
            && self.anchor.file == line.file
            && self.anchor.hunk == line.hunk
    }

    fn bytes(&self) -> Range<usize> {
        self.anchor.bytes.start.min(self.end.bytes.start)
            ..self.anchor.bytes.end.max(self.end.bytes.end)
    }

    pub fn includes(&self, patches: &[&FileChange], line: &Location) -> bool {
        self.same_hunk(line.source(patches), line) && self.bytes().contains(&line.bytes.start)
    }

    pub fn excerpt(&self, patches: &[&FileChange], lines: &[Line]) -> Option<Excerpt> {
        let mut before = (None, None);
        let mut after = (None, None);
        let mut edges = (false, false);
        for line in lines.iter().filter_map(|line| line.location.as_ref()) {
            if !self.includes(patches, line) {
                continue;
            }
            edges.0 |= line.bytes.start == self.anchor.bytes.start;
            edges.1 |= line.bytes.start == self.end.bytes.start;
            if let Some(n) = line.numbers.before {
                before.0.get_or_insert(n);
                before.1 = Some(n);
            }
            if let Some(n) = line.numbers.after {
                after.0.get_or_insert(n);
                after.1 = Some(n);
            }
        }
        if !edges.0 || !edges.1 {
            return None;
        }
        let bytes = self.bytes();
        let partial = (bytes.start > 0 && self.source.as_bytes()[bytes.start - 1] != b'\n')
            || (bytes.end < self.source.len() && self.source.as_bytes()[bytes.end - 1] != b'\n');
        let mut labels = Vec::with_capacity(2);
        for (name, (start, end)) in [("before", before), ("after", after)] {
            if let Some((start, end)) = start.zip(end) {
                labels.push(if start == end {
                    format!("{name} line {start}")
                } else {
                    format!("{name} lines {start}–{end}")
                });
            }
        }
        let mut label = labels.join(", ");
        if partial {
            label.push_str(" (partial line)");
        }
        Some(Excerpt {
            label,
            file: self.anchor.file,
            source: self.source.clone(),
            bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(text: &str) -> FileChange {
        FileChange {
            path: "src/cache.rs".into(),
            diff: text.into(),
        }
    }

    #[test]
    fn range_keeps_both_revision_coordinates_and_the_existing_draft() {
        let file = file(
            "--- a/src/cache.rs\n+++ b/src/cache.rs\n@@ -6,2 +10,3 @@\n keep\n-old\n+new\n+tail\n",
        );
        let patches = [&file];
        let page = page(&patches, Cursor::default());
        let choose = |range, index: usize| {
            let location = page.lines[index].location.as_ref().unwrap();
            Selection::select(range, location.source(&patches).clone(), location.clone())
        };
        assert!(page.lines[..3].iter().all(|line| line.location.is_none()));
        let range = choose(Some(choose(None, 6)), 4);
        let excerpt = range.excerpt(&patches, &page.lines).unwrap();
        assert_eq!(excerpt.label, "before line 7, after lines 11–12");
        assert_eq!(
            excerpt.append_to("Check the timeout".into(), "src/cache.rs"),
            "Check the timeout\n\nReview context for \"src/cache.rs\" (before line 7, after lines 11–12)\n```diff\n-old\n+new\n+tail\n```\n\n"
        );
        let replacement = self::file("@@ -6 +10 @@\n-old\n+different\n");
        let replacement_patches = [&replacement];
        let replacement_page = self::page(&replacement_patches, Cursor::default());
        assert!(
            range
                .excerpt(&replacement_patches, &replacement_page.lines)
                .is_none()
        );
        assert!(excerpt.matches(&patches, Some(&range)));
        assert!(!excerpt.matches(&replacement_patches, Some(&range)));
    }

    #[test]
    fn hunk_and_page_boundaries_do_not_invent_contiguous_lines() {
        let body = format!(
            "@@ -1,64 +1,64 @@\n{}@@ -140 +152 @@\n-end\n+new\n",
            " context\n".repeat(64)
        );
        let patch = file(&body);
        let patches = [&patch];
        let first = page(&patches, Cursor::default());
        let next = first.next.unwrap();
        let second = page(&patches, next.clone());
        let select = |previous, index: usize| {
            let location = second.lines[index].location.as_ref().unwrap();
            Selection::select(
                previous,
                location.source(&patches).clone(),
                location.clone(),
            )
        };
        assert_eq!(second.lines[0].location.as_ref().unwrap().label(), "64");
        let selection = select(Some(select(None, 0)), 2);
        assert_eq!(
            selection.excerpt(&patches, &second.lines).unwrap().label,
            "before line 140"
        );
        let replacement = file("@@ -0,0 +1 @@\n+short\n");
        let reset = page(&[&replacement], next);
        assert_eq!(reset.lines[1].location.as_ref().unwrap().label(), "+1");
    }

    #[test]
    fn wrapped_rows_report_exact_partial_text_without_advancing_the_line() {
        let body = format!("@@ -0,0 +4,2 @@\n+{}\n+tail\n", "`".repeat(540));
        let patch = file(&body);
        let patches = [&patch];
        let page = page(&patches, Cursor::default());
        let line = page.lines[2].location.as_ref().unwrap();
        let selection = Selection::select(None, line.source(&patches).clone(), line.clone());
        let excerpt = selection.excerpt(&patches, &page.lines).unwrap();
        let text = excerpt.append_to(String::new(), "src/cache.rs");
        assert_eq!(excerpt.label, "after line 4 (partial line)");
        assert!(text.contains(&format!("\n{}diff\n{}\n", "`".repeat(30), "`".repeat(29))));
        assert_eq!(page.lines[3].location.as_ref().unwrap().label(), "+5");
    }

    #[test]
    fn oversized_unicode_lines_and_all_subsequent_edits_stay_reachable() {
        let text = "🌍".repeat(12000);
        let first = FileChange {
            path: "f".into(),
            diff: text.clone().into(),
        };
        let second = FileChange {
            path: "f".into(),
            diff: "last line".into(),
        };
        let files = [&first, &second];
        let mut cursor = Cursor::default();
        let mut displayed = String::new();
        let mut pages = 0;
        loop {
            let page = page(&files, cursor);
            assert!(page.lines.len() <= 64 && page.lines.iter().all(|s| s.text.len() <= 512));
            displayed.extend(page.lines.into_iter().map(|line| line.text));
            pages += 1;
            let Some(next) = page.next else { break };
            cursor = next;
        }
        assert!(pages > 1);
        assert_eq!(displayed, format!("{text}last line"));
    }
}
