//! `read_media`: show an image or a video to the model.
//!
//! What goes out depends on the provider ([`Llm::media_policy`]): images are
//! converted to a format it accepts and brought within the per-image byte
//! budget ([`ToolOutputPolicy::image_byte_budget`], 256 KiB by default);
//! video goes inline or — on Kimi — is uploaded by the provider before the
//! request; what the model cannot take is described instead, with the reason.
//! Each media part follows a caption naming its file, so the model knows what
//! it is looking at, and a request that later has to drop the media can still
//! say where it came from.
//!
//! [`Llm::media_policy`]: crate::agent_loop::llm::Llm::media_policy
//! [`ToolOutputPolicy::image_byte_budget`]: crate::tool_output::ToolOutputPolicy::image_byte_budget

use async_trait::async_trait;
use base64::Engine;
use lm_bridge::media::{MediaPolicy, REQUEST_MEDIA_BUDGET_LOW_BYTES, VideoSupport, normalize_mime};
use lm_bridge::{
    message::{ContentPart, ImageUrl, UrlRef},
    tool::Tool,
};
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

mod image_compress;
use image_compress::{ImageTarget, prepare_image, resolve_max_image_edge_px, sniff_image_mime};

/// Largest media file read at all.
const MAX_MEDIA_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// Largest video sent inline: its `data:` URL must fit the request's media
/// budget with the rest of the conversation's media dropped.
const MAX_INLINE_VIDEO_BYTES: u64 = (REQUEST_MEDIA_BUDGET_LOW_BYTES as u64 / 4) * 3;

/// Formats a vision fallback is given: every vision model takes these.
const FALLBACK_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg"];

/// The caption prefix naming a media part's file; see [`caption_path`].
const CAPTION_FILE_PREFIXES: [&str; 2] = ["<system>Image file: ", "<system>Video file: "];

pub struct ReadMediaTool {
    tool: Tool,
}

impl ReadMediaTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "read_media",
                "Show an image or a video file to the model. Images are converted to a format \
                 the current provider accepts and downsampled to fit its limits; video is sent \
                 when the current model takes video. When the model cannot take the file, the \
                 result says why and what to do instead. For a model that reads no images, a \
                 vision model describes the image, or answers `question` about it.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the media file" },
                        "full_resolution": {
                            "type": "boolean",
                            "description": "Skip image downscaling and send the image at native resolution and size. The request may then be rejected as too large."
                        },
                        "question": {
                            "type": "string",
                            "description": "When a vision model describes the image for you because your model reads no images: what to find out from it, such as \"transcribe every label and the text of each arrow\". Without it the image is described in general."
                        }
                    },
                    "required": ["path"]
                }),
            ),
        }
    }

    pub(crate) fn mime_type(path: &std::path::Path) -> Option<&'static str> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        match ext.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            "bmp" => Some("image/bmp"),
            "heic" => Some("image/heic"),
            "heif" => Some("image/heif"),
            "avif" => Some("image/avif"),
            "tif" | "tiff" => Some("image/tiff"),
            "svg" => Some("image/svg+xml"),
            "mp4" | "m4v" => Some("video/mp4"),
            "mov" => Some("video/quicktime"),
            "avi" => Some("video/x-msvideo"),
            "mkv" => Some("video/x-matroska"),
            "webm" => Some("video/webm"),
            "mpeg" | "mpg" => Some("video/mpeg"),
            "3gp" => Some("video/3gpp"),
            "flv" => Some("video/x-flv"),
            "mp3" => Some("audio/mpeg"),
            "wav" => Some("audio/wav"),
            "ogg" => Some("audio/ogg"),
            "m4a" => Some("audio/mp4"),
            "flac" => Some("audio/flac"),
            _ => None,
        }
    }
}

impl Default for ReadMediaTool {
    fn default() -> Self {
        Self::new()
    }
}

