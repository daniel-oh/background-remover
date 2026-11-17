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
use image::{ExtendedColorType, ImageEncoder, RgbImage};

use crate::model::Model;
use crate::resample::resize_lanczos;

/// The model's input side.
pub const SIDE: u32 = 1024;

/// Where a removal went wrong.
#[derive(Debug)]
#[non_exhaustive]
pub enum CutoutError {
    /// The bytes were not a picture this service decodes.
    Decode(String),
    /// The picture has more pixels than the caller allows. Found by reading
    /// the header, before anything is decoded.
    TooLarge {
        /// Width times height, as the file declares it.
        pixels: u64,
        /// The limit it went over.
        max: u64,
    },
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
            CutoutError::TooLarge { pixels, max } => write!(
                f,
                "the picture is {:.1} megapixels; {:.1} at most",
                *pixels as f64 / 1e6,
                *max as f64 / 1e6
            ),
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
/// pixels are the ones the Python service saw. No limit on the picture's
/// size; see [`decode_within`].
pub fn decode(bytes: &[u8]) -> Result<RgbImage, CutoutError> {
    decode_within(bytes, 0)
}

/// [`decode`], refusing a picture of more than `max_pixels` pixels (0 means
/// no limit). The size comes from the file's header, so a small file that
/// declares an enormous picture is refused before any of it is decoded: the
/// body limit bounds the bytes sent, this bounds the memory they expand to.
pub fn decode_within(bytes: &[u8], max_pixels: u64) -> Result<RgbImage, CutoutError> {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return decode_jpeg(bytes, max_pixels);
    }
    let reader = || {
        image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| CutoutError::Decode(e.to_string()))
    };
    let (w, h) = reader()?
        .into_dimensions()
        .map_err(|e| CutoutError::Decode(e.to_string()))?;
    within(w as u64, h as u64, max_pixels)?;
    let mut reader = reader()?;
    // The decoder's own allocation limit, sized to the picture we just
    // agreed to: 16-bit RGBA is the widest thing it can hand back.
    let mut limits = image::Limits::default();
    limits.max_alloc = Some((w as u64 * h as u64 * 8).max(64 << 20));
    reader.limits(limits);
    reader
        .decode()
        .map(|img| img.into_rgb8())
        .map_err(|e| CutoutError::Decode(e.to_string()))
}

fn within(w: u64, h: u64, max_pixels: u64) -> Result<(), CutoutError> {
    let pixels = w.saturating_mul(h);
    if max_pixels > 0 && pixels > max_pixels {
        return Err(CutoutError::TooLarge {
            pixels,
            max: max_pixels,
        });
    }
    Ok(())
}

fn decode_jpeg(bytes: &[u8], max_pixels: u64) -> Result<RgbImage, CutoutError> {
    // libjpeg reports errors by longjmp, which the mozjpeg crate turns into an
    // unwind. Catch it here so a corrupt file is an error, never a dead process;
    // this only works because the crate is not built with panic=abort.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_jpeg_inner(bytes, max_pixels)
    }))
    .unwrap_or_else(|_| Err(CutoutError::Decode("corrupt JPEG".into())))
}

fn decode_jpeg_inner(bytes: &[u8], max_pixels: u64) -> Result<RgbImage, CutoutError> {
    let decompress =
        mozjpeg::Decompress::new_mem(bytes).map_err(|e| CutoutError::Decode(e.to_string()))?;
    // The header is read; nothing is allocated for the pixels yet.
    within(
        decompress.width() as u64,
        decompress.height() as u64,
        max_pixels,
    )?;
    let mut image = decompress
        .rgb()
        .map_err(|e| CutoutError::Decode(e.to_string()))?;
    let (w, h) = (image.width() as u32, image.height() as u32);
    let raw: Vec<u8> = image
        .read_scanlines::<u8>()
        .map_err(|e| CutoutError::Decode(e.to_string()))?;
    image
        .finish()
        .map_err(|e| CutoutError::Decode(e.to_string()))?;
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

/// The picture's RGB and the mask, interleaved as RGBA.
fn rgba_of(rgb: &RgbImage, alpha: &[u8]) -> Vec<u8> {
    let raw = rgb.as_raw();
    let mut rgba = vec![0u8; raw.len() / 3 * 4];
    for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        px[0] = raw[i * 3];
        px[1] = raw[i * 3 + 1];
        px[2] = raw[i * 3 + 2];
        px[3] = alpha[i];
    }
    rgba
}

