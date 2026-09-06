//! The image work around the model, matching rembg's `DisSession`:
//!
//! 1. decode, drop alpha (`convert("RGB")`)
//! 2. Lanczos to 1024 by 1024, `/255`, `(x - 0.5) / 1.0`, NCHW float32
//! 3. run the model, take the first output's single channel
//! 4. min-max the map to 0..1, `* 255`, uint8 by truncation
//! 5. Lanczos the mask back to the original size, use it as alpha, PNG
//!
//! The resizes go through our own port of Pillow's resampler
//! (`resample.rs`), so the mask lands where the Python service's did, to the
//! bit.

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder, RgbImage, RgbaImage};

use crate::model::Model;
use crate::resample::resize_lanczos;

/// The model's input side.
pub const SIDE: u32 = 1024;

/// Where a removal went wrong.
#[derive(Debug)]
pub enum CutoutError {
    /// The bytes were not a picture this service decodes.
    Decode(String),
    /// A resize could not be set up.
    Resize(String),
    /// The model failed to load or to run.
    Model(String),
    /// The PNG could not be written.
    Encode(String),
}

impl std::fmt::Display for CutoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CutoutError::Decode(e) => write!(f, "decode: {e}"),
            CutoutError::Resize(e) => write!(f, "resize: {e}"),
            CutoutError::Model(e) => write!(f, "model: {e}"),
            CutoutError::Encode(e) => write!(f, "encode: {e}"),
        }
    }
}

impl std::error::Error for CutoutError {}

/// Decode to RGB the way PIL's `convert("RGB")` does: alpha dropped, no
/// orientation applied. JPEGs go through libjpeg-turbo (the decoder Pillow
/// ships), with its defaults (slow-integer DCT, fancy upsampling), so the
/// pixels are the ones the Python service saw.
pub fn decode(bytes: &[u8]) -> Result<RgbImage, CutoutError> {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return decode_jpeg(bytes);
    }
    image::load_from_memory(bytes)
        .map(|img| img.to_rgb8())
        .map_err(|e| CutoutError::Decode(e.to_string()))
}

fn decode_jpeg(bytes: &[u8]) -> Result<RgbImage, CutoutError> {
    let decompress =
        mozjpeg::Decompress::new_mem(bytes).map_err(|e| CutoutError::Decode(e.to_string()))?;
    let mut image = decompress
        .rgb()
        .map_err(|e| CutoutError::Decode(e.to_string()))?;
    let (w, h) = (image.width() as u32, image.height() as u32);
    let pixels: Vec<[u8; 3]> = image
        .read_scanlines()
        .map_err(|e| CutoutError::Decode(e.to_string()))?;
    image
        .finish()
        .map_err(|e| CutoutError::Decode(e.to_string()))?;
    let raw: Vec<u8> = pixels.into_iter().flatten().collect();
    RgbImage::from_raw(w, h, raw).ok_or_else(|| CutoutError::Decode("jpeg buffer size".into()))
}

/// The RGB picture at the model's size.
pub fn to_model_size(rgb: &RgbImage) -> Result<RgbImage, CutoutError> {
    let data = resize_lanczos(
        rgb.as_raw(),
        rgb.width() as usize,
        rgb.height() as usize,
        3,
        SIDE as usize,
        SIDE as usize,
    );
    RgbImage::from_raw(SIDE, SIDE, data)
        .ok_or_else(|| CutoutError::Resize("bad buffer size".into()))
}

/// NCHW float32 with rembg's normalisation for isnet: `/255`, mean 0.5, std 1.
pub fn tensor_of(rgb: &RgbImage) -> Vec<f32> {
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let raw = rgb.as_raw();
    let plane = w * h;
    let mut out = vec![0f32; 3 * plane];
    for i in 0..plane {
        for c in 0..3 {
            out[c * plane + i] = raw[i * 3 + c] as f32 / 255.0 - 0.5;
        }
    }
    out
}

/// rembg's post: rescale the map to 0..1 by its own min and max, then uint8
/// the way numpy's `astype("uint8")` does it, by truncation.
pub fn mask_of(plane: &[f32]) -> Vec<u8> {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for &p in plane {
        lo = lo.min(p);
        hi = hi.max(p);
    }
    let d = (hi - lo).max(1e-6);
    plane
        .iter()
        .map(|&p| ((p - lo) / d * 255.0) as u8)
        .collect()
}

