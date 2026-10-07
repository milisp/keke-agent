use keke_config_types::ImageLimits;
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

use crate::support;

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
#[derive(Default)]
pub struct ViewImage {
    limits: ImageLimits,
}

impl ViewImage {
    /// Use the deployment's validated preparation budgets.
    pub fn new(limits: ImageLimits) -> Self {
        Self { limits }
    }
}

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

    fn should_list(&self, ctx: &ListToolsContext) -> bool {
        ctx.attributes
            .get("supports_vision")
            .is_none_or(|value| value != "false")
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Read)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        let path = support::resolve(&ctx, &args.path, support::Access::Read)?;
        let display = support::display(&ctx.workspace_root, &path);

        let prepared = keke_image::load_path(path.as_path(), self.limits)
            .await
            .map_err(|error| ToolError::custom(error.code(), format!("{display}: {error}")))?;

        Ok(ViewImageOutput {
            path: display,
            media_type: prepared.image.media_type.clone(),
            bytes: prepared.bytes,
            image: prepared.image,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use keke_paths::AbsPath;
    use keke_protocol::ToolCallId;

    use super::*;

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    fn png() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("encode");
        bytes.into_inner()
    }

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

    #[test]
    fn a_known_text_only_model_is_not_offered_the_image_tool() {
        let mut ctx = ListToolsContext::default();
        assert!(ViewImage::default().should_list(&ctx));
        ctx.attributes
            .insert("supports_vision".into(), "false".into());
        assert!(!ViewImage::default().should_list(&ctx));
        ctx.attributes
            .insert("supports_vision".into(), "true".into());
        assert!(ViewImage::default().should_list(&ctx));
    }

    #[tokio::test]
    async fn an_image_reaches_the_model_as_an_image_block() {
        let (_dir, ctx) = workspace();
        std::fs::write(ctx.workspace_root.as_path().join("shot.png"), png()).expect("write");

        let out = ViewImage::default()
            .run(ctx, args("shot.png"))
            .await
            .expect("views");

        let blocks = out.render();
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text.contains("shot.png")));
        let ContentBlock::Image(image) = &blocks[1] else {
            panic!("the second block must be the image, got {blocks:?}");
        };
        assert_eq!(image.media_type, "image/png");
        assert_eq!(STANDARD.decode(&image.data).expect("base64"), png());
    }

    /// The pixels are in the result's content, which resume replays; carrying
    /// them in `value` as well would store every image twice.
    #[tokio::test]
    async fn the_structured_value_does_not_carry_the_pixels() {
        let (_dir, ctx) = workspace();
        std::fs::write(ctx.workspace_root.as_path().join("shot.png"), png()).expect("write");

        let out = ViewImage::default()
            .run(ctx, args("shot.png"))
            .await
            .expect("views");

        let value = serde_json::to_value(&out).expect("serializes");
        assert!(value.get("image").is_none(), "{value}");
        assert!(!value.to_string().contains(&STANDARD.encode(png())));
    }

    /// A `.png` that is not a PNG must fail here, with the file named, rather
    /// than as an anonymous 400 from the provider.
    #[tokio::test]
    async fn the_extension_is_not_trusted() {
        let (_dir, ctx) = workspace();
        std::fs::write(ctx.workspace_root.as_path().join("fake.png"), "just text").expect("write");

        let error = ViewImage::default()
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
        let mut bytes = png().to_vec();
        bytes.resize(ImageLimits::default().read_bytes as usize + 1, 0);
        std::fs::write(ctx.workspace_root.as_path().join("big.png"), bytes).expect("write");

        let error = ViewImage::default()
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

        let error = ViewImage::default()
            .run(ctx, args("nope.png"))
            .await
            .expect_err("missing");

        assert!(
            matches!(&error, ToolError::Execution { code, message }
                if code == "file_not_found" && message.contains("nope.png")),
            "got {error:?}"
        );
    }
}
