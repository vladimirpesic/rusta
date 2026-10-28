//! Preparing an image for the model: a format the provider accepts, within a
//! pixel and byte budget.
//!
//! The rules:
//!
//! - The actual format is sniffed from the bytes, not trusted to the file
//!   extension.
//! - An image the provider accepts, inside the edge ceiling and the byte
//!   budget, is sent untouched.
//! - Otherwise it is decoded (PNG, JPEG, GIF, WebP, BMP), downsampled so the
//!   longest edge fits the ceiling, and re-encoded in an accepted format: PNG
//!   first when the source is lossless and the result fits, then JPEG at
//!   stepped-down quality, then smaller edges.
//! - EXIF orientation is baked into the pixels, and every dimension reported
//!   here is in that upright space — the same space the model sees.
//! - Formats that cannot be decoded here (HEIC, HEIF, AVIF, TIFF) are sent raw
//!   when the provider accepts them and they fit the budget; otherwise the
//!   caller gets an error with a conversion command, never a silent pass
//!   through that the provider would reject.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, metadata::Orientation};

/// Built-in longest-edge ceiling (px). Larger images are scaled down to fit.
///
/// This is the default only: the effective ceiling is resolved per call by
/// [`resolve_max_image_edge_px`], which lets [`MAX_IMAGE_EDGE_ENV`] override it.
/// 2000px keeps a full-page screenshot legible while leaving room under the
/// providers' request-body limits for an image-heavy conversation.
pub const MAX_IMAGE_EDGE_PX: u32 = 2_000;

/// Environment variable overriding the longest-edge ceiling (px).
///
/// Read live on every resolution, so it applies without wiring. A value that is
/// not a positive integer is ignored.
pub const MAX_IMAGE_EDGE_ENV: &str = "RUSTA_IMAGE_MAX_EDGE_PX";

/// Pixel-count ceiling above which an image is not decoded. This avoids
/// decoding obvious decompression bombs into huge bitmaps.
const MAX_DECODE_PIXELS: u64 = 100_000_000;

/// Raw-byte ceiling above which an image is not decoded.
const MAX_DECODE_BYTES: usize = 64 * 1024 * 1024;

/// JPEG quality steps tried in order until the byte budget is met.
const JPEG_QUALITY_STEPS: [u8; 4] = [80, 60, 40, 20];

/// Longest edges tried, in order, when the byte budget cannot be met at the
/// resolved ceiling. Steps at or above that ceiling are skipped, so with the
/// built-in 2000px ceiling the first entry is a no-op.
const FALLBACK_EDGES_PX: [u32; 6] = [2_000, 1_000, 768, 512, 384, 256];

/// Parses an edge override as a positive integer, ignoring anything else — an
/// empty, zero, or malformed value must not collapse the ceiling to nothing.
fn parse_edge_override(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok().filter(|&px| px > 0)
}

/// Reads `MAX_IMAGE_EDGE_ENV`, or `None` when unset or malformed.
fn max_image_edge_from_env() -> Option<u32> {
    parse_edge_override(&std::env::var(MAX_IMAGE_EDGE_ENV).ok()?)
}

/// The longest-edge ceiling in force for this call: the environment override
/// when it names a positive integer, otherwise [`MAX_IMAGE_EDGE_PX`].
pub fn resolve_max_image_edge_px() -> u32 {
    max_image_edge_from_env().unwrap_or(MAX_IMAGE_EDGE_PX)
}

/// What a prepared image must fit.
#[derive(Debug, Clone, Copy)]
pub struct ImageTarget<'a> {
    /// Formats the provider accepts.
    pub accepted: &'a [&'a str],
    /// Raw-byte budget; `0` sets none.
    pub byte_budget: usize,
    /// Longest-edge ceiling in pixels.
    pub max_edge: u32,
    /// The provider's own longest-edge limit, which even `full_resolution`
    /// keeps to; `None` where it documents none.
    pub provider_max_edge: Option<u32>,
    /// Keep native resolution and bytes (`full_resolution`), within the
    /// provider's edge limit. A format the provider does not accept is still
    /// converted.
    pub full_resolution: bool,
}

impl ImageTarget<'_> {
    /// The longest edge the image may keep: the provider's limit when it is
    /// read at full resolution, otherwise the ceiling, never past that limit.
    fn edge_limit(&self) -> u32 {
        let provider = self.provider_max_edge.unwrap_or(u32::MAX);
        if self.full_resolution {
            provider
        } else {
            self.max_edge.min(provider)
        }
    }

    fn accepts(&self, mime: &str) -> bool {
        self.accepted.contains(&mime)
    }

    fn fits(&self, len: usize) -> bool {
        self.byte_budget == 0 || len <= self.byte_budget
    }
}