/// The mask back at the picture's size.
pub fn mask_to_size(mask: &[u8], w: u32, h: u32) -> Result<Vec<u8>, CutoutError> {
    if mask.len() != (SIDE * SIDE) as usize {
        return Err(CutoutError::Resize("mask is not the model's size".into()));
    }
    Ok(resize_lanczos(
        mask,
        SIDE as usize,
        SIDE as usize,
        1,
        w as usize,
        h as usize,
    ))
}

/// The original RGB with the mask as alpha, as a PNG. `fast` trades about a
/// quarter more bytes for a much quicker encode.
pub fn compose_png(rgb: &RgbImage, alpha: &[u8], fast: bool) -> Result<Vec<u8>, CutoutError> {
    let (w, h) = (rgb.width(), rgb.height());
    let raw = rgb.as_raw();
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        px[0] = raw[i * 3];
        px[1] = raw[i * 3 + 1];
        px[2] = raw[i * 3 + 2];
        px[3] = alpha[i];
    }
    let img = RgbaImage::from_raw(w, h, rgba)
        .ok_or_else(|| CutoutError::Encode("bad buffer size".into()))?;
    let mut out = Vec::with_capacity((w * h) as usize);
    let compression = if fast {
        CompressionType::Fast
    } else {
        CompressionType::Default
    };
    PngEncoder::new_with_quality(&mut out, compression, FilterType::Adaptive)
        .write_image(img.as_raw(), w, h, ExtendedColorType::Rgba8)
        .map_err(|e| CutoutError::Encode(e.to_string()))?;
    Ok(out)
}

/// What to hand back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// The picture with the background transparent, as a PNG.
    Png,
    /// The same as a lossless WebP: usually a third the size of the PNG.
    Webp,
    /// Only the mask, as an 8-bit greyscale PNG, for pipelines that composite themselves.
    MaskPng,
}

impl Output {
    /// The response's content type.
    pub fn content_type(self) -> &'static str {
        match self {
            Output::Png | Output::MaskPng => "image/png",
            Output::Webp => "image/webp",
        }
    }
}

/// The original RGB with the mask as alpha, as a lossless WebP.
pub fn compose_webp(rgb: &RgbImage, alpha: &[u8]) -> Result<Vec<u8>, CutoutError> {
    let (w, h) = (rgb.width(), rgb.height());
    let raw = rgb.as_raw();
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        px[0] = raw[i * 3];
        px[1] = raw[i * 3 + 1];
        px[2] = raw[i * 3 + 2];
        px[3] = alpha[i];
    }
    let mut out = Vec::with_capacity((w * h) as usize);
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .write_image(&rgba, w, h, ExtendedColorType::Rgba8)
        .map_err(|e| CutoutError::Encode(e.to_string()))?;
    Ok(out)
}

/// The mask alone, as an 8-bit greyscale PNG at the picture's size.
pub fn mask_png(alpha: &[u8], w: u32, h: u32, fast: bool) -> Result<Vec<u8>, CutoutError> {
    let mut out = Vec::with_capacity(alpha.len() / 4);
    let compression = if fast {
        CompressionType::Fast
    } else {
        CompressionType::Default
    };
    PngEncoder::new_with_quality(&mut out, compression, FilterType::Adaptive)
        .write_image(alpha, w, h, ExtendedColorType::L8)
        .map_err(|e| CutoutError::Encode(e.to_string()))?;
    Ok(out)
}

/// The whole job: bytes of a photo in, the chosen output out.
pub fn cutout_as(
    bytes: &[u8],
    model: &Model,
    png_fast: bool,
    output: Output,
) -> Result<Vec<u8>, CutoutError> {
    let rgb = decode(bytes)?;
    let small = to_model_size(&rgb)?;
    let plane = model.infer(tensor_of(&small)).map_err(CutoutError::Model)?;
    let mask = mask_of(&plane);
    let alpha = mask_to_size(&mask, rgb.width(), rgb.height())?;
    match output {
        Output::Png => compose_png(&rgb, &alpha, png_fast),
        Output::Webp => compose_webp(&rgb, &alpha),
        Output::MaskPng => mask_png(&alpha, rgb.width(), rgb.height(), png_fast),
    }
}