/// The file a media caption names, if `text` is one; see [`ReadMediaTool`].
#[must_use]
pub fn caption_path(text: &str) -> Option<&str> {
    let rest = CAPTION_FILE_PREFIXES
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))?;
    rest.split('\n').next().filter(|path| !path.is_empty())
}

/// The caption put before a media part: its file on the first line, details
/// on the second.
fn caption(kind: &str, path: &std::path::Path, details: &str) -> ContentPart {
    ContentPart::text(format!(
        "<system>{kind} file: {}\n{details}</system>",
        path.display()
    ))
}

#[async_trait]
impl ExecutableTool for ReadMediaTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    /// Offered when something can see the media: a model that takes images
    /// or video, or a vision fallback that describes images for one that
    /// does not.
    fn is_available(&self, ctx: &ExecutableToolContext) -> bool {
        ctx.vision_fallback.is_some()
            || ctx.llm.as_ref().is_some_and(|llm| {
                let policy = llm.media_policy();
                policy.takes_images() || policy.takes_video()
            })
    }

    fn accesses(&self, input: &serde_json::Value) -> ToolAccesses {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        ToolAccesses::read_file(path)
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        "silent"
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
        // Readable, not just allowed: an MCP tool's image is saved in the
        // spill directory, and its caption points here.
        let path = match ctx.resolve_readable_path(std::path::Path::new(path)) {
            Ok(p) => p,
            Err(e) => return ExecutableToolResult::error(e),
        };
        if !path.exists() {
            return ExecutableToolResult::error(format!("File not found: {}", path.display()));
        }

        let metadata = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => {
                return ExecutableToolResult::error(format!(
                    "Failed to read metadata for {}: {e}",
                    path.display()
                ));
            }
        };
        if metadata.len() > MAX_MEDIA_FILE_BYTES {
            return ExecutableToolResult::error(format!(
                "File {} is too large ({} bytes; max {MAX_MEDIA_FILE_BYTES} bytes)",
                path.display(),
                metadata.len(),
            ));
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                return ExecutableToolResult::error(format!(
                    "Failed to read {}: {e}",
                    path.display()
                ));
            }
        };
        let Some(mime) = sniff_image_mime(&bytes).or_else(|| Self::mime_type(&path)) else {
            return ExecutableToolResult::error(format!(
                "Unsupported or unknown media type for {}; read_media shows images and video",
                path.display()
            ));
        };
        let full_resolution = input
            .get("full_resolution")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let question = input.get("question").and_then(|v| v.as_str());

        if mime == "image/svg+xml" {
            return ExecutableToolResult::error(format!(
                "{} is an SVG — a text format. Read its markup with read_file, or render it to \
                 PNG first (e.g. `rsvg-convert \"{0}\" -o out.png` or `magick \"{0}\" out.png`) \
                 to see it drawn.",
                path.display()
            ));
        }
        if mime.starts_with("image/") {
            match image_parts(bytes, mime, &path, full_resolution, question, ctx).await {
                Ok(output) => ExecutableToolResult {
                    output,
                    is_error: false,
                    stop_turn: false,
                },
                Err(message) => ExecutableToolResult::error(message),
            }
        } else if mime.starts_with("video/") {
            read_video(&path, &bytes, mime, ctx)
        } else {
            ExecutableToolResult::text(format!(
                "Audio file '{}' ({mime}, {} bytes). None of the models Rusta talks to takes \
                 audio input, so its content cannot be shown.",
                path.display(),
                bytes.len()
            ))
        }
    }
}

