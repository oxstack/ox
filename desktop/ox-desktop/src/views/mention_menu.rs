use super::workbench::Workbench;
use crate::{
    appearance::{LABEL_FONT, Palette, Step},
    kit::{button, col, row, small},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::ButtonVariants;
use gpui_component::input::{Textarea, TextareaState};
use ox_desk_core::{
    actions::Action as DeskAction,
    mentions::{ImageInput, token},
    store::Store,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

fn file_image(path: &Path) -> bool {
    path.is_absolute()
        && path.extension().and_then(|v| v.to_str()).is_some_and(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif"
            )
        })
}

fn file_heic(path: &Path) -> bool {
    path.is_absolute()
        && path
            .extension()
            .and_then(|v| v.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "heic" | "heif"))
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", (bytes + 512) / 1024)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn image_size_label(img: &ImageInput) -> String {
    match img {
        ImageInput::File(path) => std::fs::metadata(path)
            .ok()
            .map(|m| format_bytes(m.len()))
            .unwrap_or_default(),
        ImageInput::Clipboard(_, data) => {
            let b64_len = data.split_once(',').map_or(data.len(), |(_, b)| b.len());
            format_bytes((b64_len as u64 * 3) / 4)
        }
    }
}

pub fn format_file_mention_tag(project: &Path, path: &Path) -> String {
    let display = path.strip_prefix(project).unwrap_or(path).to_string_lossy();
    if display.chars().any(char::is_whitespace) {
        format!("@{}", serde_json::json!(display.as_ref()))
    } else {
        format!("@{display}")
    }
}