fn png_compression(fast: bool) -> CompressionType {
    if fast {
        CompressionType::Fast
    } else {
        CompressionType::Default
    }
}

fn encode_png(
    data: &[u8],
    w: u32,
    h: u32,
    color: ExtendedColorType,
    fast: bool,
) -> Result<Vec<u8>, CutoutError> {
    let mut out = Vec::with_capacity(data.len() / 4);
    PngEncoder::new_with_quality(&mut out, png_compression(fast), FilterType::Adaptive)
        .write_image(data, w, h, color)
        .map_err(|e| CutoutError::Encode(e.to_string()))?;
    Ok(out)
}

fn encode_webp(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, CutoutError> {
    let mut out = Vec::with_capacity(rgba.len() / 4);
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .write_image(rgba, w, h, ExtendedColorType::Rgba8)
        .map_err(|e| CutoutError::Encode(e.to_string()))?;
    Ok(out)
}

/// The original RGB with the mask as alpha, as a PNG. `fast` trades about a
/// quarter more bytes for a much quicker encode.
pub fn compose_png(rgb: &RgbImage, alpha: &[u8], fast: bool) -> Result<Vec<u8>, CutoutError> {
    encode_png(
        &rgba_of(rgb, alpha),
        rgb.width(),
        rgb.height(),
        ExtendedColorType::Rgba8,
        fast,
    )
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
    encode_webp(&rgba_of(rgb, alpha), rgb.width(), rgb.height())
}

/// The mask alone, as an 8-bit greyscale PNG at the picture's size.
pub fn mask_png(alpha: &[u8], w: u32, h: u32, fast: bool) -> Result<Vec<u8>, CutoutError> {
    encode_png(alpha, w, h, ExtendedColorType::L8, fast)
}

/// How a removal should be done. `Options::default()` is a PNG at the
/// default compression with no limit on the picture's size.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Options {
    /// What to hand back.
    pub output: Output,
    /// The fast PNG encoder: larger files, quicker.
    pub png_fast: bool,
    /// Refuse pictures of more pixels than this; 0 means no limit.
    pub max_pixels: u64,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            output: Output::Png,
            png_fast: false,
            max_pixels: 0,
        }
    }
}

/// Milliseconds each stage of one removal took.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stages {
    /// Decoding the picture.
    pub decode: u32,
    /// Both resizes: down to the model's size and the mask back up.
    pub resize: u32,
    /// The model, including loading it when it was not resident.
    pub model: u32,
    /// Composing and encoding the output.
    pub encode: u32,
}

impl std::fmt::Display for Stages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "decode={}ms resize={}ms model={}ms encode={}ms",
            self.decode, self.resize, self.model, self.encode
        )
    }
}

/// A finished removal.
#[derive(Debug)]
pub struct Cutout {
    /// The encoded output.
    pub bytes: Vec<u8>,
    /// The picture's width.
    pub width: u32,
    /// The picture's height.
    pub height: u32,
    /// Where the time went.
    pub stages: Stages,
}

/// The whole job: bytes of a photo in, the chosen output out, with the
/// picture's size and the time each stage took.
pub fn cutout_with(bytes: &[u8], model: &Model, opts: &Options) -> Result<Cutout, CutoutError> {
    let ms = |t: std::time::Instant| t.elapsed().as_millis() as u32;
    let mut stages = Stages::default();

    let t = std::time::Instant::now();
    let rgb = decode_within(bytes, opts.max_pixels)?;
    let (w, h) = rgb.dimensions();
    stages.decode = ms(t);

    let t = std::time::Instant::now();
    let tensor = tensor_of(&to_model_size(&rgb)?);
    stages.resize = ms(t);

    let t = std::time::Instant::now();
    let plane = model.infer(tensor).map_err(CutoutError::Model)?;
    stages.model = ms(t);

    let t = std::time::Instant::now();
    let alpha = mask_to_size(&mask_of(&plane), w, h)?;
    stages.resize += ms(t);

    let t = std::time::Instant::now();
    let bytes = match opts.output {
        Output::MaskPng => {
            drop(rgb);
            mask_png(&alpha, w, h, opts.png_fast)?
        }
        Output::Png | Output::Webp => {
            // Let the separate planes go before encoding: the encoder only
            // needs the interleaved copy, and a large picture is large.
            let rgba = rgba_of(&rgb, &alpha);
            drop(rgb);
            drop(alpha);
            if opts.output == Output::Webp {
                encode_webp(&rgba, w, h)?
            } else {
                encode_png(&rgba, w, h, ExtendedColorType::Rgba8, opts.png_fast)?
            }
        }
    };
    stages.encode = ms(t);

    Ok(Cutout {
        bytes,
        width: w,
        height: h,
        stages,
    })
}

