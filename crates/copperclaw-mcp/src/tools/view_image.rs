//! `view_image`: load an image file from the container and surface it to
//! the model as an image content block, so a vision-capable model (e.g.
//! minimax-m3) can actually see the pixels.
//!
//! The runner's tool-dispatch layer pulls the returned image block out of
//! the `CallToolResult` and pushes it into the transcript as a
//! `HistoryMessage::Image`; the anthropic provider then serialises it as a
//! base64 `image` content block. Text-only providers drop it gracefully.
//!
//! This is the read side of multimodal — pairing with inbound photos. Use
//! it for screenshots, charts, diagrams, or any image already on disk
//! (one fetched with `web_fetch`/`curl`, generated, or in a repo).

use crate::context::{bytes_b64, ToolContext};
use crate::error::ToolError;
use crate::tools::{make_tool, parse_args, ToolEntry, ToolHandler};
use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;

/// Cap on the image we'll load. The base64 lives in conversation history
/// (re-sent every turn until compaction), and a tiled vision model gains
/// nothing from more than a few megapixels — oversized images just burn
/// the context budget. Above this the call is refused with a downscale hint.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024; // 5 MB

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
}

pub fn schema() -> Tool {
    make_tool(
        "view_image",
        "Load an image file from the container and view it yourself (vision). Use this to actually SEE a screenshot, chart, diagram, or photo that is already on disk — e.g. one you downloaded with `web_fetch`/`curl`, generated, or that lives in a repo. Pass the file `path` (PNG, JPEG, WebP, or GIF). The image is attached to the conversation so you can describe or reason about it in your next reply. Images over 5 MB are refused — downscale first (e.g. `shell` with `convert -resize 1280x in.png out.png`).",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path"],
            "properties": {
                "path": { "type": "string", "minLength": 1 }
            }
        }),
    )
}

/// Map a file extension to an image MIME type. `None` for anything we
/// don't recognise as an image the vision models accept.
fn mime_for(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let Some(mime) = mime_for(&input.path) else {
        return Err(ToolError::Validation(format!(
            "`{}` is not a supported image type — pass a .png, .jpg/.jpeg, .webp, or .gif file.",
            input.path
        )));
    };
    let meta = tokio::fs::metadata(&input.path)
        .await
        .map_err(|e| ToolError::Validation(format!("cannot open `{}`: {e}", input.path)))?;
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(ToolError::Validation(format!(
            "`{}` is {} bytes, over the {MAX_IMAGE_BYTES}-byte view_image limit — downscale it first (e.g. shell `convert -resize 1280x {0} /data/small.png`).",
            input.path,
            meta.len()
        )));
    }
    let bytes = tokio::fs::read(&input.path)
        .await
        .map_err(|e| ToolError::Validation(format!("cannot read `{}`: {e}", input.path)))?;
    let b64 = bytes_b64::encode(&bytes);
    Ok(CallToolResult::success(vec![
        Content::text(format!(
            "Loaded image {} ({} bytes, {mime}); it is attached below for you to see.",
            input.path,
            bytes.len()
        )),
        Content::image(b64, mime.to_string()),
    ]))
}

struct Handler;
#[async_trait::async_trait]
impl ToolHandler for Handler {
    async fn call(
        &self,
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        handle(arguments, ctx).await
    }
}

pub fn entry() -> ToolEntry {
    ToolEntry {
        tool: schema(),
        handler: Box::new(Handler),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_detection_covers_common_types() {
        assert_eq!(mime_for("/data/a.png"), Some("image/png"));
        assert_eq!(mime_for("/data/a.JPG"), Some("image/jpeg"));
        assert_eq!(mime_for("/data/a.jpeg"), Some("image/jpeg"));
        assert_eq!(mime_for("/data/a.webp"), Some("image/webp"));
        assert_eq!(mime_for("/data/a.gif"), Some("image/gif"));
        assert_eq!(mime_for("/data/a.txt"), None);
        assert_eq!(mime_for("/data/noext"), None);
    }

    #[tokio::test]
    async fn rejects_non_image_extension() {
        let mut args = JsonObject::new();
        args.insert("path".into(), "/tmp/notes.txt".into());
        let err = handle(Some(args), &crate::context::MockToolContext::new()).await;
        assert!(matches!(err, Err(ToolError::Validation(_))));
    }
}
