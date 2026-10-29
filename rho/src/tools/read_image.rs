//! `ReadImage`: an image file for the model to look at.

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::extension::{Context, ToolOutput};
use crate::provider::{ToolName, ToolSpec};

use super::{Tool, ToolKind, input_schema, parse, resolve};
use crate::provider::ImageSource;

pub const READ_IMAGE: ToolName = ToolName::from_static("ReadImage");

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
        use base64::Engine;
        let i: ReadImageInput = parse(input)?;
        let path = resolve(ctx.cwd(), &i.path);
        // The size first, so a huge file is not read for nothing.
        let bytes = ctx
            .run_sandboxed(move || {
                let len = std::fs::metadata(&path)?.len();
                if len > ImageSource::MAX_BYTES as u64 {
                    return Err(std::io::Error::other(format!(
                        "{:.1} MB is too large, the limit is {} MB; make a smaller copy first",
                        len as f64 / (1024.0 * 1024.0),
                        ImageSource::MAX_BYTES / (1024 * 1024)
                    )));
                }
                std::fs::read(&path)
            })
            .await?
            .with_context(|| format!("reading {}", i.path))?;
        let Some(media_type) = image_type(&bytes) else {
            bail!("{} is not a PNG, JPEG, GIF or WebP image", i.path);
        };
        let source = ImageSource::Base64 {
            media_type: media_type.into(),
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        };
        Ok(ToolOutput {
            output: format!(
                "{} ({media_type}, {} KB)",
                i.path,
                bytes.len().div_ceil(1024)
            ),
            images: vec![source],
            ..Default::default()
        })
    }
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