/// The parts that show the image in `bytes` (from the file at `path`) to the
/// current model: a caption naming the file, then the image converted and
/// downsampled for the provider. For a model without vision, the vision
/// fallback's description — or its answer to `question` — or a note. `Err`
/// is a message for the model: the image cannot be sent, and why.
///
/// Shared with tools that produce images — an MCP server's screenshot is
/// saved and shown through this, so it gets the same treatment as a file.
pub async fn image_parts(
    bytes: Vec<u8>,
    mime: &str,
    path: &std::path::Path,
    full_resolution: bool,
    question: Option<&str>,
    ctx: &ExecutableToolContext,
) -> Result<Vec<ContentPart>, String> {
    let policy = ctx
        .llm
        .as_ref()
        .map_or(MediaPolicy::TEXT_ONLY, |llm| llm.media_policy());
    let byte_budget = ctx.tool_output.image_byte_budget;
    let max_edge = resolve_max_image_edge_px();

    if !policy.takes_images() {
        let Some(fallback) = &ctx.vision_fallback else {
            return Ok(vec![ContentPart::text(format!(
                "Media file '{}' ({mime}, {} bytes). The current model does not support vision, \
                 so the image content cannot be shown.",
                path.display(),
                bytes.len()
            ))]);
        };
        let prepared = prepare_off_thread(
            bytes.clone(),
            mime,
            ImageTarget {
                accepted: FALLBACK_IMAGE_MIMES,
                byte_budget,
                max_edge,
                provider_max_edge: None,
                full_resolution,
            },
        )
        .await;
        let prepared = prepared.map_err(|message| message(path))?;
        let data = base64::engine::general_purpose::STANDARD.encode(&prepared.bytes);
        let question = question.map(str::trim).filter(|q| !q.is_empty());
        let text = match fallback
            .describe_image(&prepared.mime_type, &data, question)
            .await
        {
            Ok(description) => {
                let asked = question.map_or_else(String::new, |q| format!(" (asked: {q})"));
                format!(
                    "Image '{}' ({mime}, {} bytes). Vision analysis{asked}: {description}",
                    path.display(),
                    bytes.len()
                )
            }
            Err(e) => format!(
                "Media file '{}' ({mime}, {} bytes). Vision fallback failed: {e}. The current model \
                 does not support vision, so the image content cannot be shown.",
                path.display(),
                bytes.len()
            ),
        };
        return Ok(vec![ContentPart::text(text)]);
    }

    let prepared = prepare_off_thread(
        bytes,
        mime,
        ImageTarget {
            accepted: policy.image_mimes,
            byte_budget,
            max_edge,
            provider_max_edge: policy.max_image_edge_px,
            full_resolution,
        },
    )
    .await;
    let prepared = prepared.map_err(|message| message(path))?;

    let mut details = normalize_mime(&prepared.mime_type);
    if let Some((w, h)) = prepared.dimensions {
        details.push_str(&format!(", {w}x{h} px"));
    }
    details.push_str(&format!(", {} bytes", prepared.bytes.len()));
    if let Some(original) = &prepared.converted_from {
        details.push_str(&format!(
            "; converted from {original}, which the current provider does not take as it was"
        ));
    }
    if let Some((w, h)) = prepared.downsampled_from {
        details.push_str(&format!(
            "; downsampled from {w}x{h} px to fit model limits — fine detail may be lost"
        ));
    }
    details.push('.');
    let data = base64::engine::general_purpose::STANDARD.encode(&prepared.bytes);
    Ok(vec![
        caption("Image", path, &details),
        ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: format!("data:{};base64,{data}", prepared.mime_type),
                id: None,
            },
        },
    ])
}

/// [`prepare_image`] for `target` on a blocking thread; an error comes back as
/// a function from the file's path to the message.
async fn prepare_off_thread(
    bytes: Vec<u8>,
    mime: &str,
    target: ImageTarget<'static>,
) -> Result<image_compress::PreparedImage, Box<dyn Fn(&std::path::Path) -> String + Send>> {
    let mime = mime.to_string();
    let task = tokio::task::spawn_blocking(move || prepare_image(&bytes, &mime, &target)).await;
    match task {
        Ok(Ok(prepared)) => Ok(prepared),
        Ok(Err(e)) => Err(Box::new(move |path: &std::path::Path| e.describe(path))),
        Err(e) => Err(Box::new(move |path: &std::path::Path| {
            format!("Image preparation failed for {}: {e}", path.display())
        })),
    }
}

