//! Terminal drops arrive as shell-quoted paths. Keep those paths editable in
//! the composer, then prepare their bytes off the rendering thread at send.
use std::path::PathBuf;
use std::sync::Arc;

use keke_protocol::{ContentBlock, Message};

use super::App;
use crate::Cell;

pub(crate) struct PreparedPrompt {
    generation: u64,
    draft: String,
    restored: Vec<keke_protocol::ImageBlock>,
    result: Result<Message, String>,
}

/// Split shell quoting without expanding variables or executing anything.
fn tokens(input: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut output = Vec::new();
    let mut start = None;
    let mut value = String::new();
    let mut quote = None;
    let mut escaped = false;
    for (offset, ch) in input.char_indices() {
        if start.is_none() && ch.is_whitespace() {
            continue;
        }
        start.get_or_insert(offset);
        if escaped {
            value.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                value.push(ch);
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch.is_whitespace() {
            if let Some(begin) = start.take() {
                output.push((begin..offset, std::mem::take(&mut value)));
            }
        } else {
            value.push(ch);
        }
    }
    if let Some(start) = start {
        // Incomplete quoting is ordinary editable text, not a file operation.
        if quote.is_none() && !escaped {
            output.push((start..input.len(), value));
        }
    }
    output
}

fn image_paths(input: &str) -> Vec<(std::ops::Range<usize>, PathBuf)> {
    if crate::slash::parse(input.trim()).is_some() {
        return Vec::new();
    }
    tokens(input)
        .into_iter()
        .filter_map(|(range, token)| {
            let path = PathBuf::from(&token);
            // Requiring an explicit path avoids interpreting ordinary words such
            // as "screenshot.png" in a sentence as local attachment requests.
            let explicit =
                path.is_absolute() || token.starts_with("./") || token.starts_with("../");
            let image = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    matches!(
                        ext.to_ascii_lowercase().as_str(),
                        "png" | "jpg" | "jpeg" | "gif" | "webp"
                    )
                });
            (explicit && image).then_some((range, path))
        })
        .collect()
}

impl App {
    #[must_use]
    pub fn with_image_limits(mut self, limits: keke_config_types::ImageLimits) -> Self {
        self.image_limits = limits;
        self
    }

    #[must_use]
    pub fn with_image_root(mut self, root: PathBuf) -> Self {
        self.image_root = root;
        self
    }

