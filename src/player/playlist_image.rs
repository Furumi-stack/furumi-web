use std::io::Cursor;

use base64::{Engine, engine::general_purpose::STANDARD};
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, codecs::jpeg::JpegEncoder,
    imageops::FilterType,
};
use sha2::{Digest, Sha256};

const MAX_UPLOAD: usize = 4 * 1024 * 1024;
const MAX_DECODED: u64 = 64 * 1024 * 1024;
const INVALID: &str = "Choose a JPEG, PNG or WebP image up to 4 MiB";
const TOO_LARGE: &str = "The image dimensions are too large";

pub(super) async fn decode(encoded: String) -> Result<(Vec<u8>, String), String> {
    if encoded.len() > MAX_UPLOAD.div_ceil(3) * 4 {
        return Err(INVALID.into());
    }
    // Bound concurrent full-resolution decodes independently of the async worker pool.
    static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let slot = SLOTS.acquire().await.map_err(|_| INVALID.to_owned())?;
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        let raw = STANDARD.decode(encoded).map_err(|_| INVALID)?;
        if raw.len() > MAX_UPLOAD {
            return Err(INVALID);
        }
        canonical_cover(&raw)
    })
    .await
    .map_err(|_| "Could not process the image".to_owned())?
    .map_err(str::to_owned)
}

fn canonical_cover(raw: &[u8]) -> Result<(Vec<u8>, String), &'static str> {
    let format = image::guess_format(raw).map_err(|_| INVALID)?;
    if !matches!(
        format,
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP
    ) {
        return Err(INVALID);
    }
    let mut reader = ImageReader::with_format(Cursor::new(raw), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(MAX_DECODED);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|_| INVALID)?;
    if decoder.total_bytes() > MAX_DECODED {
        return Err(TOO_LARGE);
    }
    let orientation = decoder.orientation().map_err(|_| INVALID)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| INVALID)?;
    image.apply_orientation(orientation);
    let edge = 512.min(image.width()).min(image.height());
    if edge == 0 {
        return Err(INVALID);
    }
    let side = image.width().min(image.height());
    let square = image.crop_imm(
        (image.width() - side) / 2,
        (image.height() - side) / 2,
        side,
        side,
    );
    let thumb = square
        .resize_exact(edge, edge, FilterType::Triangle)
        .to_rgba8();
    // Match the dark artwork tile underneath transparent areas of the preview.
    let mut rgb = image::RgbImage::new(edge, edge);
    for (out, pixel) in rgb.pixels_mut().zip(thumb.pixels()) {
        let alpha = u16::from(pixel[3]);
        for (channel, &value) in out.0.iter_mut().zip(pixel.0.iter()) {
            *channel = ((u16::from(value) * alpha + 32 * (255 - alpha)) / 255) as u8;
        }
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 85)
        .encode_image(&rgb)
        .map_err(|_| INVALID)?;
    let hash = format!("{:x}", Sha256::digest(&jpeg));
    Ok((jpeg, hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn uploads_are_validated_and_saved_as_square_jpegs() {
        let mut input = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(10, 6)
            .write_to(&mut input, ImageFormat::Png)
            .unwrap();
        let (jpeg, hash) = decode(STANDARD.encode(input.into_inner())).await.unwrap();
        let cover = image::load_from_memory_with_format(&jpeg, ImageFormat::Jpeg).unwrap();
        assert_eq!((cover.width(), cover.height()), (6, 6));
        assert_eq!(hash, format!("{:x}", Sha256::digest(&jpeg)));
        for invalid in [
            "invalid base64!".to_owned(),
            STANDARD.encode(b"<svg/>"),
            "A".repeat(MAX_UPLOAD.div_ceil(3) * 4 + 1),
        ] {
            assert!(decode(invalid).await.is_err());
        }
    }
}