/// An image ready to send.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    /// Bytes to send: the re-encoded image, or the original when unchanged.
    pub bytes: Vec<u8>,
    /// MIME type of `bytes`.
    pub mime_type: String,
    /// Pixel dimensions of `bytes` (upright), when known.
    pub dimensions: Option<(u32, u32)>,
    /// The upright dimensions before downsampling, when it was downsampled.
    pub downsampled_from: Option<(u32, u32)>,
    /// The original format, when the image was converted to another.
    pub converted_from: Option<String>,
}

#[cfg(test)]
impl PreparedImage {
    /// Whether `bytes` differs from the input.
    fn changed(&self) -> bool {
        self.downsampled_from.is_some() || self.converted_from.is_some()
    }
}

/// Why an image cannot be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// The provider does not accept `mime`, and it cannot be converted here.
    Unconvertible { mime: String },
    /// Accepted, but larger than the byte budget, and it cannot be
    /// re-encoded here.
    TooLargeToShrink {
        mime: String,
        bytes: usize,
        budget: usize,
    },
    /// The bytes do not decode as an image.
    Undecodable { mime: String },
    /// Larger than the provider's edge limit, and too large to decode here
    /// to scale it down.
    OverEdgeLimit {
        mime: String,
        width: u32,
        height: u32,
        limit: u32,
    },
}

impl ImageError {
    /// What went wrong, and a command that fixes it, for the image at `path`.
    #[must_use]
    pub fn describe(&self, path: &std::path::Path) -> String {
        let shown = path.display();
        let converted = path.with_extension("jpg");
        let convert = format!(
            "Convert it to JPEG first, then read the converted file — for example `magick \
             \"{shown}\" \"{jpg}\"` (ImageMagick), `sips -s format jpeg \"{shown}\" --out \
             \"{jpg}\"` (macOS), or `heif-convert \"{shown}\" \"{jpg}\"` (HEIC/HEIF on Linux, \
             package libheif-examples).",
            jpg = converted.display()
        );
        match self {
            Self::Unconvertible { mime } => format!(
                "{shown} is a {mime} image, a format the current provider does not accept and \
                 read_media cannot convert. {convert}"
            ),
            Self::TooLargeToShrink {
                mime,
                bytes,
                budget,
            } => format!(
                "{shown} is a {bytes}-byte {mime} image, over the {budget}-byte per-image limit, \
                 and this format cannot be downsampled here. {convert}"
            ),
            Self::Undecodable { mime } => format!(
                "{shown} does not decode as a {mime} image; it may be damaged or not an image \
                 at all."
            ),
            Self::OverEdgeLimit {
                mime,
                width,
                height,
                limit,
            } => {
                let scaled = path.with_extension("scaled.jpg");
                format!(
                    "{shown} is a {width}x{height} px {mime} image, past the {limit} px per side \
                     the current provider takes, and too large to scale down here. Scale it \
                     down first, then read the scaled file — for example `magick \"{shown}\" \
                     -resize {limit}x{limit} \"{out}\"` (ImageMagick) or `sips -Z {limit} \
                     \"{shown}\" --out \"{out}\"` (macOS).",
                    out = scaled.display()
                )
            }
        }
    }
}

/// The image format `bytes` actually hold, sniffed from their magic numbers.
#[must_use]
pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if let Some(brand) = bytes
        .get(8..12)
        .filter(|_| bytes.get(4..8) == Some(b"ftyp"))
    {
        return match brand {
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" => Some("image/heic"),
            b"mif1" | b"msf1" | b"heif" => Some("image/heif"),
            b"avif" | b"avis" => Some("image/avif"),
            _ => None,
        };
    }
    match image::guess_format(bytes).ok()? {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::WebP => Some("image/webp"),
        ImageFormat::Bmp => Some("image/bmp"),
        ImageFormat::Tiff => Some("image/tiff"),
        ImageFormat::Avif => Some("image/avif"),
        ImageFormat::Ico => Some("image/x-icon"),
        _ => None,
    }
}

/// Formats this build can decode and re-encode from.
fn decodable(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
    )
}