    pub(crate) fn image_attachment_label(&self) -> Option<String> {
        let paths = image_paths(&self.input.text());
        if paths.is_empty() && self.restored_images.is_empty() {
            return None;
        }
        let names = paths
            .iter()
            .map(|(_, path)| {
                format!(
                    "[image: {}]",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let names = format!(
            "{names}{}",
            self.restored_images
                .iter()
                .map(|image| format!(" [image: {}]", image.media_type))
                .collect::<String>()
        );
        Some(if self.preparing_images {
            format!(" preparing {names} ")
        } else {
            format!(" {names} · edit paths / Ctrl-Backspace to remove ")
        })
    }

    pub(super) fn invalidate_image_preparation(&mut self) {
        self.image_preparation_generation = self.image_preparation_generation.wrapping_add(1);
        self.preparing_images = false;
    }

    pub(super) fn prepare_image_prompt(&mut self) -> bool {
        let draft = self.input.text();
        if crate::slash::parse(draft.trim()).is_some() {
            return false;
        }
        let paths = image_paths(&draft);
        if paths.is_empty() && self.restored_images.is_empty() {
            return false;
        }
        self.image_preparation_generation = self.image_preparation_generation.wrapping_add(1);
        let generation = self.image_preparation_generation;
        self.preparing_images = true;
        let root = self.image_root.clone();
        let restored = self.restored_images.clone();
        let limits = self.image_limits;
        let sender = self.image_preparation_sender.clone();
        tokio::spawn(async move {
            let original = draft.clone();
            let original_restored = restored.clone();
            let result = async move {
                let mut text = String::new();
                let mut end = 0;
                let mut images: Vec<_> = restored.into_iter().map(ContentBlock::Image).collect();
                for (range, path) in paths {
                    let path = if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    };
                    text.push_str(&draft[end..range.start]);
                    end = range.end;
                    let prepared = keke_image::load_path(&path, limits)
                        .await
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                    images.push(ContentBlock::Image(prepared.image));
                }
                text.push_str(&draft[end..]);
                let mut message = Message::user(text.trim());
                if text.trim().is_empty() {
                    message.content.clear();
                }
                message.content.extend(images);
                Ok(message)
            }
            .await;
            let _ = sender.send(PreparedPrompt {
                generation,
                draft: original,
                restored: original_restored,
                result,
            });
        });
        true
    }

    pub(crate) fn finish_image_prompt(&mut self, prepared: PreparedPrompt) {
        if prepared.generation != self.image_preparation_generation {
            return;
        }
        self.preparing_images = false;
        let message = match prepared.result {
            Ok(message) => message,
            Err(error) => {
                self.transcript.push(Cell::Error(error));
                return;
            }
        };
        // An edit made while loading wins: never send stale attachment content.
        if self.input.text() != prepared.draft || self.restored_images != prepared.restored {
            return;
        }
        self.input.take();
        self.restored_images.clear();
        self.history.submit(&prepared.draft);
        self.transcript
            .push(Cell::User(crate::transcript::user_message_text(&message)));
        self.scroll.follow();
        self.begin_turn();
        let conversation = Arc::clone(&self.conversation);
        tokio::spawn(async move {
            let _ = conversation.prompt_message(message).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_are_explicit_and_shell_quoting_preserves_unicode_and_spaces() {
        let found = image_paths("look '/tmp/你好 one.png' /tmp/two\\ words.JPG please");
        assert_eq!(
            found
                .iter()
                .map(|(_, path)| path.to_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["/tmp/你好 one.png", "/tmp/two words.JPG"]
        );
        assert!(image_paths("explain image.png").is_empty());
        assert!(image_paths("/help /tmp/image.png").is_empty());
    }
}

#[cfg(test)]
mod submission_tests {
    use super::*;
    use keke_acp::ScriptedConversation;
    use std::sync::Arc;

    fn png(path: &std::path::Path) {
        image::RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 255]))
            .save(path)
            .unwrap();
    }

    async fn finish(app: &mut App) {
        let prepared = app.image_preparations.recv().await.unwrap();
        app.finish_image_prompt(prepared);
        tokio::task::yield_now().await;
    }

    #[tokio::test]
    async fn image_only_and_text_with_multiple_images_reach_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("你好 one.png");
        png(&path);
        for text in [
            format!("'{}'", path.display()),
            format!("explain '{}' '{}'", path.display(), path.display()),
        ] {
            let (scripted, _) = ScriptedConversation::new(Vec::new());
            let scripted = Arc::new(scripted);
            let (mut app, _) = App::new(scripted.clone());
            app.handle_paste(&text);
            app.submit();
            assert_eq!(
                app.input.text(),
                text,
                "retain draft until loading succeeds"
            );
            finish(&mut app).await;
            let messages = scripted.messages();
            assert_eq!(messages.len(), 1);
            assert_eq!(
                messages[0]
                    .content
                    .iter()
                    .filter(|block| matches!(block, ContentBlock::Image(_)))
                    .count(),
                if text.starts_with("explain") { 2 } else { 1 }
            );
            assert_eq!(
                messages[0].text(),
                if text.starts_with("explain") {
                    "explain"
                } else {
                    ""
                }
            );
            assert!(app.input.is_empty());
        }
    }

    #[test]
    fn restored_image_only_messages_remain_visible() {
        let message = Message {
            role: keke_protocol::Role::User,
            content: vec![ContentBlock::Image(keke_protocol::ImageBlock {
                data: "secret encoded bytes".into(),
                media_type: "image/png".into(),
            })],
        };
        let mut transcript = crate::Transcript::default();
        transcript.replay(&[message]);
        assert_eq!(
            transcript.last(),
            Some(&Cell::User("[image: image/png]".into()))
        );
    }

    #[tokio::test]
    async fn rewind_resends_inline_image_after_source_file_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.png");
        png(&path);
        let image = keke_image::load_path(&path, Default::default())
            .await
            .unwrap()
            .image;
        std::fs::remove_file(path).unwrap();
        for text in ["", "edited text"] {
            let (scripted, _) = ScriptedConversation::new(Vec::new());
            let scripted = Arc::new(scripted);
            let (mut app, _) = App::new(scripted.clone());
            app.rewind_pending = true;
            app.rewind_draft = Some("old text".into());
            app.input.set_text(text);
            app.report_rewind(&keke_acp::Rewound {
                input: Some(Message {
                    role: keke_protocol::Role::User,
                    content: vec![
                        ContentBlock::text("old text"),
                        ContentBlock::Image(image.clone()),
                    ],
                }),
                ..Default::default()
            });
            assert_eq!(
                app.input.text(),
                text,
                "preserve edits made while rewind awaits"
            );
            assert!(!app.rewind_pending);
            app.submit();
            finish(&mut app).await;
            let messages = scripted.messages();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].text(), text);
            assert!(
                messages[0]
                    .content
                    .contains(&ContentBlock::Image(image.clone()))
            );
            assert!(app.restored_images.is_empty());
        }
    }

    #[tokio::test]
    async fn rewind_wait_removal_and_reset_do_not_send_unwanted_images() {
        let (scripted, _) = ScriptedConversation::new(Vec::new());
        let scripted = Arc::new(scripted);
        let (mut app, _) = App::new(scripted.clone());
        let image = keke_protocol::ImageBlock {
            data: "already prepared".into(),
            media_type: "image/png".into(),
        };
        app.restored_images.push(image.clone());
        app.rewind_pending = true;
        app.submit();
        assert!(!app.preparing_images);
        app.rewind_pending = false;
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Backspace,
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert!(app.restored_images.is_empty());
        app.submit();
        assert!(scripted.messages().is_empty());
        app.restored_images.push(image.clone());
        app.apply(keke_acp::Update::SessionReset);
        assert!(app.restored_images.is_empty());
        app.restored_images.push(image);
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('u'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert!(app.restored_images.is_empty());
    }

    #[tokio::test]
    async fn reset_invalidates_preparation_even_when_draft_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.png");
        png(&path);
        let (scripted, _) = ScriptedConversation::new(Vec::new());
        let scripted = Arc::new(scripted);
        let (mut app, _) = App::new(scripted.clone());
        app.input.set_text(&path.display().to_string());
        app.submit();
        let stale = app.image_preparations.recv().await.unwrap();
        app.apply(keke_acp::Update::SessionReset);
        app.submit();
        assert!(app.preparing_images);
        app.finish_image_prompt(stale);
        assert!(
            app.preparing_images,
            "old completion must not retire the new job"
        );
        assert!(scripted.messages().is_empty());
        finish(&mut app).await;
        assert_eq!(scripted.messages().len(), 1);
    }

    #[tokio::test]
    async fn relative_image_paths_use_session_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        png(&dir.path().join("local.png"));
        let (scripted, _) = ScriptedConversation::new(Vec::new());
        let scripted = Arc::new(scripted);
        let (app, _) = App::new(scripted.clone());
        let mut app = app.with_image_root(dir.path().to_path_buf());
        app.input.set_text("./local.png");
        app.submit();
        finish(&mut app).await;
        assert!(matches!(
            scripted.messages()[0].content[0],
            ContentBlock::Image(_)
        ));
    }

    #[test]
    fn files_only_rewind_does_not_replace_composer_attachments() {
        let (scripted, _) = ScriptedConversation::new(Vec::new());
        let (mut app, _) = App::new(Arc::new(scripted));
        app.input.set_text("unrelated draft");
        app.report_rewind(&keke_acp::Rewound {
            input: Some(Message {
                role: keke_protocol::Role::User,
                content: vec![ContentBlock::Image(keke_protocol::ImageBlock {
                    data: "old bytes".into(),
                    media_type: "image/png".into(),
                })],
            }),
            ..Default::default()
        });
        assert_eq!(app.input.text(), "unrelated draft");
        assert!(app.restored_images.is_empty());
    }

    #[tokio::test]
    async fn bare_path_delivered_as_key_events_attaches_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.png");
        png(&path);
        let (scripted, _) = ScriptedConversation::new(Vec::new());
        let scripted = Arc::new(scripted);
        let (mut app, _) = App::new(scripted.clone());
        for ch in path.display().to_string().chars() {
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(ch),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        finish(&mut app).await;
        assert!(matches!(
            scripted.messages()[0].content[0],
            ContentBlock::Image(_)
        ));
    }

    #[tokio::test]
    async fn missing_or_corrupt_images_preserve_draft_and_do_not_send() {
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("corrupt.png");
        std::fs::write(&corrupt, b"not an image").unwrap();
        for path in [corrupt, dir.path().join("missing.png")] {
            let (scripted, _) = ScriptedConversation::new(Vec::new());
            let scripted = Arc::new(scripted);
            let (mut app, _) = App::new(scripted.clone());
            let draft = path.display().to_string();
            app.input.set_text(&draft);
            app.submit();
            finish(&mut app).await;
            assert_eq!(app.input.text(), draft);
            assert!(scripted.messages().is_empty());
            assert!(matches!(app.transcript.last(), Some(Cell::Error(_))));
        }
    }

    #[tokio::test]
    async fn removing_attachment_during_preparation_cancels_stale_submission() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.png");
        png(&path);
        let (scripted, _) = ScriptedConversation::new(Vec::new());
        let scripted = Arc::new(scripted);
        let (mut app, _) = App::new(scripted.clone());
        app.input.set_text(&path.display().to_string());
        app.submit();
        app.input.set_text("edited");
        finish(&mut app).await;
        assert_eq!(app.input.text(), "edited");
        assert!(scripted.messages().is_empty());
    }
}