/// Bytes of a photo in, the chosen output out. No limit on the picture's
/// size; [`cutout_with`] takes one.
pub fn cutout_as(
    bytes: &[u8],
    model: &Model,
    png_fast: bool,
    output: Output,
) -> Result<Vec<u8>, CutoutError> {
    let opts = Options {
        output,
        png_fast,
        max_pixels: 0,
    };
    cutout_with(bytes, model, &opts).map(|c| c.bytes)
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

    /// Deterministic junk after a JPEG marker: the kind of body that made the
    /// service abort before 0.2.1.
    pub(crate) fn corrupt_jpeg() -> Vec<u8> {
        let mut x: u32 = 0x1234_5678;
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0];
        out.extend((0..5000).map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        }));
        out
    }

    #[test]
    fn a_corrupt_jpeg_is_an_error_not_a_crash() {
        let err = decode(&corrupt_jpeg()).expect_err("junk must not decode");
        assert!(matches!(err, CutoutError::Decode(_)), "{err}");
        // Still able to decode afterwards: libjpeg's error path left nothing behind.
        let good = std::fs::read("testdata/sample.jpg").unwrap();
        assert!(decode(&good).is_ok());
    }

    /// `testdata/sample.jpg` with the size in its frame header rewritten: a
    /// small file that claims to be an enormous picture.
    pub(crate) fn jpeg_claiming(w: u16, h: u16) -> Vec<u8> {
        let mut jpeg = std::fs::read("testdata/sample.jpg").unwrap();
        let mut at = 2;
        loop {
            assert_eq!(jpeg[at], 0xFF, "a marker at {at}");
            let len = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;
            // SOF0..SOF2: precision, then height, then width.
            if matches!(jpeg[at + 1], 0xC0..=0xC2) {
                jpeg[at + 5..at + 7].copy_from_slice(&h.to_be_bytes());
                jpeg[at + 7..at + 9].copy_from_slice(&w.to_be_bytes());
                return jpeg;
            }
            at += 2 + len;
        }
    }

    /// A PNG that is a signature, a header for a `w` by `h` picture, and an
    /// empty data chunk: 53 bytes.
    pub(crate) fn png_claiming(w: u32, h: u32) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &b in data {
                crc ^= b as u32;
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
                }
            }
            !crc
        }
        let mut header = b"IHDR".to_vec();
        header.extend(w.to_be_bytes());
        header.extend(h.to_be_bytes());
        header.extend([8, 2, 0, 0, 0]); // 8-bit RGB, no interlace
        let mut data = b"IDAT".to_vec();
        data.extend([0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]); // zlib, nothing in it
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        for chunk in [header, data] {
            png.extend((chunk.len() as u32 - 4).to_be_bytes());
            png.extend(&chunk);
            png.extend(crc32(&chunk).to_be_bytes());
        }
        png
    }

    #[test]
    fn a_small_file_claiming_a_huge_picture_is_refused_from_its_header() {
        for (name, bytes) in [
            ("jpeg", jpeg_claiming(30_000, 30_000)),
            ("png", png_claiming(30_000, 30_000)),
        ] {
            let started = std::time::Instant::now();
            let err = decode_within(&bytes, 25_000_000).expect_err(name);
            assert!(
                matches!(
                    err,
                    CutoutError::TooLarge {
                        pixels: 900_000_000,
                        max: 25_000_000
                    }
                ),
                "{name}: {err}"
            );
            // Refused on the header alone: no time to have decoded anything.
            assert!(started.elapsed().as_millis() < 200, "{name} took too long");
        }
    }

    #[test]
    fn the_limit_is_on_pixels_and_zero_means_none() {
        let good = std::fs::read("testdata/sample.jpg").unwrap(); // 960 by 1200
        assert!(decode_within(&good, 960 * 1200).is_ok());
        assert!(matches!(
            decode_within(&good, 960 * 1200 - 1),
            Err(CutoutError::TooLarge { .. })
        ));
        assert!(decode_within(&good, 0).is_ok());
        let png = std::fs::read("testdata/sample.png").unwrap();
        assert!(matches!(
            decode_within(&png, 1000),
            Err(CutoutError::TooLarge { .. })
        ));
        assert!(decode(&png).is_ok());
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
