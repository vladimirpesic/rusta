//! Media on the wire: what each provider takes, and how much of it one
//! request may carry.
//!
//! History is durable and the model is not: a conversation that read a HEIC
//! photo on Kimi outlives a switch to GLM, and a video read on a model that
//! takes video outlives a switch to one that does not. Every request therefore
//! re-checks its media against the provider it is about to reach
//! ([`normalize_for_policy`]), and keeps the inline payload within a budget
//! ([`apply_request_media_budget`]), replacing what cannot go
//! with a note rather than sending something the endpoint will reject — a
//! rejected request is resent with the same history, so it would fail forever.

use std::io::Cursor;

use base64::Engine;

use crate::message::{ContentPart, Message};

/// Image formats every OpenAI-compatible vision endpoint is expected to take.
pub const BASELINE_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Kimi also takes BMP, HEIC and HEIF.
pub const KIMI_IMAGE_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
    "image/heic",
    "image/heif",
];

/// Z.AI documents JPEG and PNG. WebP also works; GIF is accepted but only its
/// first frame is read, and BMP is rejected with `1210` (checked against the
/// live API). GIF and BMP are therefore converted before they are sent.
pub const GLM_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/webp"];

/// DeepSeek documents JPEG, PNG, GIF and WebP for `deepseek-flash` (a GIF must
/// be a valid one — the live API rejects a truncated file as an unsupported
/// format). `deepseek-v4-pro` takes no images: the request is
/// accepted but the model answers that it cannot see them.
pub const DEEPSEEK_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The longest image edge DeepSeek takes in every request: it rejects an
/// image over 8192 px per side, and over 4096 px once a request carries 15
/// or more images (<https://api-docs.deepseek.com/guides/vision>).
pub const DEEPSEEK_MAX_IMAGE_EDGE_PX: u32 = 4_096;

/// How much of an inline image is decoded to read its dimensions from its
/// header: 96 KiB, as 128 Ki base64 characters, enough to get past the EXIF
/// blocks a JPEG may lead with.
const HEADER_PREFIX_CHARS: usize = 128 * 1024;

/// Inline media one request may carry before older media is dropped: 20 MB,
/// measured on the `data:` URLs.
pub const REQUEST_MEDIA_BUDGET_BYTES: usize = 20 * 1024 * 1024;

/// What a request is trimmed to once it is over
/// [`REQUEST_MEDIA_BUDGET_BYTES`], so the next few do not trim again.
pub const REQUEST_MEDIA_BUDGET_LOW_BYTES: usize = 10 * 1024 * 1024;

/// How video reaches a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoSupport {
    /// Not at all; `hint` says what to do instead.
    Unsupported { hint: &'static str },
    /// As an inline `data:` URL.
    Inline,
    /// Uploaded by the provider before the request (Kimi's Files API), then
    /// referenced by id.
    Upload,
}

/// What media a provider's model takes, and in which forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaPolicy {
    /// Image formats the model takes; empty when it takes no images.
    pub image_mimes: &'static [&'static str],
    /// How video reaches the model.
    pub video: VideoSupport,
    /// The longest image edge, in pixels, the model takes; `None` where none
    /// is documented. A larger image is scaled down when it is read, even at
    /// full resolution, and one already in the history is replaced with a
    /// note.
    pub max_image_edge_px: Option<u32>,
}

impl MediaPolicy {
    /// A model that takes no media.
    pub const TEXT_ONLY: Self = Self {
        image_mimes: &[],
        video: VideoSupport::Unsupported {
            hint: "the current model does not take video input",
        },
        max_image_edge_px: None,
    };

    /// The common case: the baseline image formats, no video.
    pub const BASELINE_VISION: Self = Self {
        image_mimes: BASELINE_IMAGE_MIMES,
        video: VideoSupport::Unsupported {
            hint: "the current model does not take video input",
        },
        max_image_edge_px: None,
    };

    /// Whether the model takes images at all.
    #[must_use]
    pub fn takes_images(&self) -> bool {
        !self.image_mimes.is_empty()
    }

    /// Whether the model takes images of type `mime`.
    #[must_use]
    pub fn accepts_image(&self, mime: &str) -> bool {
        let mime = normalize_mime(mime);
        self.image_mimes.iter().any(|m| *m == mime)
    }