/// Bytes of a photo in, a PNG with alpha out.
pub fn cutout(bytes: &[u8], model: &Model, png_fast: bool) -> Result<Vec<u8>, CutoutError> {
    cutout_as(bytes, model, png_fast, Output::Png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_truncates_like_numpy() {
        // 0.5 of the range is 127.5, which astype("uint8") makes 127, not 128.
        assert_eq!(mask_of(&[0.0, 0.5, 1.0]), vec![0, 127, 255]);
        assert_eq!(mask_of(&[-2.0, 0.0, 2.0]), vec![0, 127, 255]);
    }

    #[test]
    fn flat_map_is_all_zero() {
        // A constant map has no range; the epsilon keeps it finite and at zero.
        assert_eq!(mask_of(&[0.3, 0.3, 0.3]), vec![0, 0, 0]);
    }

    #[test]
    fn mask_saturates_at_the_ends() {
        let m = mask_of(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(m[0], 0);
        assert_eq!(m[3], 255);
    }

    #[test]
    fn tensor_is_planar_and_normalised() {
        let mut img = RgbImage::new(2, 1);
        img.put_pixel(0, 0, image::Rgb([255, 0, 128]));
        img.put_pixel(1, 0, image::Rgb([0, 255, 0]));
        let t = tensor_of(&img);
        // Red plane, then green, then blue; each pixel is v/255 - 0.5.
        assert_eq!(t.len(), 6);
        assert!((t[0] - 0.5).abs() < 1e-6);
        assert!((t[1] + 0.5).abs() < 1e-6);
        assert!((t[2] + 0.5).abs() < 1e-6);
        assert!((t[3] - 0.5).abs() < 1e-6);
        assert!((t[4] - (128.0 / 255.0 - 0.5)).abs() < 1e-6);
        assert!((t[5] + 0.5).abs() < 1e-6);
    }

    #[test]
    fn png_round_trips_the_alpha() {
        let mut img = RgbImage::new(3, 2);
        for (i, px) in img.pixels_mut().enumerate() {
            *px = image::Rgb([i as u8 * 40, 10, 200]);
        }
        let alpha = [0u8, 64, 128, 192, 255, 7];
        let png = compose_png(&img, &alpha, false).unwrap();
        let back = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(back.dimensions(), (3, 2));
        for (i, px) in back.pixels().enumerate() {
            assert_eq!(px[3], alpha[i]);
            assert_eq!(px[0], i as u8 * 40);
        }
    }

    #[test]
    fn webp_and_mask_round_trip() {
        let mut img = RgbImage::new(4, 2);
        for (i, px) in img.pixels_mut().enumerate() {
            *px = image::Rgb([i as u8 * 30, 200, 10]);
        }
        let alpha = [0u8, 40, 80, 120, 160, 200, 240, 255];
        let webp = compose_webp(&img, &alpha).unwrap();
        let back = image::load_from_memory(&webp).unwrap().to_rgba8();
        assert_eq!(back.dimensions(), (4, 2));
        for (i, px) in back.pixels().enumerate() {
            assert_eq!(px[3], alpha[i]);
            assert_eq!(px[1], 200);
        }
        let mask = mask_png(&alpha, 4, 2, false).unwrap();
        let back = image::load_from_memory(&mask).unwrap().to_luma8();
        assert_eq!(back.as_raw(), &alpha);
    }

    #[test]
    fn resize_keeps_a_flat_field_flat() {
        let img = RgbImage::from_pixel(50, 30, image::Rgb([200, 100, 50]));
        let small = to_model_size(&img).unwrap();
        assert_eq!(small.dimensions(), (SIDE, SIDE));
        assert!(small.pixels().all(|p| p.0 == [200, 100, 50]));
        let mask = vec![77u8; (SIDE * SIDE) as usize];
        let back = mask_to_size(&mask, 50, 30).unwrap();
        assert_eq!(back.len(), 1500);
        assert!(back.iter().all(|&v| v == 77));
    }
}
