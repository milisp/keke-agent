use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use keke_protocol::ContentBlock;
use keke_protocol::ImageBlock;
use keke_tool::ListToolsContext;
use keke_tool::Tool;
use keke_tool::ToolCallContext;
use keke_tool::ToolCapabilities;
use keke_tool::ToolDescription;
use keke_tool::ToolError;
use keke_tool::ToolId;
use keke_tool::ToolKind;
use keke_tool::ToolOutput;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncReadExt;

use crate::support;

/// Largest file this tool will send.
///
/// Chosen so the base64 form stays under the 5 MB per-image limit Anthropic's
/// wire enforces (base64 is 4/3 the size of its input); the OpenAI wires accept
/// at least that. There is no resizing here, so an oversized file is refused
/// with its size rather than silently degraded.
const MAX_IMAGE_BYTES: u64 = 3_750_000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ViewImageArgs {
    /// Path to a PNG, JPEG, GIF, or WebP file, absolute or relative to the
    /// workspace root.
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct ViewImageOutput {
    pub path: String,
    pub media_type: String,
    pub bytes: usize,
    /// Left out of `value` on purpose: the pixels travel in the result's
    /// content blocks, which is what a resumed session replays, and serializing
    /// them here too would write every image to the log twice.
    #[serde(skip)]
    image: ImageBlock,
}

impl ToolOutput for ViewImageOutput {
    fn render(&self) -> Vec<ContentBlock> {
        vec![
            ContentBlock::text(format!(
                "{} ({}, {} bytes)",
                self.path, self.media_type, self.bytes
            )),
            ContentBlock::Image(self.image.clone()),
        ]
    }
}

/// Shows the model an image file.
pub struct ViewImage;

impl Tool for ViewImage {
    type Args = ViewImageArgs;
    type Output = ViewImageOutput;

    fn id(&self) -> ToolId {
        ToolId::new("view_image")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "View an image file (PNG, JPEG, GIF, or WebP) so you can see it. Paths may be \
             absolute or relative to the workspace root. Use it for screenshots, diagrams, and \
             UI mockups; use `read_file` for text.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Read)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        let path = support::resolve(&ctx, &args.path, support::Access::Read)?;
        let display = support::display(&ctx.workspace_root, &path);

        let file = tokio::fs::File::open(path.as_path())
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => {
                    ToolError::custom("file_not_found", format!("{display}: no such file"))
                }
                _ => ToolError::custom("read_failed", format!("{display}: {error}")),
            })?;

        // One byte past the limit is enough to tell "too big" from "exactly
        // fits" without materializing a file of any size.
        let mut buffer = Vec::new();
        file.take(MAX_IMAGE_BYTES + 1)
            .read_to_end(&mut buffer)
            .await
            .map_err(|error| ToolError::custom("read_failed", format!("{display}: {error}")))?;

        if buffer.len() as u64 > MAX_IMAGE_BYTES {
            return Err(ToolError::custom(
                "image_too_large",
                format!("{display}: larger than {MAX_IMAGE_BYTES} bytes, not sending"),
            ));
        }

        // Sniffed, not taken from the extension: a mislabeled file would be
        // rejected by the provider with an error that names neither the file
        // nor the cause.
        let Some(media_type) = sniff_media_type(&buffer) else {
            return Err(ToolError::custom(
                "unsupported_image",
                format!("{display}: not a PNG, JPEG, GIF, or WebP file"),
            ));
        };

        Ok(ViewImageOutput {
            path: display,
            media_type: media_type.to_string(),
            bytes: buffer.len(),
            image: ImageBlock {
                data: STANDARD.encode(&buffer),
                media_type: media_type.to_string(),
            },
        })
    }
}

/// The media type named by `bytes`' magic number, for the formats every wire
/// accepts.
fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use keke_paths::AbsPath;
    use keke_protocol::ToolCallId;

    use super::*;

    const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn workspace() -> (tempfile::TempDir, ToolCallContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let ctx = ToolCallContext {
            call_id: ToolCallId::new("call-1"),
            workspace_root: AbsPath::new(root).expect("absolute"),
            timeout_millis: None,
            cancelled: Arc::new(|| false),
        };
        (dir, ctx)
    }

    fn args(path: &str) -> ViewImageArgs {
        ViewImageArgs { path: path.into() }
    }

    #[tokio::test]
    async fn an_image_reaches_the_model_as_an_image_block() {
        let (_dir, ctx) = workspace();
        std::fs::write(ctx.workspace_root.as_path().join("shot.png"), PNG_HEADER).expect("write");

        let out = ViewImage.run(ctx, args("shot.png")).await.expect("views");

        let blocks = out.render();
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text.contains("shot.png")));
        let ContentBlock::Image(image) = &blocks[1] else {
            panic!("the second block must be the image, got {blocks:?}");
        };
        assert_eq!(image.media_type, "image/png");
        assert_eq!(STANDARD.decode(&image.data).expect("base64"), PNG_HEADER);
    }

    /// The pixels are in the result's content, which resume replays; carrying
    /// them in `value` as well would store every image twice.
    #[tokio::test]
    async fn the_structured_value_does_not_carry_the_pixels() {
        let (_dir, ctx) = workspace();
        std::fs::write(ctx.workspace_root.as_path().join("shot.png"), PNG_HEADER).expect("write");

        let out = ViewImage.run(ctx, args("shot.png")).await.expect("views");

        let value = serde_json::to_value(&out).expect("serializes");
        assert!(value.get("image").is_none(), "{value}");
        assert!(!value.to_string().contains(&STANDARD.encode(PNG_HEADER)));
    }

    /// A `.png` that is not a PNG must fail here, with the file named, rather
    /// than as an anonymous 400 from the provider.
    #[tokio::test]
    async fn the_extension_is_not_trusted() {
        let (_dir, ctx) = workspace();
        std::fs::write(ctx.workspace_root.as_path().join("fake.png"), "just text").expect("write");

        let error = ViewImage
            .run(ctx, args("fake.png"))
            .await
            .expect_err("not an image");

        assert!(
            matches!(&error, ToolError::Execution { code, message }
                if code == "unsupported_image" && message.contains("fake.png")),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn an_oversized_file_is_refused_not_sent() {
        let (_dir, ctx) = workspace();
        let mut bytes = PNG_HEADER.to_vec();
        bytes.resize(MAX_IMAGE_BYTES as usize + 1, 0);
        std::fs::write(ctx.workspace_root.as_path().join("big.png"), bytes).expect("write");

        let error = ViewImage
            .run(ctx, args("big.png"))
            .await
            .expect_err("too large");

        assert!(
            matches!(&error, ToolError::Execution { code, .. } if code == "image_too_large"),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_file_names_itself() {
        let (_dir, ctx) = workspace();

        let error = ViewImage
            .run(ctx, args("nope.png"))
            .await
            .expect_err("missing");

        assert!(
            matches!(&error, ToolError::Execution { code, message }
                if code == "file_not_found" && message.contains("nope.png")),
            "got {error:?}"
        );
    }

    #[test]
    fn every_supported_format_is_recognized_by_its_magic_number() {
        assert_eq!(sniff_media_type(PNG_HEADER), Some("image/png"));
        assert_eq!(sniff_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_media_type(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_media_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_media_type(b"RIFF\0\0\0\0WAVEfmt "), None);
    }
}
