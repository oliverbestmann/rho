//! `ReadImage`: an image file for the model to look at; `ShowImage`: one
//! for the user only.

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::extension::{Context, ToolOutput};
use crate::provider::{ToolName, ToolSpec};

use super::{Tool, ToolKind, input_schema, parse, resolve};
use crate::provider::ImageSource;

pub const READ_IMAGE: ToolName = ToolName::from_static("ReadImage");
pub const SHOW_IMAGE: ToolName = ToolName::from_static("ShowImage");

#[derive(Deserialize, JsonSchema)]
struct ReadImageInput {
    path: String,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

pub struct ReadImage;

impl Tool for ReadImage {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn titled(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: READ_IMAGE,
            description:
                "Look at an image file (PNG, JPEG, GIF or WebP, at most 5 MB). Relative paths are resolved against the working directory. Sandboxed like Read."
                    .into(),
            input_schema: input_schema::<ReadImageInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: ReadImageInput = parse(input)?;
        let (image, info) = load(ctx, &i.path).await?;
        Ok(ToolOutput {
            output: info,
            images: vec![image],
            ..Default::default()
        })
    }
}

pub struct ShowImage;

impl Tool for ShowImage {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn titled(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SHOW_IMAGE,
            description:
                "Show the user an image file (PNG, JPEG, GIF or WebP, at most 5 MB), for example a chart or screenshot you made. You do not see the image; use ReadImage to look at it yourself. Sandboxed like Read."
                    .into(),
            input_schema: input_schema::<ReadImageInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: ReadImageInput = parse(input)?;
        let (image, info) = load(ctx, &i.path).await?;
        Ok(ToolOutput {
            output: format!("shown to the user: {info}"),
            shown: vec![image],
            ..Default::default()
        })
    }
}

/// Reads the image at `path` and describes it as "path (type, size)".
async fn load(ctx: &Context, path: &str) -> Result<(ImageSource, String)> {
    use base64::Engine;
    let resolved = resolve(ctx.cwd(), path);
    // The size first, so a huge file is not read for nothing.
    let bytes = ctx
        .run_sandboxed(move || {
            let len = std::fs::metadata(&resolved)?.len();
            if len > ImageSource::MAX_BYTES as u64 {
                return Err(std::io::Error::other(format!(
                    "{:.1} MB is too large, the limit is {} MB; make a smaller copy first",
                    len as f64 / (1024.0 * 1024.0),
                    ImageSource::MAX_BYTES / (1024 * 1024)
                )));
            }
            std::fs::read(&resolved)
        })
        .await?
        .with_context(|| format!("reading {path}"))?;
    let Some(media_type) = image_type(&bytes) else {
        bail!("{path} is not a PNG, JPEG, GIF or WebP image");
    };
    let info = format!("{path} ({media_type}, {} KB)", bytes.len().div_ceil(1024));
    let source = ImageSource::Base64 {
        media_type: media_type.into(),
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
    };
    Ok((source, info))
}

/// The media type from the file's magic bytes; extensions can lie.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    Some(match bytes {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Access;
    use crate::tools::test_ctx as ctx;
    use serde_json::json;

    /// A 1x1 PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    #[tokio::test]
    async fn read_image_returns_the_image() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap();
        // The type comes from the content, not the extension.
        std::fs::write(dir.path().join("pic.jpg"), &png).unwrap();
        std::fs::write(dir.path().join("notes.png"), "text").unwrap();
        let ctx = ctx(dir.path());

        let out = ReadImage
            .run(json!({"path": "pic.jpg"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.output, "pic.jpg (image/png, 1 KB)");
        assert_eq!(
            out.images,
            [ImageSource::Base64 {
                media_type: "image/png".into(),
                data: PNG.into()
            }]
        );

        let err = |path: &str| {
            let ctx = ctx.clone();
            let path = path.to_string();
            async move {
                let e = ReadImage.run(json!({"path": path}), &ctx).await;
                format!("{:#}", e.unwrap_err())
            }
        };
        assert!(err("notes.png").await.contains("not a PNG"));
        assert!(err("missing.png").await.contains("reading missing.png"));
        let big = std::fs::File::create(dir.path().join("big.png")).unwrap();
        big.set_len(ImageSource::MAX_BYTES as u64 + 1).unwrap();
        assert!(err("big.png").await.contains("too large"));
    }

    #[tokio::test]
    async fn show_image_is_only_for_the_user() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap();
        std::fs::write(dir.path().join("chart.png"), &png).unwrap();
        let out = ShowImage
            .run(json!({"path": "chart.png"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(out.output, "shown to the user: chart.png (image/png, 1 KB)");
        assert!(out.images.is_empty(), "the model gets no pixels");
        assert_eq!(out.shown.len(), 1);
    }

    #[tokio::test]
    async fn read_image_is_sandboxed() {
        let (cwd, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let pic = other.path().join("pic.gif");
        std::fs::write(&pic, b"GIF89a").unwrap();
        let ctx = ctx(cwd.path());
        let input = json!({"path": pic});
        let err = format!(
            "{:#}",
            ReadImage.run(input.clone(), &ctx).await.unwrap_err()
        );
        assert!(err.contains("Permission denied"), "{err}");
        ctx.grant(other.path(), Access::Read);
        let out = ReadImage.run(input, &ctx).await.unwrap();
        assert_eq!(out.images.len(), 1);
    }
}