fn mention_spans_in_text(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut out = Vec::new();
    let mut offset = 0;
    while let Some(rel) = text[offset..].find('@') {
        let start = offset + rel;
        let before_ok = start == 0
            || text[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let after_at = start + 1;
        if !before_ok || after_at >= text.len() {
            offset = after_at;
            continue;
        }
        let rest = &text[after_at..];
        let mention = if rest.starts_with('"') {
            let mut end = 1;
            let bytes = rest.as_bytes();
            while end < bytes.len() {
                if bytes[end] == b'\\' {
                    end += 2;
                    continue;
                }
                if bytes[end] == b'"' {
                    end += 1;
                    break;
                }
                end += 1;
            }
            &rest[..end.min(rest.len())]
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            &rest[..end]
        };
        if !mention.is_empty() {
            let end = after_at + mention.len();
            out.push((start..end, format!("@{mention}")));
            offset = end;
        } else {
            offset = after_at;
        }
    }
    out
}

pub fn mentions_in_text(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (_, tag) in mention_spans_in_text(text) {
        if !out.contains(&tag) {
            out.push(tag);
        }
    }
    out
}

/// Removes every complete parsed mention token matching `target_tag` (plus one trailing space if present),
/// without touching longer mentions that share a prefix or email addresses.
pub fn remove_mention_from_text(text: &str, target_tag: &str) -> String {
    let spans: Vec<std::ops::Range<usize>> = mention_spans_in_text(text)
        .into_iter()
        .filter(|(_, tag)| tag == target_tag)
        .map(|(range, _)| range)
        .collect();
    if spans.is_empty() {
        return text.to_owned();
    }
    let mut result = text.to_owned();
    for range in spans.into_iter().rev() {
        let mut end = range.end;
        if result[end..].starts_with(' ') {
            end += 1;
        }
        result.replace_range(range.start..end, "");
    }
    result
}

fn mime(format: ImageFormat) -> Option<&'static str> {
    Some(match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::Webp => "image/webp",
        _ => return None,
    })
}
impl Workbench {
    pub(super) fn search_mention(
        &mut self,
        owner: Option<String>,
        editor: Entity<TextareaState>,
        cx: &mut Context<Self>,
    ) {
        let read = editor.read(cx);
        let value = read.value();
        let Some(store) = &mut self.store else {
            return;
        };
        if store.selected() != owner.as_deref() {
            return;
        }
        let range = token(&value, read.cursor());
        self.mention_index = 0;
        self.mention_task = None;
        let Some(range) = range else {
            let _ = store.find_mention(owner.as_deref(), None);
            return;
        };
        let query = value[range.start + 1..range.end].to_owned();
        self.mention_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(110))
                .await;
            let _ = this.update(cx, |s, cx| {
                if let Some(store) = &mut s.store
                    && store.selected() == owner.as_deref()
                {
                    if let Err(err) = store.find_mention(owner.as_deref(), Some(&query)) {
                        s.launch_error = Some(err.to_string());
                    }
                    let _ = s.wake.try_send(());
                    cx.notify();
                }
            });
        }));
    }
    pub(super) fn mention_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        let owner = store.selected().map(str::to_owned);
        let Some(editor) = self.editors.get(&owner) else {
            return false;
        };
        if !editor.read(cx).focus_handle(cx).is_focused(window)
            || editor.update(cx, |f, cx| f.marked_text_range(window, cx).is_some())
        {
            return false;
        }
        let state = editor.read(cx);
        let value = state.value();
        let Some(range) = token(&value, state.cursor()) else {
            return false;
        };
        let query = &value[range.start + 1..range.end];
        let (matches, _) = store.mention_matches(owner.as_deref(), query);
        if matches.is_empty() {
            return false;
        };
        let last = matches.len() - 1;
        self.mention_index = self.mention_index.min(last);
        match key {
            "up" => self.mention_index = self.mention_index.saturating_sub(1),
            "down" => self.mention_index = (self.mention_index + 1).min(last),
            "tab" => {
                let path = matches[self.mention_index].to_string_lossy().into_owned();
                insert_mention(editor, query, &path, window, cx);
            }
            _ => return false,
        }
        cx.notify();
        true
    }
    pub(super) fn prompt_attach_files(
        &mut self,
        owner: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let choice = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach images or files".into()),
        });
        cx.spawn_in(window, async move |s, cx| {
            let result = choice.await;
            let _ = s.update_in(cx, |s, _, cx| match result {
                Ok(Ok(Some(paths))) if !paths.is_empty() => {
                    s.attach_files(owner.as_deref(), &paths, cx);
                }
                Ok(Err(err)) => {
                    s.launch_error = Some(err.to_string());
                    cx.notify();
                }
                _ => (),
            });
        })
        .detach();
    }

    pub(super) fn attach_files(
        &mut self,
        owner: Option<&str>,
        paths: &[PathBuf],
        cx: &mut Context<Self>,
    ) {
        if self.route.runtime == ox_desk_core::runtime::Runtime::Antigravity {
            return;
        }
        let project = self.project.clone();
        let Some(store) = &mut self.store else {
            return;
        };
        if store.selected() != owner {
            return;
        }
        if self.link.remote().is_some() {
            let paths = paths.to_vec();
            let task = cx
                .background_spawn(async move { crate::platform::attachments::read_images(&paths) });
            self.prepare_images(owner.map(str::to_owned), task, cx);
            return;
        }
        let owner_key = owner.map(str::to_owned);
        let mut heic_paths = Vec::new();
        let mut mention_tags = Vec::new();
        for path in paths {
            if file_image(path) {
                if let Ok(meta) = std::fs::metadata(path)
                    && meta.len() > MAX_IMAGE_BYTES
                {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    self.launch_error = Some(format!(
                        "{name} is {} (limit is {}).",
                        format_bytes(meta.len()),
                        format_bytes(MAX_IMAGE_BYTES)
                    ));
                    continue;
                }
                store.attach_image(owner, ImageInput::File(path.clone()));
            } else if file_heic(path) {
                if let Ok(meta) = std::fs::metadata(path)
                    && meta.len() > MAX_IMAGE_BYTES * 2
                {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    self.launch_error = Some(format!(
                        "{name} is {} (limit is {}).",
                        format_bytes(meta.len()),
                        format_bytes(MAX_IMAGE_BYTES * 2)
                    ));
                    continue;
                }
                heic_paths.push(path.clone());
            } else if path.is_absolute() {
                mention_tags.push(format_file_mention_tag(&project, path));
            } else {
                self.launch_error = Some("Choose a PNG, JPEG, GIF or WebP image.".into());
            }
        }
        if !mention_tags.is_empty() {
            let mut draft = store.draft(owner).to_owned();
            let mut existing = mentions_in_text(&draft);
            for tag in mention_tags {
                if !existing.contains(&tag) {
                    if !draft.is_empty() && !draft.ends_with(char::is_whitespace) {
                        draft.push(' ');
                    }
                    draft.push_str(&tag);
                    draft.push(' ');
                    existing.push(tag);
                }
            }
            let _ = store.dispatch(DeskAction::DraftEdit(owner_key.clone(), draft));
        }
        if !heic_paths.is_empty() {
            *self.image_work.entry(owner_key.clone()).or_default() += 1;
            let convert = cx.background_spawn(async move {
                let mut converted = Vec::new();
                let mut errors = Vec::new();
                for path in heic_paths {
                    match crate::platform::convert_heic_to_png(&path) {
                        Ok(bytes) => {
                            if bytes.len() as u64 > MAX_IMAGE_BYTES {
                                let name = path.file_name().unwrap_or_default().to_string_lossy();
                                errors.push(format!(
                                    "Converted {name} is {} (limit is {}).",
                                    format_bytes(bytes.len() as u64),
                                    format_bytes(MAX_IMAGE_BYTES)
                                ));
                            } else {
                                converted.push(ImageInput::Clipboard(
                                    "image/png".into(),
                                    Arc::from(format!(
                                        "data:image/png;base64,{}",
                                        STANDARD.encode(bytes)
                                    )),
                                ));
                            }
                        }
                        Err(err) => errors.push(err),
                    }
                }
                (converted, errors)
            });
            cx.spawn(async move |this, cx| {
                let (inputs, errors) = convert.await;
                let _ = this.update(cx, |s, cx| {
                    if let Some(store) = &mut s.store {
                        for image in inputs {
                            store.attach_image(owner_key.as_deref(), image);
                        }
                    }
                    if let Some(err) = errors.into_iter().next() {
                        s.launch_error = Some(err);
                    }
                    if let Some(count) = s.image_work.get_mut(&owner_key) {
                        *count -= 1;
                        if *count == 0 {
                            s.image_work.remove(&owner_key);
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }
    fn paste_images(
        &mut self,
        owner: Option<String>,
        clip: &ClipboardItem,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.route.runtime == ox_desk_core::runtime::Runtime::Antigravity
            || self
                .store
                .as_ref()
                .is_none_or(|s| s.selected() != owner.as_deref())
        {
            return false;
        }
        let mut images = vec![];
        let mut handled = false;
        for entry in clip.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    handled = true;
                    if let Some(mime) = mime(image.format) {
                        if image.bytes.len() as u64 > MAX_IMAGE_BYTES {
                            self.launch_error = Some(format!(
                                "Pasted image is {} (limit is {}).",
                                format_bytes(image.bytes.len() as u64),
                                format_bytes(MAX_IMAGE_BYTES)
                            ));
                            continue;
                        }
                        images.push((mime, image.bytes.clone()));
                    } else {
                        self.launch_error = Some("Paste a PNG, JPEG, GIF or WebP image.".into());
                    }
                }
                ClipboardEntry::ExternalPaths(paths) if !paths.paths().is_empty() => {
                    self.attach_files(owner.as_deref(), paths.paths(), cx);
                    handled = true;
                }
                _ => {}
            }
        }
        if !images.is_empty() {
            let remote = self.link.remote().is_some();
            let convert = cx.background_spawn(async move {
                if remote {
                    crate::platform::attachments::check_limits(
                        images.len(),
                        images.iter().map(|(_, b)| b.len()).sum(),
                    )?;
                }
                Ok(images
                    .into_iter()
                    .map(|(mime, bytes)| crate::platform::attachments::encode(mime, &bytes))
                    .collect())
            });
            self.prepare_images(owner, convert, cx);
        }
        if handled {
            cx.notify();
        }
        handled
    }
    fn prepare_images(
        &mut self,
        owner: Option<String>,
        task: Task<Result<Vec<ImageInput>, String>>,
        cx: &mut Context<Self>,
    ) {
        *self.image_work.entry(owner.clone()).or_default() += 1;
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                if let Some(store) = &mut s.store {
                    let result = result.and_then(|added| {
                        if s.link.remote().is_some() {
                            crate::platform::attachments::check_draft(
                                store.draft_images(owner.as_deref()),
                                &added,
                            )?;
                        }
                        Ok(added)
                    });
                    match result {
                        Ok(images) => {
                            for image in images {
                                store.attach_image(owner.as_deref(), image);
                            }
                        }
                        Err(error) => s.launch_error = Some(error),
                    }
                }
                if let Some(count) = s.image_work.get_mut(&owner) {
                    *count -= 1;
                    if *count == 0 {
                        s.image_work.remove(&owner);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
fn insert_mention(
    input: &Entity<TextareaState>,
    expected: &str,
    chosen: &str,
    window: &mut Window,
    cx: &mut App,
) {
    input.update(cx, |edit, cx| {
        let text = edit.value();
        let Some(range) = token(&text, edit.cursor()) else {
            return;
        };
        if &text[range.start + 1..range.end] != expected {
            return;
        }
        let tag = if chosen.chars().any(char::is_whitespace) {
            format!("@{} ", serde_json::json!(chosen))
        } else {
            format!("@{chosen} ")
        };
        edit.set_selected_range(range, cx);
        edit.replace(tag, window, cx);
        edit.focus_handle(cx).focus(window, cx);
    });
}
/// A tiny 16×16 pixel-art thumbnail icon for attached images in the composer.
fn image_thumb_icon(p: Palette) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .size(px(16.))
        .rounded(px(2.))
        .bg(rgb(p.nav))
        .border_1()
        .border_color(rgb(p.line_strong))
        .flex()
        .items_center()
        .justify_center()
        .child(div().size(px(8.)).rounded(px(1.)).bg(rgb(p.accent)))
}

pub(super) fn composer_input(
    editor: Entity<TextareaState>,
    owner: Option<String>,
    store: &Store,
    encoding: bool,
    current_match: usize,
    cx: &mut Context<Workbench>,
) -> Div {
    let p = Palette::get(cx);
    let view = cx.weak_entity();
    let dest = owner.clone();
    let pasted = owner.clone();
    let dragging_owner = owner.clone();
    let can_attach = store.profile().route.runtime != ox_desk_core::runtime::Runtime::Antigravity;
    let mut panel = col()
        .relative()
        .gap_1()
        .on_drag_move::<ExternalPaths>(cx.listener(
            move |s, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                let inside = can_attach && event.bounds.contains(&event.event.position);
                if inside {
                    let files = event.drag(cx).paths();
                    if s.file_hover
                        .as_ref()
                        .is_none_or(|(id, paths)| id != &dragging_owner || paths != files)
                    {
                        s.file_hover = Some((dragging_owner.clone(), files.to_vec()));
                        cx.notify();
                    }
                } else if s.file_hover.take().is_some() {
                    cx.notify();
                }
            },
        ))
        .on_drop(cx.listener(move |s, paths: &ExternalPaths, _, cx| {
            s.file_hover = None;
            s.attach_files(dest.as_deref(), paths.paths(), cx);
        }))
        .child(
            Textarea::new(&editor)
                .appearance(false)
                .on_paste(move |clip, _, cx| {
                    view.update(cx, |s, cx| s.paste_images(pasted.clone(), clip, cx))
                        .unwrap_or(false)
                }),
        );
    let value = editor.read(cx).value();
    if let Some(range) = token(&value, editor.read(cx).cursor()) {
        let query = value[range.start + 1..range.end].to_owned();
        let (files, error) = store.mention_matches(owner.as_deref(), &query);
        if let Some(error) = error {
            panel = panel.child(small(p, error.to_owned()));
        }
        let mut matches = col()
            .id("mention-results")
            .max_h(px(130.))
            .overflow_y_scroll()
            .gap_1();
        for (i, path) in files.iter().enumerate() {
            let name = path.to_string_lossy().into_owned();
            let chosen = name.clone();
            let input = editor.clone();
            let expected = query.clone();
            let owner = owner.clone();
            matches = matches.child(button(
                p,
                ("mention", i),
                name,
                i == current_match,
                true,
                cx.listener(move |s, _, window, cx| {
                    if s.store
                        .as_ref()
                        .is_none_or(|store| store.selected() != owner.as_deref())
                    {
                        return;
                    }
                    insert_mention(&input, &expected, &chosen, window, cx);
                }),
            ));
        }
        if !files.is_empty() {
            panel = panel.child(matches);
        }
    }
    if encoding {
        panel = panel.child(small(p, "Preparing attachment…"));
    }
    let file_mentions = mentions_in_text(&value);
    let mut chips = row().gap_1().flex_wrap();
    for (i, img) in store.draft_images(owner.as_deref()).iter().enumerate() {
        let target = owner.clone();
        let name = match img {
            ImageInput::File(_) => img.label(),
            ImageInput::Clipboard(mime, _) => {
                format!(
                    "{} image",
                    mime.trim_start_matches("image/").to_ascii_uppercase()
                )
            }
        };
        let size_label = image_size_label(img);
        let probe_label = if size_label.is_empty() {
            format!("{name}   ×")
        } else {
            format!("{name} ({size_label})   ×")
        };
        let thumb: AnyElement = match img {
            ImageInput::File(path) => gpui::img(path.clone())
                .size(px(18.))
                .rounded(px(2.))
                .object_fit(ObjectFit::Cover)
                .into_any_element(),
            ImageInput::Clipboard(..) => image_thumb_icon(p).into_any_element(),
        };
        let id = ElementId::from(("attached-image", i));
        let probe_label = SharedString::from(probe_label);
        let chip = gpui_component::button::Button::new(id.clone())
            .ghost()
            .h(px(26.))
            .px(px(6.))
            .rounded(px(4.))
            .bg(rgb(p.raised))
            .on_click(cx.listener(move |s, _, _, cx| {
                if let Some(store) = &mut s.store {
                    store.remove_image(target.as_deref(), i);
                    cx.notify();
                }
            }))
            .child(
                row()
                    .gap(px(6.))
                    .child(thumb)
                    .child(
                        div()
                            .text_size(p.text(Step::Meta))
                            .text_color(rgb(p.fg))
                            .child(name),
                    )
                    .when(!size_label.is_empty(), |r| {
                        r.child(
                            div()
                                .font_family(LABEL_FONT)
                                .text_size(p.text(Step::Label) * 0.9)
                                .text_color(rgb(p.quiet()))
                                .child(size_label),
                        )
                    })
                    .child(
                        div()
                            .text_size(p.text(Step::Meta))
                            .text_color(rgb(p.muted))
                            .child("×"),
                    ),
            );
        chips = chips.child(crate::platform::layout_probe::mark(
            chip,
            &id,
            &probe_label,
            true,
        ));
    }
    for (i, tag) in file_mentions.iter().enumerate() {
        let target = owner.clone();
        let tag_to_remove = tag.clone();
        let input_for_remove = editor.clone();
        chips = chips.child(
            button(
                p,
                ("attached-mention", i),
                format!("{tag}   ×"),
                false,
                true,
                cx.listener(move |s, _, window, cx| {
                    if let Some(store) = &mut s.store {
                        let current = store.draft(target.as_deref());
                        let next = remove_mention_from_text(current, &tag_to_remove);
                        let _ = store.dispatch(DeskAction::DraftEdit(target.clone(), next.clone()));
                        input_for_remove.update(cx, |e, cx| e.set_value(next, window, cx));
                        cx.notify();
                    }
                }),
            )
            .h(px(26.)),
        );
    }
    if !store.draft_images(owner.as_deref()).is_empty() || !file_mentions.is_empty() {
        panel = panel.child(chips);
    }
    if !store.draft_images(owner.as_deref()).is_empty() {
        if !store.has_command_schema() || store.commands_loading() {
            panel = panel.child(small(p, "Checking model image support…"));
        }
        if let Some(warning) = store.image_model_warning(owner.as_deref()) {
            panel = panel.child(small(p, warning));
        }
    }
    let view = cx.weak_entity();
    panel.child(
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                let view = view.clone();
                // A native Wayland drag can release over the surface without a regular
                // wl_pointer enter. The file-drag coordinates still identify this field.
                window.on_mouse_event(move |up: &MouseUpEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture
                        || up.button != MouseButton::Left
                        || !bounds.contains(&up.position)
                        || !cx.has_active_drag()
                    {
                        return;
                    }
                    let _ = view.update(cx, |s, cx| {
                        if let Some((owner, paths)) = s.file_hover.take() {
                            cx.stop_active_drag(window);
                            s.attach_files(owner.as_deref(), &paths, cx);
                            cx.stop_propagation();
                        }
                    });
                });
            },
        )
        .absolute()
        .inset_0(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[core::prelude::v1::test]
    fn formats_byte_sizes_and_extracts_mentions_cleanly() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");

        let project = Path::new("/work/ox");
        assert_eq!(
            format_file_mention_tag(project, Path::new("/work/ox/docs/spec.pdf")),
            "@docs/spec.pdf"
        );
        assert_eq!(
            format_file_mention_tag(project, Path::new("/work/ox/my docs/spec.pdf")),
            r#"@"my docs/spec.pdf""#
        );
        assert_eq!(
            format_file_mention_tag(project, Path::new("/tmp/report.pdf")),
            "@/tmp/report.pdf"
        );
        assert_eq!(
            mentions_in_text(
                r#"Please check @docs/spec.pdf and @"my docs/notes.md" (not user@example.com)"#
            ),
            vec![
                "@docs/spec.pdf".to_owned(),
                r#"@"my docs/notes.md""#.to_owned()
            ]
        );
        assert_eq!(
            remove_mention_from_text("Compare @src/foo @src/foo_test and foo@src/foo", "@src/foo"),
            "Compare @src/foo_test and foo@src/foo"
        );
    }
}