/// Why a video cannot be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoRefusal {
    /// The current model does not take video; the message explains.
    Unsupported(String),
    /// Too large to send inline; the message says how to shrink it.
    TooLarge(String),
}

impl VideoRefusal {
    /// The explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Unsupported(message) | Self::TooLarge(message) => message,
        }
    }
}

/// The parts that show the video in `bytes` (from the file at `path`) to the
/// current model — a caption naming the file, then the video — or why it
/// cannot be sent. Shared with tools that produce video.
pub fn video_parts(
    bytes: &[u8],
    mime: &str,
    path: &std::path::Path,
    ctx: &ExecutableToolContext,
) -> Result<Vec<ContentPart>, VideoRefusal> {
    let policy = ctx
        .llm
        .as_ref()
        .map_or(MediaPolicy::TEXT_ONLY, |llm| llm.media_policy());
    let size = bytes.len() as u64;
    match policy.video {
        VideoSupport::Unsupported { hint } => {
            return Err(VideoRefusal::Unsupported(format!(
                "Video file '{}' ({mime}, {size} bytes) cannot be shown: {hint}.",
                path.display()
            )));
        }
        VideoSupport::Inline if size > MAX_INLINE_VIDEO_BYTES => {
            return Err(VideoRefusal::TooLarge(format!(
                "{} is {size} bytes, too large to send inline (the limit is \
                 {MAX_INLINE_VIDEO_BYTES} bytes). Trim or downscale it first, e.g. `ffmpeg -i \
                 \"{0}\" -t 60 -vf scale=-2:480 short.mp4`, then read the result.",
                path.display()
            )));
        }
        VideoSupport::Inline | VideoSupport::Upload => {}
    }
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(vec![
        caption("Video", path, &format!("{mime}, {size} bytes.")),
        ContentPart::VideoUrl {
            video_url: UrlRef {
                url: format!("data:{mime};base64,{data}"),
                id: None,
            },
        },
    ])
}