/// Prepare `bytes` (declared as `declared_mime`, corrected by sniffing) for a
/// provider that wants `target`.
pub fn prepare_image(
    bytes: &[u8],
    declared_mime: &str,
    target: &ImageTarget<'_>,
) -> Result<PreparedImage, ImageError> {
    let mime = sniff_image_mime(bytes).unwrap_or(declared_mime).to_string();
    let accepted = target.accepts(&mime);
    let dimensions = if decodable(&mime) {
        sniff_dimensions(bytes)
    } else {
        None
    };
    let unchanged = || PreparedImage {
        bytes: bytes.to_vec(),
        mime_type: mime.clone(),
        dimensions,
        downsampled_from: None,
        converted_from: None,
    };

    let edge_limit = target.edge_limit();
    let within_edge = dimensions.is_none_or(|(w, h)| w.max(h) <= edge_limit);
    if accepted && within_edge && (target.full_resolution || target.fits(bytes.len())) {
        return Ok(unchanged());
    }
    if !decodable(&mime) {
        return match (accepted, target.fits(bytes.len())) {
            // Cannot be resized; within budget it goes as it is.
            (true, true) => Ok(unchanged()),
            (true, false) => Err(ImageError::TooLargeToShrink {
                mime,
                bytes: bytes.len(),
                budget: target.byte_budget,
            }),
            (false, _) => Err(ImageError::Unconvertible { mime }),
        };
    }
    let Some((width, height)) = dimensions else {
        return Err(ImageError::Undecodable { mime });
    };
    let too_big_to_decode =
        bytes.len() > MAX_DECODE_BYTES || u64::from(width) * u64::from(height) > MAX_DECODE_PIXELS;
    if too_big_to_decode {
        if let Some(limit) = target.provider_max_edge
            && width.max(height) > limit
        {
            return Err(ImageError::OverEdgeLimit {
                mime,
                width,
                height,
                limit,
            });
        }
        return if accepted && target.fits(bytes.len()) {
            Ok(unchanged())
        } else {
            Err(ImageError::TooLargeToShrink {
                mime,
                bytes: bytes.len(),
                budget: target.byte_budget,
            })
        };
    }
    let image =
        decode_upright(bytes).ok_or_else(|| ImageError::Undecodable { mime: mime.clone() })?;

    // Lossless sources try PNG first: a screenshot's text stays sharp.
    let lossless_first = mime != "image/jpeg";
    let encoded = encode_stepping_down_edges(image, edge_limit, target, lossless_first).ok_or(
        ImageError::TooLargeToShrink {
            mime: mime.clone(),
            bytes: bytes.len(),
            budget: target.byte_budget,
        },
    )?;
    let downsampled = (encoded.width, encoded.height) != (width, height);
    Ok(PreparedImage {
        converted_from: (encoded.mime != mime).then(|| mime.clone()),
        bytes: encoded.bytes,
        mime_type: encoded.mime.to_string(),
        dimensions: Some((encoded.width, encoded.height)),
        downsampled_from: downsampled.then_some((width, height)),
    })
}

/// One encoding of an image.
struct Encoded {
    bytes: Vec<u8>,
    mime: &'static str,
    width: u32,
    height: u32,
}

/// The ladder steps that actually shrink an image already fitted to `max_edge`.
///
/// `fit_within_edge` never upscales, so a step at or above the ceiling would
/// return the image untouched and waste an encode proving it.
fn fallback_edges_below(max_edge: u32) -> impl Iterator<Item = u32> {
    FALLBACK_EDGES_PX.into_iter().filter(move |&e| e < max_edge)
}

/// Fits `image` to `max_edge`, then walks [`FALLBACK_EDGES_PX`] downwards until
/// an encoding fits the budget.
///
/// The ladder descends, so each step re-samples the previous step's output
/// rather than the full-size original.
fn encode_stepping_down_edges(
    image: DynamicImage,
    max_edge: u32,
    target: &ImageTarget<'_>,
    lossless_first: bool,
) -> Option<Encoded> {
    let mut current = fit_within_edge(image, max_edge);
    if let Some(encoded) = encode_within_budget(&current, target, lossless_first) {
        return Some(encoded);
    }
    let start = current.width().max(current.height());
    for edge in fallback_edges_below(start) {
        current = fit_within_edge(current, edge);
        if let Some(encoded) = encode_within_budget(&current, target, lossless_first) {
            return Some(encoded);
        }
    }
    None
}