    /// Whether the model takes video in any form.
    #[must_use]
    pub fn takes_video(&self) -> bool {
        !matches!(self.video, VideoSupport::Unsupported { .. })
    }
}

/// `mime` lowercased, without parameters, with `image/jpg` spelled
/// `image/jpeg`.
#[must_use]
pub fn normalize_mime(mime: &str) -> String {
    let base = mime.split(';').next().unwrap_or(mime).trim().to_lowercase();
    if base == "image/jpg" {
        "image/jpeg".to_string()
    } else {
        base
    }
}

/// Whether `url` is a `data:` URL.
#[must_use]
pub fn is_data_url(url: &str) -> bool {
    url.len() >= 5 && url[..5].eq_ignore_ascii_case("data:")
}

/// The MIME type and base64 payload of a `data:<mime>[;params];base64,<data>`
/// URL.
#[must_use]
pub fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    if !is_data_url(url) {
        return None;
    }
    let (header, data) = url[5..].split_once(',')?;
    let mut fields = header.split(';');
    let mime = fields.next()?.trim();
    if mime.is_empty() || !fields.any(|f| f.trim().eq_ignore_ascii_case("base64")) {
        return None;
    }
    Some((mime, data))
}

/// The note that stands in for media a request cannot carry.
fn omitted(kind: &str, why: &str) -> ContentPart {
    ContentPart::text(format!("[{kind} omitted: {why}]"))
}

/// The pixel dimensions of an image sent as base64 `data`, read from its
/// header without decoding the image; `None` when they cannot be read.
fn image_dimensions(data: &str) -> Option<(u32, u32)> {
    // A prefix of whole four-character groups decodes on its own.
    let prefix = &data[..data.len().min(HEADER_PREFIX_CHARS)];
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(prefix)
        .ok()?;
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// The note for an inline image `policy` cannot take as it is: one of a
/// format the model does not accept, or one larger than its edge limit.
fn image_refusal(mime: &str, data: &str, policy: &MediaPolicy) -> Option<ContentPart> {
    let mime = normalize_mime(mime);
    if !policy.accepts_image(&mime) {
        return Some(omitted(
            "image",
            &format!(
                "{mime} is not a format this provider accepts; convert the file to PNG or \
                 JPEG and read it again"
            ),
        ));
    }
    let limit = policy.max_image_edge_px?;
    let (width, height) = image_dimensions(data)?;
    (width.max(height) > limit).then(|| {
        omitted(
            "image",
            &format!(
                "{width}x{height} px is larger than the {limit} px per side this provider \
                 takes; read the file again to send it scaled down"
            ),
        )
    })
}

/// Replace the media in `messages` that `policy` cannot take with notes saying
/// why: images of formats the model does not accept or larger than it takes,
/// video it cannot take, and audio, which no provider Rusta speaks to accepts.
/// Returns how many parts were replaced.
///
/// Images on a text-only model are left for the serializer, which already
/// degrades them; `ms://` and remote references are left as they are.
pub fn normalize_for_policy(messages: &mut [Message], policy: &MediaPolicy) -> usize {
    let mut replaced = 0;
    for message in messages.iter_mut() {
        for part in &mut message.content {
            let note = match part {
                ContentPart::ImageUrl { image_url } if policy.takes_images() => {
                    parse_data_url(&image_url.url)
                        .and_then(|(mime, data)| image_refusal(mime, data, policy))
                }
                ContentPart::VideoUrl { .. } => match policy.video {
                    VideoSupport::Unsupported { hint } => Some(omitted("video", hint)),
                    VideoSupport::Inline | VideoSupport::Upload => None,
                },
                ContentPart::AudioUrl { .. } => Some(omitted(
                    "audio",
                    "the current model does not take audio input",
                )),
                _ => None,
            };
            if let Some(note) = note {
                *part = note;
                replaced += 1;
            }
        }
    }
    replaced
}

/// Keep the inline media of one request within
/// [`REQUEST_MEDIA_BUDGET_BYTES`]: when the `data:` URLs in `messages` add up
/// to more, the oldest are replaced with a note until the rest fit in
/// [`REQUEST_MEDIA_BUDGET_LOW_BYTES`]. Returns how many were dropped.
///
/// Media sent by reference (`ms://`, `https://`) costs the request almost
/// nothing and is not counted.
pub fn apply_request_media_budget(messages: &mut [Message]) -> usize {
    let mut inline: Vec<(usize, usize, usize)> = Vec::new();
    for (m, message) in messages.iter().enumerate() {
        for (p, part) in message.content.iter().enumerate() {
            if let Some(url) = media_url(part).filter(|url| is_data_url(url)) {
                inline.push((m, p, url.len()));
            }
        }
    }
    let mut total: usize = inline.iter().map(|(_, _, len)| len).sum();
    if total <= REQUEST_MEDIA_BUDGET_BYTES {
        return 0;
    }
    let mut dropped = 0;
    for (m, p, len) in inline {
        if total <= REQUEST_MEDIA_BUDGET_LOW_BYTES {
            break;
        }
        let part = &mut messages[m].content[p];
        let kind = media_kind(part);
        *part = omitted(kind, "dropped to fit the request's media budget");
        total -= len;
        dropped += 1;
    }
    tracing::warn!(
        "conversation media exceeded the {} MB per-request budget; {dropped} older item(s) were \
         omitted from this request",
        REQUEST_MEDIA_BUDGET_BYTES / (1024 * 1024)
    );
    dropped
}

/// A file extension for `mime`, for naming saved media; `bin` when unknown.
#[must_use]
pub fn extension_for_mime(mime: &str) -> &'static str {
    match normalize_mime(mime).as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/heic" => "heic",
        "image/heif" => "heif",
        "image/avif" => "avif",
        "image/tiff" => "tiff",
        "image/svg+xml" => "svg",
        "video/mp4" => "mp4",
        "video/mpeg" => "mpeg",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/x-msvideo" => "avi",
        "video/x-flv" => "flv",
        "video/3gpp" => "3gp",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "audio/mp4" => "m4a",
        "audio/flac" => "flac",
        "application/pdf" => "pdf",
        "application/json" => "json",
        "application/zip" => "zip",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "text/html" => "html",
        _ => "bin",
    }
}