fn read_video(
    path: &std::path::Path,
    bytes: &[u8],
    mime: &str,
    ctx: &ExecutableToolContext,
) -> ExecutableToolResult {
    match video_parts(bytes, mime, path, ctx) {
        Ok(output) => ExecutableToolResult {
            output,
            is_error: false,
            stop_turn: false,
        },
        Err(VideoRefusal::Unsupported(message)) => ExecutableToolResult::text(message),
        Err(VideoRefusal::TooLarge(message)) => ExecutableToolResult::error(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::types::ExecutableToolContext;

    #[test]
    fn test_mime_type_detection() {
        assert_eq!(
            ReadMediaTool::mime_type(std::path::Path::new("x.png")),
            Some("image/png")
        );
        assert_eq!(
            ReadMediaTool::mime_type(std::path::Path::new("x.mp4")),
            Some("video/mp4")
        );
        assert_eq!(
            ReadMediaTool::mime_type(std::path::Path::new("x.unknown")),
            None
        );
    }

    fn test_ctx() -> crate::agent_loop::types::ExecutableToolContext {
        use std::sync::Arc;
        let background = Arc::new(crate::background::BackgroundManager::new());
        crate::agent_loop::types::ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::env::current_dir().unwrap(),
            allowed_paths: std::collections::HashSet::new(),
            todo_list: crate::TodoList::shared(),
            tool_event_sender: None,
            user_interaction: None,
            search_url: None,
            plan_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            permission_mode: crate::permissions::PermissionMode::Default,
            llm: None,
            vision_fallback: None,
            web_search_client: None,
            web_reader_client: None,
            background: background.clone(),
            goal_manager: std::sync::Arc::new(std::sync::Mutex::new(
                crate::goal::GoalManager::new(),
            )),
            tool_disclosure: None,
            stale_guard: Default::default(),
            fork_source: Default::default(),
            cron: Arc::new(crate::cron::CronManager::new()),
            ..Default::default()
        }
    }

    fn vision_ctx() -> crate::agent_loop::types::ExecutableToolContext {
        use std::sync::Arc;

        struct VisionLlm;

        #[async_trait]
        impl crate::agent_loop::Llm for VisionLlm {
            fn model_name(&self) -> &str {
                "vision-mock"
            }

            fn supports_vision(&self) -> bool {
                true
            }

            async fn chat(
                &self,
                _messages: Vec<lm_bridge::message::Message>,
                _tools: Vec<lm_bridge::tool::Tool>,
                _callbacks: crate::agent_loop::LlmCallbacks,
                _cancel: &tokio_util::sync::CancellationToken,
            ) -> Result<crate::agent_loop::types::StepResult, lm_bridge::errors::LlmError>
            {
                unimplemented!("vision mock is not expected to generate")
            }
        }

        let mut ctx = test_ctx();
        ctx.llm = Some(Arc::new(VisionLlm));
        ctx
    }

    #[tokio::test]
    async fn test_read_media_returns_image_part_when_vision_supported() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pixel.png");
        // Minimal 1x1 PNG.
        let bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x02\0\0\0\x90wS\xde\0\0\0\x0cIDATx\x9cc\xf8\x0f\0\0\x01\x01\x00\x05\x18\xd8N\0\0\x00\0IEND\xaeB`\x82";
        tokio::fs::write(&path, bytes).await.unwrap();

        let mut ctx = vision_ctx();
        ctx.cwd = temp.path().to_path_buf();
        let tool = ReadMediaTool::new();
        let result = tool
            .execute(json!({"path": "pixel.png"}), &ctx, "test-id")
            .await;
        assert!(!result.is_error);
        assert_eq!(result.output.len(), 2);
        match &result.output[0] {
            ContentPart::Text { text } => {
                // The path resolved, as the tools resolve every path.
                let resolved = std::fs::canonicalize(&path).unwrap();
                assert_eq!(caption_path(text), Some(resolved.to_str().unwrap()));
                assert!(text.contains("image/png, 1x1 px"), "{text}");
            }
            _ => panic!("expected a caption first"),
        }
        match &result.output[1] {
            ContentPart::ImageUrl { image_url } => {
                assert!(image_url.url.starts_with("data:image/png;base64,"));
            }
            _ => panic!("expected image content part"),
        }
    }

    /// A model with a given media policy.
    struct PolicyLlm(MediaPolicy);

    #[async_trait]
    impl crate::agent_loop::Llm for PolicyLlm {
        fn model_name(&self) -> &str {
            "policy-mock"
        }

        fn supports_vision(&self) -> bool {
            self.0.takes_images()
        }

        fn media_policy(&self) -> MediaPolicy {
            self.0
        }

        async fn chat(
            &self,
            _messages: Vec<lm_bridge::message::Message>,
            _tools: Vec<lm_bridge::tool::Tool>,
            _callbacks: crate::agent_loop::LlmCallbacks,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> Result<crate::agent_loop::types::StepResult, lm_bridge::errors::LlmError> {
            unimplemented!("policy mock is not expected to generate")
        }
    }

    fn ctx_with(policy: MediaPolicy, dir: &std::path::Path) -> ExecutableToolContext {
        let mut ctx = test_ctx();
        ctx.cwd = dir.to_path_buf();
        ctx.llm = Some(std::sync::Arc::new(PolicyLlm(policy)));
        ctx
    }

    /// Offered when something can see the media: a model that takes images
    /// or video, or a vision fallback for one that takes neither.
    #[test]
    fn offered_only_when_something_can_see_the_media() {
        let tool = ReadMediaTool::new();
        let temp = tempfile::tempdir().unwrap();
        let text_only = ctx_with(MediaPolicy::TEXT_ONLY, temp.path());
        assert!(!tool.is_available(&test_ctx()), "no model");
        assert!(!tool.is_available(&text_only), "a text-only model");
        assert!(tool.is_available(&ctx_with(MediaPolicy::BASELINE_VISION, temp.path())));
        let video_only = MediaPolicy {
            image_mimes: &[],
            video: VideoSupport::Inline,
            max_image_edge_px: None,
        };
        assert!(tool.is_available(&ctx_with(video_only, temp.path())));
        let mut fallback = text_only;
        fallback.vision_fallback = Some(std::sync::Arc::new(
            crate::vision_fallback::LlmVisionFallback::new(std::sync::Arc::new(PolicyLlm(
                MediaPolicy::BASELINE_VISION,
            ))),
        ));
        assert!(tool.is_available(&fallback), "a vision fallback");
    }

    /// A vision fallback that keeps what it was given.
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<(Vec<u8>, Option<String>)>>);

    #[async_trait]
    impl crate::vision_fallback::VisionFallback for Recorder {
        async fn describe_image(
            &self,
            _mime: &str,
            base64_data: &str,
            question: Option<&str>,
        ) -> Result<String, String> {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(base64_data)
                .unwrap();
            self.0
                .lock()
                .unwrap()
                .push((bytes, question.map(str::to_string)));
            Ok("an answer".to_string())
        }
    }

    /// For a model that reads no images, the fallback answers the question
    /// asked, and a full-resolution read reaches it at full size.
    #[tokio::test]
    async fn the_fallback_answers_the_question_at_the_resolution_asked() {
        let temp = tempfile::tempdir().unwrap();
        let wide = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(2_400, 60, |x, _| {
            image::Rgb([(x % 256) as u8, 0, 0])
        }));
        wide.save(temp.path().join("diagram.png")).unwrap();
        let recorder = std::sync::Arc::new(Recorder::default());
        let mut ctx = ctx_with(MediaPolicy::TEXT_ONLY, temp.path());
        ctx.vision_fallback = Some(recorder.clone());

        let answered = ReadMediaTool::new()
            .execute(
                json!({"path": "diagram.png", "question": "Which labels does it show?"}),
                &ctx,
                "t",
            )
            .await;
        assert!(
            answered
                .text_content()
                .contains("Vision analysis (asked: Which labels does it show?): an answer"),
            "{}",
            answered.text_content()
        );
        ReadMediaTool::new()
            .execute(
                json!({"path": "diagram.png", "full_resolution": true}),
                &ctx,
                "t",
            )
            .await;

        let seen = recorder.0.lock().unwrap();
        assert_eq!(seen[0].1.as_deref(), Some("Which labels does it show?"));
        assert_eq!(seen[1].1, None);
        let width = |bytes: &[u8]| image::load_from_memory(bytes).unwrap().width();
        assert!(
            width(&seen[0].0) <= image_compress::MAX_IMAGE_EDGE_PX,
            "downsampled by default"
        );
        assert_eq!(width(&seen[1].0), 2_400, "full resolution is full size");
    }

    fn encoded(format: image::ImageFormat) -> Vec<u8> {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(24, 24, |x, y| {
            image::Rgb([(x * 10) as u8, (y * 10) as u8, 128])
        }));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), format)
            .unwrap();
        buf
    }

    /// GLM rejects BMP, so a BMP reaches it as PNG, and the caption says so.
    #[tokio::test]
    async fn images_are_converted_to_a_format_the_provider_takes() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("shot.bmp"),
            encoded(image::ImageFormat::Bmp),
        )
        .unwrap();
        let glm = MediaPolicy {
            image_mimes: lm_bridge::media::GLM_IMAGE_MIMES,
            video: VideoSupport::Inline,
            max_image_edge_px: None,
        };
        let result = ReadMediaTool::new()
            .execute(
                json!({"path": "shot.bmp"}),
                &ctx_with(glm, temp.path()),
                "t",
            )
            .await;
        assert!(!result.is_error, "{}", result.text_content());
        assert!(result.text_content().contains("converted from image/bmp"));
        assert!(matches!(
            &result.output[1],
            ContentPart::ImageUrl { image_url } if image_url.url.starts_with("data:image/png;base64,")
        ));
    }

    /// DeepSeek takes at most 4096 px per side, so even a full-resolution
    /// read of a 6000 px image comes out within it.
    #[tokio::test]
    async fn a_full_resolution_read_keeps_to_deepseeks_edge_limit() {
        use base64::Engine as _;
        use lm_bridge::provider::ChatProvider;

        let temp = tempfile::tempdir().unwrap();
        let wide = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(6_000, 40, |x, y| {
            image::Rgb([(x % 256) as u8, (y * 6) as u8, 64])
        }));
        wide.save(temp.path().join("wide.png")).unwrap();
        let deepseek = lm_bridge::providers::DeepseekChatProvider::new(
            "deepseek-flash",
            None,
            "https://api.deepseek.com",
            lm_bridge::providers::openai_compat::HttpTimeouts::idle(
                std::time::Duration::from_secs(5),
            ),
        )
        .unwrap()
        .media_policy();

        let result = ReadMediaTool::new()
            .execute(
                json!({"path": "wide.png", "full_resolution": true}),
                &ctx_with(deepseek, temp.path()),
                "t",
            )
            .await;

        assert!(!result.is_error, "{}", result.text_content());
        assert!(
            result
                .text_content()
                .contains("downsampled from 6000x40 px"),
            "{}",
            result.text_content()
        );
        let ContentPart::ImageUrl { image_url } = &result.output[1] else {
            panic!("an image: {:?}", result.output);
        };
        let (_, data) = image_url.url.split_once(',').unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        let sent = image::load_from_memory(&bytes).unwrap();
        assert!(
            sent.width().max(sent.height()) <= lm_bridge::media::DEEPSEEK_MAX_IMAGE_EDGE_PX,
            "{}x{}",
            sent.width(),
            sent.height()
        );
    }

    /// A format that cannot be converted here fails with a command that fixes
    /// it; where the provider takes it, it goes as it is.
    #[tokio::test]
    async fn heic_goes_raw_to_kimi_and_fails_clearly_elsewhere() {
        let temp = tempfile::tempdir().unwrap();
        let heic = b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heicpayload".to_vec();
        std::fs::write(temp.path().join("IMG_1.HEIC"), &heic).unwrap();
        let kimi = MediaPolicy {
            image_mimes: lm_bridge::media::KIMI_IMAGE_MIMES,
            video: VideoSupport::Upload,
            max_image_edge_px: None,
        };
        let sent = ReadMediaTool::new()
            .execute(
                json!({"path": "IMG_1.HEIC"}),
                &ctx_with(kimi, temp.path()),
                "t",
            )
            .await;
        assert!(matches!(
            &sent.output[1],
            ContentPart::ImageUrl { image_url } if image_url.url.starts_with("data:image/heic;base64,")
        ));
        let refused = ReadMediaTool::new()
            .execute(
                json!({"path": "IMG_1.HEIC"}),
                &ctx_with(MediaPolicy::BASELINE_VISION, temp.path()),
                "t",
            )
            .await;
        assert!(refused.is_error);
        assert!(refused.text_content().contains("heif-convert"));
    }

    /// Video goes out only where the model takes it, inline within a size
    /// limit; elsewhere the result says why.
    #[tokio::test]
    async fn video_follows_the_policy() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("clip.mp4"),
            b"\x00\x00\x00\x18ftypmp42clip",
        )
        .unwrap();

        let described = ReadMediaTool::new()
            .execute(
                json!({"path": "clip.mp4"}),
                &ctx_with(MediaPolicy::BASELINE_VISION, temp.path()),
                "t",
            )
            .await;
        assert!(!described.is_error);
        assert!(described.text_content().contains("cannot be shown"));
        assert!(
            described
                .output
                .iter()
                .all(|p| matches!(p, ContentPart::Text { .. }))
        );

        let inline = MediaPolicy {
            image_mimes: lm_bridge::media::GLM_IMAGE_MIMES,
            video: VideoSupport::Inline,
            max_image_edge_px: None,
        };
        let sent = ReadMediaTool::new()
            .execute(
                json!({"path": "clip.mp4"}),
                &ctx_with(inline, temp.path()),
                "t",
            )
            .await;
        assert!(matches!(
            &sent.output[1],
            ContentPart::VideoUrl { video_url } if video_url.url.starts_with("data:video/mp4;base64,")
        ));

        let big = temp.path().join("long.mp4");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_INLINE_VIDEO_BYTES + 1)
            .unwrap();
        let refused = ReadMediaTool::new()
            .execute(
                json!({"path": "long.mp4"}),
                &ctx_with(inline, temp.path()),
                "t",
            )
            .await;
        assert!(refused.is_error);
        assert!(refused.text_content().contains("ffmpeg"));
    }

    #[tokio::test]
    async fn audio_and_svg_are_explained() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.wav"), b"RIFF....WAVE").unwrap();
        std::fs::write(temp.path().join("logo.svg"), "<svg/>").unwrap();
        let ctx = ctx_with(MediaPolicy::BASELINE_VISION, temp.path());
        let audio = ReadMediaTool::new()
            .execute(json!({"path": "a.wav"}), &ctx, "t")
            .await;
        assert!(audio.text_content().contains("takes audio input"));
        let svg = ReadMediaTool::new()
            .execute(json!({"path": "logo.svg"}), &ctx, "t")
            .await;
        assert!(svg.is_error);
        assert!(svg.text_content().contains("read_file"));
    }

    #[test]
    fn captions_name_their_file() {
        assert_eq!(
            caption_path("<system>Image file: /tmp/a b.png\nimage/png, 1 bytes.</system>"),
            Some("/tmp/a b.png")
        );
        assert_eq!(caption_path("<system>Something else</system>"), None);
    }

    #[tokio::test]
    async fn test_read_media_returns_text_summary_for_non_vision_model() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pixel.png");
        let bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x02\0\0\0\x90wS\xde\0\0\0\x0cIDATx\x9cc\xf8\x0f\0\0\x01\x01\x00\x05\x18\xd8N\0\0\x00\0IEND\xaeB`\x82";
        tokio::fs::write(&path, bytes).await.unwrap();

        let mut ctx = test_ctx();
        ctx.cwd = temp.path().to_path_buf();
        let tool = ReadMediaTool::new();
        let result = tool
            .execute(json!({"path": "pixel.png"}), &ctx, "test-id")
            .await;
        assert!(!result.is_error);
        let text = result.text_content();
        assert!(text.contains("Media file"));
        assert!(text.contains("image/png"));
        assert!(text.contains("does not support vision"));
    }

    #[tokio::test]
    async fn test_read_media_rejects_oversized_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("huge.mp4");
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .await
            .unwrap();
        // 100 MiB + 1 byte, which exceeds MAX_MEDIA_FILE_BYTES.
        file.set_len(100 * 1024 * 1024 + 1).await.unwrap();

        let mut ctx = test_ctx();
        ctx.cwd = temp.path().to_path_buf();
        let tool = ReadMediaTool::new();
        let result = tool
            .execute(json!({"path": "huge.mp4"}), &ctx, "test-id")
            .await;
        assert!(result.is_error);
        assert!(result.text_content().contains("too large"));
    }
}