/// The first encoding of `image` in an accepted format that fits the budget:
/// PNG (when `lossless_first`), then JPEG at stepped-down quality.
fn encode_within_budget(
    image: &DynamicImage,
    target: &ImageTarget<'_>,
    lossless_first: bool,
) -> Option<Encoded> {
    let (width, height) = (image.width(), image.height());
    if lossless_first && target.accepts("image/png") {
        let mut buf = Vec::new();
        if image
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .is_ok()
            && target.fits(buf.len())
        {
            return Some(Encoded {
                bytes: buf,
                mime: "image/png",
                width,
                height,
            });
        }
    }
    if !target.accepts("image/jpeg") {
        return None;
    }
    let rgb = flatten_onto_white(image);
    for quality in JPEG_QUALITY_STEPS {
        let mut buf = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
        if encoder.encode_image(&rgb).is_err() {
            continue;
        }
        if target.fits(buf.len()) {
            return Some(Encoded {
                bytes: buf,
                mime: "image/jpeg",
                width,
                height,
            });
        }
    }
    None
}

/// `image` as RGB, with any transparency composited onto white — JPEG has no
/// alpha channel, and dropping it would expose whatever colour the
/// transparent pixels happen to store (usually black).
fn flatten_onto_white(image: &DynamicImage) -> image::RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend = |c: u8| {
            let alpha = u32::from(a);
            ((u32::from(c) * alpha + 255 * (255 - alpha)) / 255) as u8
        };
        image::Rgb([blend(r), blend(g), blend(b)])
    })
}

/// True when the orientation transposes the image's axes, so that the stored
/// width becomes the displayed height.
fn swaps_axes(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// Dimensions as the image is meant to be *displayed*, i.e. after EXIF
/// orientation is applied. A portrait photo shot on a phone is commonly stored
/// landscape with `Rotate90`; reporting its stored dimensions swaps width and
/// height relative to the picture the model actually sees.
///
/// Reads headers only — never decodes pixels, so the decompression-bomb guard
/// downstream still runs before any bitmap is allocated.
fn sniff_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    Some(if swaps_axes(orientation) {
        (height, width)
    } else {
        (width, height)
    })
}

/// Decodes `bytes`, baking any EXIF orientation into the pixels.
///
/// Re-encoding drops the EXIF block, so an image rotated only by its metadata
/// would reach the model sideways. Applying the transform here means the pixels
/// carry the orientation and the metadata is no longer needed.
fn decode_upright(bytes: &[u8]) -> Option<DynamicImage> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    Some(image)
}