/// The URL of a media part.
#[must_use]
pub fn media_url(part: &ContentPart) -> Option<&str> {
    match part {
        ContentPart::ImageUrl { image_url } => Some(&image_url.url),
        ContentPart::VideoUrl { video_url } => Some(&video_url.url),
        ContentPart::AudioUrl { audio_url } => Some(&audio_url.url),
        _ => None,
    }
}

/// `image`, `video` or `audio` for a media part; `media` otherwise.
#[must_use]
pub fn media_kind(part: &ContentPart) -> &'static str {
    match part {
        ContentPart::ImageUrl { .. } => "image",
        ContentPart::VideoUrl { .. } => "video",
        ContentPart::AudioUrl { .. } => "audio",
        _ => "media",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ImageUrl, UrlRef};

    fn image(url: &str) -> ContentPart {
        ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: url.to_string(),
                id: None,
            },
        }
    }

    fn video(url: &str) -> ContentPart {
        ContentPart::VideoUrl {
            video_url: UrlRef {
                url: url.to_string(),
                id: None,
            },
        }
    }

    fn user_with(parts: Vec<ContentPart>) -> Message {
        let mut message = Message::user("");
        message.content = parts;
        message
    }

    #[test]
    fn data_urls_are_parsed() {
        assert_eq!(
            parse_data_url("data:image/png;base64,AAAA"),
            Some(("image/png", "AAAA"))
        );
        assert_eq!(
            parse_data_url("DATA:video/mp4;codecs=avc1;base64,BBBB"),
            Some(("video/mp4", "BBBB"))
        );
        assert_eq!(parse_data_url("data:text/plain,hello"), None);
        assert_eq!(parse_data_url("ms://abc"), None);
        assert_eq!(normalize_mime("Image/JPG; q=1"), "image/jpeg");
    }

    #[test]
    fn policies_answer_for_formats_and_video() {
        let glm = MediaPolicy {
            image_mimes: GLM_IMAGE_MIMES,
            video: VideoSupport::Inline,
            max_image_edge_px: None,
        };
        assert!(glm.accepts_image("image/webp"));
        assert!(!glm.accepts_image("image/bmp"));
        assert!(glm.takes_video());
        assert!(!MediaPolicy::TEXT_ONLY.takes_images());
        assert!(!MediaPolicy::BASELINE_VISION.takes_video());
    }

    /// A PNG of `width` x `height` as a `data:` URL.
    fn png_data_url(width: u32, height: u32) -> String {
        let image = image::DynamicImage::ImageLuma8(image::GrayImage::new(width, height));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    /// An image in the history larger than the provider's edge limit — read
    /// at full resolution under another provider — becomes a note; one
    /// within the limit stays, and a provider without a limit keeps both.
    #[test]
    fn images_past_the_edge_limit_are_replaced_with_notes() {
        let history = || {
            vec![user_with(vec![
                image(&png_data_url(5_000, 4)),
                image(&png_data_url(4_096, 4)),
            ])]
        };
        let deepseek = MediaPolicy {
            image_mimes: DEEPSEEK_IMAGE_MIMES,
            video: VideoSupport::Unsupported { hint: "no video" },
            max_image_edge_px: Some(DEEPSEEK_MAX_IMAGE_EDGE_PX),
        };
        let mut messages = history();
        assert_eq!(normalize_for_policy(&mut messages, &deepseek), 1);
        let parts = &messages[0].content;
        assert_eq!(
            parts[0],
            ContentPart::text(
                "[image omitted: 5000x4 px is larger than the 4096 px per side this provider \
                 takes; read the file again to send it scaled down]"
            )
        );
        assert!(matches!(parts[1], ContentPart::ImageUrl { .. }));

        let mut messages = history();
        assert_eq!(
            normalize_for_policy(&mut messages, &MediaPolicy::BASELINE_VISION),
            0
        );
    }

    /// What a provider cannot take becomes a note; what it can stays.
    #[test]
    fn unsupported_media_is_replaced_with_notes() {
        let mut messages = vec![user_with(vec![
            image("data:image/bmp;base64,AAAA"),
            image("data:image/png;base64,AAAA"),
            image("ms://file-1"),
            video("data:video/mp4;base64,AAAA"),
            ContentPart::AudioUrl {
                audio_url: UrlRef {
                    url: "data:audio/wav;base64,AAAA".into(),
                    id: None,
                },
            },
        ])];
        let policy = MediaPolicy {
            image_mimes: GLM_IMAGE_MIMES,
            video: VideoSupport::Unsupported {
                hint: "use the analyze_video tool",
            },
            max_image_edge_px: None,
        };
        assert_eq!(normalize_for_policy(&mut messages, &policy), 3);
        let parts = &messages[0].content;
        assert!(matches!(&parts[0], ContentPart::Text { text } if text.contains("image/bmp")));
        assert!(matches!(&parts[1], ContentPart::ImageUrl { .. }));
        assert!(matches!(&parts[2], ContentPart::ImageUrl { .. }));
        assert!(matches!(&parts[3], ContentPart::Text { text } if text.contains("analyze_video")));
        assert!(
            matches!(&parts[4], ContentPart::Text { text } if text.starts_with("[audio omitted"))
        );
    }

    /// Past the budget, the oldest inline media goes first, down to the low
    /// mark; references are not counted.
    #[test]
    fn the_request_budget_drops_the_oldest_inline_media() {
        let big = format!("data:image/png;base64,{}", "A".repeat(6 * 1024 * 1024));
        let mut messages = vec![
            user_with(vec![image(&big)]),
            user_with(vec![image("ms://kept-by-reference")]),
            user_with(vec![image(&big)]),
            user_with(vec![image(&big)]),
            user_with(vec![image(&big)]),
        ];
        let dropped = apply_request_media_budget(&mut messages);
        assert_eq!(dropped, 3);
        for i in [0, 2, 3] {
            assert!(
                matches!(&messages[i].content[0], ContentPart::Text { text } if text.contains("media budget")),
                "message {i} should have been dropped"
            );
        }
        assert!(matches!(
            &messages[1].content[0],
            ContentPart::ImageUrl { .. }
        ));
        assert!(matches!(
            &messages[4].content[0],
            ContentPart::ImageUrl { .. }
        ));

        let mut small = vec![user_with(vec![image("data:image/png;base64,AAAA")])];
        assert_eq!(apply_request_media_budget(&mut small), 0);
    }
}
