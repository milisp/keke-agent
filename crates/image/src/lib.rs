//! One preparation path for tool images and user attachments, with bounded reads
//! and decoding outside the async executor.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageFormat, ImageReader, imageops::FilterType};
use keke_config_types::ImageLimits;
use keke_protocol::ImageBlock;
use std::{io::Cursor, path::Path};
use tokio::io::AsyncReadExt;

/// The validated bytes actually sent to a model, rather than the source file.
#[derive(Debug)]
pub struct PreparedImage {
    pub image: ImageBlock,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

/// Preparation failures are reported before invalid data can enter history.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("{0}")]
    InvalidLimits(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("not a regular file")]
    NotFile,
    #[error("source image exceeds the {0} byte read limit")]
    ReadLimit(u64),
    #[error("not a supported PNG, JPEG, GIF, or WebP image")]
    Unsupported,
    #[error("image base64 is invalid: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("image is invalid: {0}")]
    Decode(#[from] image::ImageError),
    #[error("image dimensions {width}x{height} exceed the {limit} pixel decode limit")]
    PixelLimit { width: u32, height: u32, limit: u64 },
    #[error("image cannot fit the {0} byte encoded limit")]
    EncodedLimit(u64),
    #[error("image preparation worker failed: {0}")]
    Worker(String),
}

impl ImageError {
    /// Stable error codes shared by attachment and tool callers.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(error) if error.kind() == std::io::ErrorKind::NotFound => "file_not_found",
            Self::Io(_) | Self::NotFile => "read_failed",
            Self::ReadLimit(_) | Self::PixelLimit { .. } | Self::EncodedLimit(_) => {
                "image_too_large"
            }
            Self::Unsupported => "unsupported_image",
            Self::Decode(_) | Self::Base64(_) => "invalid_image",
            Self::InvalidLimits(_) | Self::Worker(_) => "image_failed",
        }
    }
}

/// Load a regular file with bounded memory, then validate and prepare its pixels.
pub async fn load_path(path: &Path, limits: ImageLimits) -> Result<PreparedImage, ImageError> {
    limits.check().map_err(ImageError::InvalidLimits)?;
    // Check before opening so a named pipe cannot block a dropped-file operation.
    if !tokio::fs::metadata(path).await?.is_file() {
        return Err(ImageError::NotFile);
    }
    let file = tokio::fs::File::open(path).await?;
    if !file.metadata().await?.is_file() {
        return Err(ImageError::NotFile);
    }
    let mut bytes = Vec::new();
    file.take(limits.read_bytes + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > limits.read_bytes {
        return Err(ImageError::ReadLimit(limits.read_bytes));
    }
    tokio::task::spawn_blocking(move || prepare(bytes, limits))
        .await
        .map_err(|error| ImageError::Worker(error.to_string()))?
}

/// Prepare an inline user attachment; its declared media type is not trusted.
pub async fn prepare_inline(
    image: ImageBlock,
    limits: ImageLimits,
) -> Result<PreparedImage, ImageError> {
    limits.check().map_err(ImageError::InvalidLimits)?;
    // Bound the encoded string before decoding can allocate its output buffer.
    let max_base64 = limits.read_bytes.div_ceil(3) * 4;
    if image.data.len() as u64 > max_base64 {
        return Err(ImageError::ReadLimit(limits.read_bytes));
    }
    tokio::task::spawn_blocking(move || {
        let bytes = STANDARD.decode(image.data)?;
        if bytes.len() as u64 > limits.read_bytes {
            return Err(ImageError::ReadLimit(limits.read_bytes));
        }
        prepare(bytes, limits)
    })
    .await
    .map_err(|error| ImageError::Worker(error.to_string()))?
}

fn prepare(bytes: Vec<u8>, limits: ImageLimits) -> Result<PreparedImage, ImageError> {
    let format = image::guess_format(&bytes).map_err(|_| ImageError::Unsupported)?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
    ) {
        return Err(ImageError::Unsupported);
    }
    let (width, height) =
        ImageReader::with_format(Cursor::new(&bytes), format).into_dimensions()?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > limits.decoded_pixels {
        return Err(ImageError::PixelLimit {
            width,
            height,
            limit: limits.decoded_pixels,
        });
    }
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut decoder_limits = image::Limits::default();
    decoder_limits.max_alloc = Some(limits.decoded_pixels * 16);
    reader.limits(decoder_limits);
    let mut decoded = reader.decode()?;
    if width <= limits.max_dimension
        && height <= limits.max_dimension
        && bytes.len() as u64 <= limits.encoded_bytes
        && matches!(format, ImageFormat::Png | ImageFormat::Jpeg)
    {
        return Ok(finish(bytes, format, width, height));
    }
    if width > limits.max_dimension || height > limits.max_dimension {
        decoded = decoded.resize(
            limits.max_dimension,
            limits.max_dimension,
            FilterType::Lanczos3,
        );
    }
    // Screenshots keep sharp text and transparency whenever lossless output fits.
    if format == ImageFormat::Png || decoded.color().has_alpha() {
        let mut png = Cursor::new(Vec::new());
        decoded.write_to(&mut png, ImageFormat::Png)?;
        if png.get_ref().len() as u64 <= limits.encoded_bytes {
            return Ok(finish(
                png.into_inner(),
                ImageFormat::Png,
                decoded.width(),
                decoded.height(),
            ));
        }
    }
    loop {
        let rgb = flatten(&decoded);
        for quality in [90, 75, 60] {
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality)
                .encode_image(&rgb)?;
            if jpeg.len() as u64 <= limits.encoded_bytes {
                return Ok(finish(
                    jpeg,
                    ImageFormat::Jpeg,
                    decoded.width(),
                    decoded.height(),
                ));
            }
        }
        if decoded.width() == 1 && decoded.height() == 1 {
            return Err(ImageError::EncodedLimit(limits.encoded_bytes));
        }
        decoded = decoded.resize(
            (decoded.width() * 3 / 4).max(1),
            (decoded.height() * 3 / 4).max(1),
            FilterType::Lanczos3,
        );
    }
}