fn fit_within_edge(image: DynamicImage, max_edge: u32) -> DynamicImage {
    if image.width().max(image.height()) <= max_edge {
        return image;
    }
    // `resize` fits the image inside the box while preserving the aspect ratio,
    // so a square box scales by exactly `max_edge / longest_edge`. Rounding each
    // axis separately and passing that as the box would instead let the shorter
    // axis' rounding-down pick the scale, landing under the cap (a 3400x40 image
    // came out 2975 wide, not 3000).
    image.resize(max_edge, max_edge, image::imageops::FilterType::Triangle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASELINE: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];
    const GLM: &[&str] = &["image/png", "image/jpeg", "image/webp"];

    fn target(accepted: &'static [&'static str], budget: usize) -> ImageTarget<'static> {
        ImageTarget {
            accepted,
            byte_budget: budget,
            max_edge: MAX_IMAGE_EDGE_PX,
            provider_max_edge: None,
            full_resolution: false,
        }
    }

    fn encode(img: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), format)
            .expect("encode fixture");
        buf
    }

    /// A gradient, so JPEG cannot compress it to nothing and the byte-budget
    /// path stays representative.
    fn gradient(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 239) as u8])
        }))
    }

    fn jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90)
            .encode_image(&gradient(width, height).to_rgb8())
            .expect("encode fixture");
        buf
    }

    fn png_crc32(data: &[u8]) -> u32 {
        let mut c: u32 = !0;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        let mut chunk = Vec::from(&kind[..]);
        chunk.extend_from_slice(data);
        out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        out.extend_from_slice(&chunk);
        out.extend_from_slice(&png_crc32(&chunk).to_be_bytes());
    }

    /// A PNG whose header claims `width`x`height` but whose pixel data is a
    /// single byte: the header parses, and decoding it would allocate the
    /// full bitmap before discovering the data is short.
    fn make_png_header_claiming(width: u32, height: u32) -> Vec<u8> {
        let mut out = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB, no interlace
        png_chunk(&mut out, b"IHDR", &ihdr);
        // A zlib stream holding one stored byte (header, final stored block,
        // Adler-32 of [0]).
        png_chunk(
            &mut out,
            b"IDAT",
            &[
                0x78, 0x01, 0x01, 0x01, 0x00, 0xFE, 0xFF, 0x00, 0x00, 0x01, 0x00, 0x01,
            ],
        );
        png_chunk(&mut out, b"IEND", &[]);
        out
    }

    /// Splices a minimal big-endian EXIF APP1 segment carrying just an
    /// Orientation tag into a JPEG, immediately after the SOI marker — where a
    /// camera would put it.
    fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "fixture must start with SOI");

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"MM\x00\x2a"); // big-endian TIFF magic
        tiff.extend_from_slice(&8u32.to_be_bytes()); // offset of IFD0
        tiff.extend_from_slice(&1u16.to_be_bytes()); // one entry
        tiff.extend_from_slice(&0x0112u16.to_be_bytes()); // tag: Orientation
        tiff.extend_from_slice(&3u16.to_be_bytes()); // type: SHORT
        tiff.extend_from_slice(&1u32.to_be_bytes()); // count
        tiff.extend_from_slice(&orientation.to_be_bytes()); // value, left-aligned
        tiff.extend_from_slice(&[0, 0]); // ...padded to 4 bytes
        tiff.extend_from_slice(&0u32.to_be_bytes()); // no next IFD

        let mut payload = Vec::from(&b"Exif\x00\x00"[..]);
        payload.extend_from_slice(&tiff);

        let mut out = Vec::from(&jpeg[..2]);
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
        out.extend_from_slice(&payload);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[test]
    fn formats_are_sniffed_from_the_bytes() {
        assert_eq!(sniff_image_mime(&jpeg_bytes(8, 8)), Some("image/jpeg"));
        let png = encode(&gradient(8, 8), ImageFormat::Png);
        assert_eq!(sniff_image_mime(&png), Some("image/png"));
        let webp = encode(&gradient(8, 8), ImageFormat::WebP);
        assert_eq!(sniff_image_mime(&webp), Some("image/webp"));
        let bmp = encode(&gradient(8, 8), ImageFormat::Bmp);
        assert_eq!(sniff_image_mime(&bmp), Some("image/bmp"));
        let heic = b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heic";
        assert_eq!(sniff_image_mime(heic), Some("image/heic"));
        assert_eq!(sniff_image_mime(b"plain text"), None);
    }

    /// An accepted image inside the ceiling and the budget is untouched.
    #[test]
    fn a_fitting_image_passes_through() {
        let bytes = jpeg_bytes(1_800, 40);
        let prepared = prepare_image(&bytes, "image/jpeg", &target(BASELINE, 262_144)).unwrap();
        assert!(!prepared.changed());
        assert_eq!(prepared.bytes, bytes);
        assert_eq!(prepared.dimensions, Some((1_800, 40)));
    }

    /// The declared type is corrected by sniffing: a PNG named `.jpg` is a PNG.
    #[test]
    fn the_declared_type_is_corrected() {
        let png = encode(&gradient(16, 16), ImageFormat::Png);
        let prepared = prepare_image(&png, "image/jpeg", &target(BASELINE, 262_144)).unwrap();
        assert_eq!(prepared.mime_type, "image/png");
        assert!(!prepared.changed());
    }

    /// A format the provider rejects is converted — WebP stays WebP for GLM,
    /// but BMP and GIF become PNG (lossless first).
    #[test]
    fn rejected_formats_are_converted() {
        let bmp = encode(&gradient(32, 32), ImageFormat::Bmp);
        let prepared = prepare_image(&bmp, "image/bmp", &target(GLM, 262_144)).unwrap();
        assert_eq!(prepared.mime_type, "image/png");
        assert_eq!(prepared.converted_from.as_deref(), Some("image/bmp"));
        assert_eq!(sniff_image_mime(&prepared.bytes), Some("image/png"));

        let gif = encode(&gradient(32, 32), ImageFormat::Gif);
        let prepared = prepare_image(&gif, "image/gif", &target(GLM, 262_144)).unwrap();
        assert_eq!(prepared.mime_type, "image/png");

        let webp = encode(&gradient(32, 32), ImageFormat::WebP);
        let prepared = prepare_image(&webp, "image/webp", &target(GLM, 262_144)).unwrap();
        assert_eq!(prepared.mime_type, "image/webp");
        assert!(!prepared.changed());
    }

    /// Over the budget, a large photo steps down to JPEG within it.
    #[test]
    fn oversized_images_are_brought_under_the_budget() {
        let noisy = DynamicImage::ImageRgb8(image::RgbImage::from_fn(1_600, 1_200, |x, y| {
            let v = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) as u8;
            image::Rgb([v, v.rotate_left(3), v.rotate_left(5)])
        }));
        let png = encode(&noisy, ImageFormat::Png);
        assert!(png.len() > 262_144);
        let prepared = prepare_image(&png, "image/png", &target(BASELINE, 262_144)).unwrap();
        assert!(prepared.bytes.len() <= 262_144, "{}", prepared.bytes.len());
        assert_eq!(prepared.mime_type, "image/jpeg");
        assert_eq!(prepared.converted_from.as_deref(), Some("image/png"));
    }

    /// Formats that cannot be decoded here go raw where accepted and small
    /// enough, and fail clearly otherwise.
    #[test]
    fn undecodable_formats_go_raw_or_fail_clearly() {
        let heic = b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heicpayload".to_vec();
        let kimi: &[&str] = &["image/png", "image/jpeg", "image/heic"];
        let prepared = prepare_image(&heic, "image/heic", &target(kimi, 262_144)).unwrap();
        assert_eq!(prepared.mime_type, "image/heic");
        assert!(!prepared.changed());

        let err = prepare_image(&heic, "image/heic", &target(GLM, 262_144)).unwrap_err();
        assert_eq!(
            err,
            ImageError::Unconvertible {
                mime: "image/heic".into()
            }
        );
        let message = err.describe(std::path::Path::new("/p/IMG_1.HEIC"));
        assert!(message.contains("heif-convert"), "{message}");
        assert!(message.contains("/p/IMG_1.jpg"), "{message}");

        let err = prepare_image(&heic, "image/heic", &target(kimi, 8)).unwrap_err();
        assert!(
            matches!(err, ImageError::TooLargeToShrink { .. }),
            "{err:?}"
        );
    }

    /// `full_resolution` keeps an accepted image as it is.
    #[test]
    fn full_resolution_skips_downscaling() {
        let bytes = jpeg_bytes(3_000, 100);
        let mut full = target(BASELINE, 8);
        full.full_resolution = true;
        let prepared = prepare_image(&bytes, "image/jpeg", &full).unwrap();
        assert!(!prepared.changed());
    }

    /// A provider's edge limit holds even at full resolution: DeepSeek takes
    /// at most 4096 px per side, so a 6000 px image is scaled to fit.
    #[test]
    fn full_resolution_keeps_to_the_providers_edge_limit() {
        let bytes = jpeg_bytes(6_000, 100);
        let mut full = target(BASELINE, 0);
        full.full_resolution = true;
        full.provider_max_edge = Some(4_096);
        let prepared = prepare_image(&bytes, "image/jpeg", &full).unwrap();
        let (width, height) = prepared.dimensions.unwrap();
        assert!(width.max(height) <= 4_096, "{width}x{height}");
        assert_eq!(prepared.downsampled_from, Some((6_000, 100)));

        // A ceiling raised past the limit is capped by it too.
        let mut raised = target(BASELINE, 0);
        raised.max_edge = 8_000;
        raised.provider_max_edge = Some(4_096);
        let prepared = prepare_image(&bytes, "image/jpeg", &raised).unwrap();
        assert!(prepared.dimensions.unwrap().0 <= 4_096);
    }

    /// An image too large to decode is refused when it is past the provider's
    /// edge limit, rather than sent for the provider to reject.
    #[test]
    fn an_undecodable_giant_past_the_edge_limit_is_refused() {
        let bytes = make_png_header_claiming(10_001, 10_001);
        let mut deepseek = target(BASELINE, 0);
        deepseek.provider_max_edge = Some(4_096);
        let err = prepare_image(&bytes, "image/png", &deepseek).unwrap_err();
        assert!(
            matches!(
                err,
                ImageError::OverEdgeLimit {
                    width: 10_001,
                    limit: 4_096,
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(
            err.describe(std::path::Path::new("big.png"))
                .contains("-resize 4096x4096"),
            "{}",
            err.describe(std::path::Path::new("big.png"))
        );
    }

    /// Transparency becomes white, not black, when JPEG has to be used.
    #[test]
    fn transparency_is_flattened_onto_white() {
        let clear = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            4,
            4,
            image::Rgba([0, 0, 0, 0]),
        ));
        let rgb = flatten_onto_white(&clear);
        assert_eq!(rgb.get_pixel(0, 0).0, [255, 255, 255]);
    }

    /// 10_001 x 10_001 = 100_020_001 pixels, past `MAX_DECODE_PIXELS`: the
    /// header is read, the bitmap is never allocated.
    #[test]
    fn test_decompression_bomb_pixel_count_is_not_decoded() {
        let bytes = make_png_header_claiming(10_001, 10_001);
        assert_eq!(sniff_dimensions(&bytes), Some((10_001, 10_001)));
        let err = prepare_image(&bytes, "image/png", &target(BASELINE, 8)).unwrap_err();
        assert!(
            matches!(err, ImageError::TooLargeToShrink { .. }),
            "{err:?}"
        );
        // Within the budget, an accepted bomb is passed on untouched rather
        // than decoded.
        let passed = prepare_image(&bytes, "image/png", &target(BASELINE, 0)).unwrap();
        assert!(!passed.changed());
    }

    /// Guards the fixture itself: if the decoder cannot see the orientation tag
    /// we spliced in, the orientation tests below would pass vacuously.
    #[test]
    fn exif_fixture_is_readable_by_the_decoder() {
        let landscape = jpeg_bytes(64, 32);
        assert_eq!(sniff_dimensions(&landscape), Some((64, 32)));

        let rotated = with_exif_orientation(&landscape, 6); // Rotate90
        assert_eq!(
            sniff_dimensions(&rotated),
            Some((32, 64)),
            "orientation 6 must transpose the reported dimensions"
        );
    }

    #[test]
    fn swaps_axes_matches_the_transposing_orientations() {
        for o in [
            Orientation::Rotate90,
            Orientation::Rotate270,
            Orientation::Rotate90FlipH,
            Orientation::Rotate270FlipH,
        ] {
            assert!(swaps_axes(o), "{o:?}");
        }
        for o in [
            Orientation::NoTransforms,
            Orientation::Rotate180,
            Orientation::FlipHorizontal,
            Orientation::FlipVertical,
        ] {
            assert!(!swaps_axes(o), "{o:?}");
        }
    }

    /// A portrait photo stored landscape with `Orientation::Rotate90`: the
    /// re-encoded JPEG carries no EXIF, so the rotation must be baked into the
    /// pixels, and the reported size must be the upright one.
    #[test]
    fn exif_rotated_image_is_uprighted_and_measured_upright() {
        let stored_landscape = jpeg_bytes(3_000, 100);
        let rotated = with_exif_orientation(&stored_landscape, 6);

        let prepared = prepare_image(&rotated, "image/jpeg", &target(BASELINE, 3_750_000)).unwrap();
        assert!(prepared.changed(), "3000px edge exceeds the 2000px cap");
        assert_eq!(prepared.dimensions, Some((67, 2_000)));
        assert_eq!(prepared.downsampled_from, Some((100, 3_000)));
    }

    /// The same pixels without the EXIF tag stay landscape. Pairing this with
    /// the test above proves the orientation, not the resize, did the work.
    #[test]
    fn image_without_exif_orientation_keeps_its_stored_axes() {
        let landscape = jpeg_bytes(3_000, 100);
        let prepared =
            prepare_image(&landscape, "image/jpeg", &target(BASELINE, 3_750_000)).unwrap();
        assert_eq!(prepared.dimensions, Some((2_000, 67)));
    }

    /// 2000px keeps image-heavy sessions inside provider request-body limits,
    /// where a larger ceiling would not.
    #[test]
    fn built_in_edge_cap_is_2000_px() {
        assert_eq!(MAX_IMAGE_EDGE_PX, 2_000);
    }

    /// Above the ceiling it is downscaled, and the longest edge lands *on* the
    /// ceiling: rounding the short axis must not drag the scale below it.
    #[test]
    fn images_beyond_the_cap_are_downscaled_onto_it() {
        let bytes = jpeg_bytes(3_400, 40);
        let prepared = prepare_image(&bytes, "image/jpeg", &target(BASELINE, 3_750_000)).unwrap();
        assert_eq!(
            prepared.dimensions.map(|(w, _)| w),
            Some(2_000),
            "extreme aspect ratio must still hit the cap"
        );
    }

    /// `fit_within_edge` scales by exactly `max_edge / longest_edge`, for any
    /// aspect ratio.
    #[test]
    fn fit_within_edge_lands_on_the_cap() {
        // Extreme aspect ratios in both orientations, plus the tightest
        // rounding case (one pixel over the cap on the long edge).
        for (w, h) in [(3_400u32, 40u32), (100, 4_000), (6_001, 17), (3_001, 3_000)] {
            let img = DynamicImage::new_rgb8(w, h);
            let out = fit_within_edge(img, MAX_IMAGE_EDGE_PX);
            assert_eq!(
                out.width().max(out.height()),
                MAX_IMAGE_EDGE_PX,
                "{w}x{h} did not land on the cap"
            );
        }
    }

    /// An image already inside the cap is returned untouched, not re-sampled.
    #[test]
    fn fit_within_edge_leaves_small_images_alone() {
        let img = DynamicImage::new_rgb8(640, 480);
        let out = fit_within_edge(img, MAX_IMAGE_EDGE_PX);
        assert_eq!((out.width(), out.height()), (640, 480));
    }

    /// The env override is resolved live, so a caller can raise or lower the
    /// ceiling without rebuilding. Anything that is not a positive integer is
    /// ignored rather than collapsing the ceiling to zero.
    #[test]
    fn env_override_accepts_only_positive_integers() {
        // The parser is exercised directly: `std::env::set_var` is `unsafe` in
        // edition 2024 and races the other tests in this binary.
        assert_eq!(parse_edge_override(" 3000 "), Some(3_000));
        assert_eq!(parse_edge_override("1"), Some(1));
        for bad in ["0", "", "  ", "-1", "2.5", "abc", "1e3", "2000px"] {
            assert_eq!(parse_edge_override(bad), None, "{bad:?}");
        }
    }

    /// With no override in the environment, the resolved ceiling is the
    /// built-in one.
    #[test]
    fn resolved_ceiling_defaults_to_the_built_in() {
        if std::env::var(MAX_IMAGE_EDGE_ENV).is_ok() {
            return; // the ambient environment owns this value; nothing to assert
        }
        assert_eq!(resolve_max_image_edge_px(), MAX_IMAGE_EDGE_PX);
    }

    /// A payload that will not fit the byte budget at the ceiling steps down
    /// the edge ladder rather than giving up after one try.
    #[test]
    fn oversized_payload_steps_down_the_edge_ladder() {
        // A tiny budget forces the ladder; noise defeats JPEG's quality steps.
        let img = image::RgbImage::from_fn(1_200, 1_200, |x, y| {
            let v = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) as u8;
            image::Rgb([v, v.rotate_left(3), v.rotate_left(5)])
        });
        let image = DynamicImage::ImageRgb8(img);

        let encoded = encode_stepping_down_edges(image, 2_000, &target(BASELINE, 60_000), false)
            .expect("must find a fitting edge");
        assert!(encoded.bytes.len() <= 60_000);
        assert!(encoded.width < 1_200);
    }

    /// Ladder steps at or above the ceiling are skipped: with the built-in
    /// 2000px ceiling the first entry is a no-op, and a ceiling lowered by the
    /// env override must not be re-raised by the ladder.
    #[test]
    fn ladder_only_offers_steps_below_the_ceiling() {
        assert_eq!(FALLBACK_EDGES_PX[0], MAX_IMAGE_EDGE_PX);
        assert!(
            FALLBACK_EDGES_PX.windows(2).all(|w| w[0] > w[1]),
            "the ladder must descend"
        );

        let below = |cap| fallback_edges_below(cap).collect::<Vec<_>>();
        assert_eq!(below(2_000), vec![1_000, 768, 512, 384, 256]);
        assert_eq!(below(3_000), vec![2_000, 1_000, 768, 512, 384, 256]);
        assert_eq!(
            below(512),
            vec![384, 256],
            "a lowered ceiling skips 2000/1000"
        );
        assert_eq!(below(256), Vec::<u32>::new());
    }

    /// And the ladder never widens an image past the ceiling in practice.
    #[test]
    fn ladder_never_raises_the_edge_above_the_ceiling() {
        let image = DynamicImage::new_rgb8(4_000, 4_000);
        let encoded = encode_stepping_down_edges(image, 512, &target(BASELINE, 3_750_000), false)
            .expect("solid image encodes");
        assert!(
            encoded.width.max(encoded.height) <= 512,
            "a 512px ceiling must not be widened by the ladder, got {}x{}",
            encoded.width,
            encoded.height
        );
    }
}
