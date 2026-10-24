//! Optional, bounded preprocessing of inline Responses input images.
//!
//! This is a lossy size reduction policy. Callers must opt in and should run it
//! on a blocking worker. Failures leave the original image untouched.

use std::io::Cursor;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{
    DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Limits, Rgb, RgbImage,
    codecs::jpeg::JpegEncoder, imageops::FilterType,
};
use serde::Serialize;
use serde_json::Value;

const MIN_IMAGE_BYTES: usize = 512 * 1024;
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_BASE64_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
const MAX_DIMENSION: u32 = 8192;
const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_REQUEST_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_REQUEST_BASE64_BYTES: usize = 64 * 1024 * 1024;
const MAX_DECODER_ALLOCATION: u64 = 96 * 1024 * 1024;
const LONG_EDGE: u32 = 2048;
const JPEG_QUALITY: u8 = 90;
const JPEG_DATA_URL_PREFIX: &str = "data:image/jpeg;base64,";

/// Payload sizes include the data URL prefix and base64 encoding, and only
/// cover images actually replaced. `skipped` counts input images left intact.
/// No image contents, URLs, or request text are retained in this summary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CompressionSummary {
    pub count: usize,
    pub before_bytes: usize,
    pub after_bytes: usize,
    pub skipped: usize,
}

struct Budget {
    pixels: u64,
    base64_bytes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            pixels: MAX_REQUEST_PIXELS,
            base64_bytes: MAX_REQUEST_BASE64_BYTES,
        }
    }
}

/// Compress eligible `input[].content[]` items of type `input_image`.
///
/// Only inline `image/png` and `image/jpeg` base64 data URLs are accepted.
/// Small images, animations, oversized images, unsupported formats, and
/// malformed inputs remain unchanged. Encoded replacements must save at least
/// 10%. Images are never enlarged; resizing uses Lanczos3 and JPEG quality 90.
pub fn compress_inline_images(body: &mut Value) -> CompressionSummary {
    let mut summary = CompressionSummary::default();
    let mut budget = Budget::default();
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return summary;
    };
    for message in input {
        // Do not descend into reasoning, compaction, tool calls, or arbitrary
        // JSON. Responses messages may omit their `type` discriminator.
        if message.get("type").is_some_and(|kind| kind != "message") {
            continue;
        }
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for item in content {
            if item.get("type").and_then(Value::as_str) != Some("input_image") {
                continue;
            }
            let Some(url) = item.get("image_url").and_then(Value::as_str) else {
                summary.skipped += 1;
                continue;
            };
            let Some(replacement) = compress_data_url(url, &mut budget) else {
                summary.skipped += 1;
                continue;
            };
            summary.count += 1;
            summary.before_bytes += url.len();
            summary.after_bytes += replacement.len();
            item["image_url"] = Value::String(replacement);
        }
    }
    summary
}

fn compress_data_url(url: &str, budget: &mut Budget) -> Option<String> {
    let (encoded, format) = if let Some(data) = url.strip_prefix("data:image/png;base64,") {
        (data, ImageFormat::Png)
    } else {
        (url.strip_prefix(JPEG_DATA_URL_PREFIX)?, ImageFormat::Jpeg)
    };
    // Check the encoded size before allocating a decoded input buffer.
    if encoded.len() < MIN_IMAGE_BYTES.div_ceil(3) * 4
        || encoded.len() > MAX_BASE64_BYTES
        || encoded.len() > budget.base64_bytes
    {
        return None;
    }
    budget.base64_bytes -= encoded.len();
    let bytes = STANDARD.decode(encoded).ok()?;
    if !(MIN_IMAGE_BYTES..=MAX_IMAGE_BYTES).contains(&bytes.len()) {
        return None;
    }
    let compressed = recompress(&bytes, format, budget)?;
    let replacement_length = JPEG_DATA_URL_PREFIX.len() + compressed.len().div_ceil(3) * 4;
    if replacement_length * 10 > url.len() * 9 {
        return None;
    }
    let mut replacement = String::with_capacity(replacement_length);
    replacement.push_str(JPEG_DATA_URL_PREFIX);
    STANDARD.encode_string(compressed, &mut replacement);
    Some(replacement)
}