fn flatten(decoded: &DynamicImage) -> DynamicImage {
    let rgba = decoded.to_rgba8();
    let rgb = image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba.get_pixel(x, y).0;
        let alpha = u16::from(p[3]);
        image::Rgb([0, 1, 2].map(|c| ((u16::from(p[c]) * alpha + 255 * (255 - alpha)) / 255) as u8))
    });
    DynamicImage::ImageRgb8(rgb)
}

fn finish(bytes: Vec<u8>, format: ImageFormat, width: u32, height: u32) -> PreparedImage {
    PreparedImage {
        image: ImageBlock {
            data: STANDARD.encode(&bytes),
            media_type: if format == ImageFormat::Png {
                "image/png"
            } else {
                "image/jpeg"
            }
            .into(),
        },
        width,
        height,
        bytes: bytes.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(width, height)
            .write_to(&mut out, ImageFormat::Png)
            .expect("encode");
        out.into_inner()
    }

    #[tokio::test]
    async fn inline_images_are_validated_and_their_mime_is_detected() {
        let source = png(2, 2);
        let prepared = prepare_inline(
            ImageBlock {
                data: STANDARD.encode(&source),
                media_type: "image/jpeg".into(),
            },
            ImageLimits::default(),
        )
        .await
        .expect("prepare");
        assert_eq!(prepared.image.media_type, "image/png");
        assert_eq!(
            STANDARD.decode(prepared.image.data).expect("base64"),
            source
        );
        let error = prepare_inline(
            ImageBlock {
                data: "!invalid!".into(),
                media_type: "image/png".into(),
            },
            ImageLimits::default(),
        )
        .await
        .expect_err("invalid base64");
        assert!(matches!(error, ImageError::Base64(_)));
    }

    #[tokio::test]
    async fn inline_base64_cannot_allocate_beyond_the_source_budget() {
        let limits = ImageLimits {
            read_bytes: 1024,
            ..ImageLimits::default()
        };
        for data in ["!".repeat(1369), STANDARD.encode(vec![0; 1025])] {
            let result = prepare_inline(
                ImageBlock {
                    data,
                    media_type: "image/png".into(),
                },
                limits,
            )
            .await;
            assert!(matches!(result, Err(ImageError::ReadLimit(1024))));
        }
    }

    #[tokio::test]
    async fn valid_small_png_keeps_its_original_lossless_bytes() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("image.wrong-extension");
        let bytes = png(20, 10);
        std::fs::write(&path, &bytes).expect("write");
        let image = load_path(&path, ImageLimits::default())
            .await
            .expect("prepare");
        assert_eq!((image.width, image.height), (20, 10));
        assert_eq!(STANDARD.decode(image.image.data).expect("base64"), bytes);
    }

    #[test]
    fn corrupt_pixels_are_rejected_despite_a_valid_signature() {
        assert!(prepare(b"\x89PNG\r\n\x1a\n".to_vec(), ImageLimits::default()).is_err());
        let mut broken = png(20, 10);
        broken.truncate(broken.len() / 2);
        assert!(prepare(broken, ImageLimits::default()).is_err());
    }

    #[test]
    fn pixel_bombs_are_refused_before_decode() {
        let error = prepare(
            png(20, 10),
            ImageLimits {
                decoded_pixels: 100,
                ..ImageLimits::default()
            },
        )
        .expect_err("pixel limit");
        assert!(matches!(error, ImageError::PixelLimit { .. }));
    }

    #[test]
    fn large_images_are_resized_and_remain_decodable() {
        let output = prepare(
            png(100, 40),
            ImageLimits {
                max_dimension: 32,
                ..ImageLimits::default()
            },
        )
        .expect("prepare");
        assert_eq!((output.width, output.height), (32, 13));
        let bytes = STANDARD.decode(output.image.data).expect("base64");
        assert!(image::load_from_memory(&bytes).is_ok());
    }

    #[test]
    fn noisy_images_fit_the_encoded_budget_after_lossless_fallback() {
        let mut seed = 17_u32;
        let rgb = image::RgbImage::from_fn(128, 128, |_, _| {
            image::Rgb([0, 1, 2].map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 24) as u8
            }))
        });
        let mut input = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(rgb)
            .write_to(&mut input, ImageFormat::Png)
            .expect("encode");
        let output = prepare(
            input.into_inner(),
            ImageLimits {
                encoded_bytes: 1024,
                ..ImageLimits::default()
            },
        )
        .expect("prepare");
        assert!(output.bytes <= 1024);
        assert_eq!(output.image.media_type, "image/jpeg");
        assert!(
            image::load_from_memory(&STANDARD.decode(output.image.data).expect("base64")).is_ok()
        );
    }

    #[tokio::test]
    async fn file_reads_are_bounded_and_directories_are_rejected() {
        let dir = tempfile::tempdir().expect("directory");
        assert!(matches!(
            load_path(dir.path(), ImageLimits::default()).await,
            Err(ImageError::NotFile)
        ));
        let path = dir.path().join("large.png");
        std::fs::write(&path, vec![0; 1025]).expect("write");
        let result = load_path(
            &path,
            ImageLimits {
                read_bytes: 1024,
                ..ImageLimits::default()
            },
        )
        .await;
        assert!(matches!(result, Err(ImageError::ReadLimit(1024))));
    }
}