fn recompress(bytes: &[u8], format: ImageFormat, budget: &mut Budget) -> Option<Vec<u8>> {
    // Converting an animated PNG to JPEG would discard frames.
    if format == ImageFormat::Png && is_animated_png(bytes) {
        return None;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODER_ALLOCATION);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || pixels > MAX_IMAGE_PIXELS
        || pixels > budget.pixels
    {
        return None;
    }
    // Charge attempted decodes too, so corrupt inputs cannot evade the budget.
    budget.pixels -= pixels;
    let orientation = decoder.orientation().ok()?;
    let mut decoded = DynamicImage::from_decoder(decoder).ok()?;
    decoded.apply_orientation(orientation);
    // Composite before filtering, so transparent RGB values do not create
    // dark halos or a black background when the alpha channel is discarded.
    let rgb = composite_on_white(decoded);
    let mut resized = DynamicImage::ImageRgb8(rgb);
    if resized.width().max(resized.height()) > LONG_EDGE {
        resized = resized.resize(LONG_EDGE, LONG_EDGE, FilterType::Lanczos3);
    }
    let mut output = Vec::new();
    JpegEncoder::new_with_quality(&mut output, JPEG_QUALITY)
        .encode_image(&resized)
        .ok()?;
    Some(output)
}

fn composite_on_white(decoded: DynamicImage) -> RgbImage {
    if !decoded.color().has_alpha() {
        return decoded.into_rgb8();
    }
    let (width, height) = decoded.dimensions();
    let mut rgb = RgbImage::new(width, height);
    for (x, y, pixel) in decoded.pixels() {
        let alpha = u32::from(pixel[3]);
        let over_white =
            |channel: u8| ((u32::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        rgb.put_pixel(
            x,
            y,
            Rgb([
                over_white(pixel[0]),
                over_white(pixel[1]),
                over_white(pixel[2]),
            ]),
        );
    }
    rgb
}

fn is_animated_png(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return false;
    }
    let mut offset = 8usize;
    while let Some(header) = bytes.get(offset..offset.saturating_add(8)) {
        let length = u32::from_be_bytes(header[..4].try_into().unwrap_or_default()) as usize;
        if &header[4..8] == b"acTL" {
            return true;
        }
        if &header[4..8] == b"IDAT" || &header[4..8] == b"IEND" {
            return false;
        }
        let Some(next) = offset.checked_add(length).and_then(|n| n.checked_add(12)) else {
            return false;
        };
        if next > bytes.len() {
            return false;
        }
        offset = next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use serde_json::json;

    fn png_url(image: &DynamicImage) -> String {
        let mut output = Cursor::new(Vec::new());
        image.write_to(&mut output, ImageFormat::Png).unwrap();
        format!(
            "data:image/png;base64,{}",
            STANDARD.encode(output.into_inner())
        )
    }

    fn request(url: String) -> Value {
        json!({"input": [{"role": "user", "content": [
            {"type": "input_text", "text": "Read this image."},
            {"type": "input_image", "image_url": url, "detail": "high"}
        ]}]})
    }

    fn noise(width: u32, height: u32, alpha: u8) -> DynamicImage {
        let mut state = 17u32;
        DynamicImage::ImageRgba8(RgbaImage::from_fn(width, height, |_, _| {
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            };
            Rgba([next(), next(), next(), alpha])
        }))
    }

    #[test]
    fn small_images_and_external_urls_are_unchanged() {
        for url in [
            png_url(&DynamicImage::new_rgb8(32, 32)),
            "https://example.invalid/image.png".into(),
            "data:image/gif;base64,AAAA".into(),
        ] {
            let mut body = request(url);
            let original = body.clone();
            let summary = compress_inline_images(&mut body);
            assert_eq!(body, original);
            assert_eq!(
                summary,
                CompressionSummary {
                    skipped: 1,
                    ..Default::default()
                }
            );
        }
    }

    #[test]
    fn large_images_are_smaller_and_keep_other_fields() {
        let mut body = request(png_url(&noise(2304, 1024, 255)));
        let summary = compress_inline_images(&mut body);
        assert_eq!(summary.count, 1);
        assert_eq!(summary.skipped, 0);
        assert!(summary.after_bytes * 10 <= summary.before_bytes * 9);
        let item = &body["input"][0]["content"][1];
        assert_eq!(item["detail"], "high");
        assert_eq!(body["input"][0]["content"][0]["text"], "Read this image.");
        let bytes = STANDARD
            .decode(
                item["image_url"]
                    .as_str()
                    .unwrap()
                    .strip_prefix(JPEG_DATA_URL_PREFIX)
                    .unwrap(),
            )
            .unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.width(), LONG_EDGE);
        assert!(decoded.height() <= LONG_EDGE);
    }

    #[test]
    fn transparent_images_have_white_backgrounds() {
        let mut body = request(png_url(&noise(1024, 1024, 0)));
        assert_eq!(compress_inline_images(&mut body).count, 1);
        let url = body["input"][0]["content"][1]["image_url"]
            .as_str()
            .unwrap();
        let bytes = STANDARD
            .decode(url.strip_prefix(JPEG_DATA_URL_PREFIX).unwrap())
            .unwrap();
        let image = image::load_from_memory(&bytes).unwrap().into_rgb8();
        assert!(
            image
                .pixels()
                .all(|pixel| pixel.0.iter().all(|channel| *channel >= 254))
        );

        let semi = DynamicImage::ImageRgba8(RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 128])));
        assert_eq!(composite_on_white(semi).get_pixel(0, 0).0, [127, 127, 127]);
    }

    #[test]
    fn already_compressed_jpeg_is_kept_when_savings_are_insufficient() {
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 80)
            .encode_image(&noise(1024, 1024, 255).into_rgb8())
            .unwrap();
        assert!(bytes.len() >= MIN_IMAGE_BYTES);
        let mut body = request(format!("{JPEG_DATA_URL_PREFIX}{}", STANDARD.encode(bytes)));
        let original = body.clone();
        assert_eq!(compress_inline_images(&mut body).count, 0);
        assert_eq!(body, original);
    }

    #[test]
    fn jpeg_orientation_is_applied_before_metadata_is_removed() {
        let mut original = Vec::new();
        JpegEncoder::new_with_quality(&mut original, 90)
            .encode_image(&DynamicImage::new_rgb8(2, 3))
            .unwrap();
        let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe1];
        bytes.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        bytes.extend_from_slice(exif);
        bytes.extend_from_slice(&original[2..]);
        let output = recompress(&bytes, ImageFormat::Jpeg, &mut Budget::default()).unwrap();
        assert_eq!(
            image::load_from_memory(&output).unwrap().dimensions(),
            (3, 2)
        );
    }

    #[test]
    fn malformed_images_and_encrypted_items_are_unchanged() {
        for data in [
            "!".repeat(MIN_IMAGE_BYTES * 2),
            STANDARD.encode(vec![0; MIN_IMAGE_BYTES]),
        ] {
            let mut body = request(format!("data:image/png;base64,{data}"));
            body["input"].as_array_mut().unwrap().push(json!({
                "type": "reasoning", "encrypted_content": "sensitive-opaque-data",
                "content": [{"type": "input_image", "image_url": "data:image/png;base64,AAAA"}]
            }));
            let original = body.clone();
            assert_eq!(compress_inline_images(&mut body).skipped, 1);
            assert_eq!(body, original);
        }
    }

    #[test]
    fn dimensions_and_request_pixel_budget_are_guarded() {
        let image = DynamicImage::new_rgb8(MAX_DIMENSION + 1, 1);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        assert!(recompress(bytes.get_ref(), ImageFormat::Png, &mut Budget::default()).is_none());

        let image = DynamicImage::new_rgb8(32, 32);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let mut budget = Budget {
            pixels: 10,
            ..Default::default()
        };
        assert!(recompress(bytes.get_ref(), ImageFormat::Png, &mut budget).is_none());
    }

    #[test]
    fn invalid_inputs_consume_the_bounded_base64_budget() {
        let encoded = "!".repeat(MIN_IMAGE_BYTES.div_ceil(3) * 4);
        let url = format!("data:image/png;base64,{encoded}");
        let mut budget = Budget {
            base64_bytes: encoded.len(),
            ..Default::default()
        };
        assert!(compress_data_url(&url, &mut budget).is_none());
        assert_eq!(budget.base64_bytes, 0);
        assert!(compress_data_url(&url, &mut budget).is_none());
        assert_eq!(budget.base64_bytes, 0);

        let mut budget = Budget::default();
        let oversized = format!("data:image/png;base64,{}", "A".repeat(MAX_BASE64_BYTES + 1));
        assert!(compress_data_url(&oversized, &mut budget).is_none());
        assert_eq!(budget.base64_bytes, MAX_REQUEST_BASE64_BYTES);
    }

    #[test]
    fn multiple_images_share_pixel_budget_without_changing_skipped_items() {
        // Exercise the shared budget directly with small deterministic images.
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(32, 32)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        let mut budget = Budget {
            pixels: 32 * 32,
            ..Default::default()
        };
        assert!(recompress(bytes.get_ref(), ImageFormat::Png, &mut budget).is_some());
        assert_eq!(budget.pixels, 0);
        assert!(recompress(bytes.get_ref(), ImageFormat::Png, &mut budget).is_none());
        assert_eq!(budget.pixels, 0);
    }

    #[test]
    fn animation_control_chunk_is_not_flattened() {
        let bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x08acTL\x00\x00\x00\x02\x00\x00\x00\x00";
        assert!(is_animated_png(bytes));
        assert!(recompress(bytes, ImageFormat::Png, &mut Budget::default()).is_none());
    }
}
