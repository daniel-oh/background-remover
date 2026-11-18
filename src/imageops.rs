//! The image work around the model, matching rembg's `DisSession`:
//!
//! 1. decode, drop alpha (`convert("RGB")`), turn the picture the way its
//!    orientation tag says (`ImageOps.exif_transpose`, as rembg's `remove`)
//! 2. Lanczos to 1024 by 1024, `/255`, `(x - 0.5) / 1.0`, NCHW float32
//! 3. run the model, take the first output's single channel
//! 4. min-max the map to 0..1, `* 255`, uint8 by truncation
//! 5. Lanczos the mask back to the original size, use it as alpha, PNG
//!
//! The resizes go through our own port of Pillow's resampler
//! (`resample.rs`), so the mask lands where the Python service's did, to the
//! bit.

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::metadata::Orientation;
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, RgbImage};

use crate::model::Model;
use crate::profile::Profile;
use crate::resample::resize_lanczos;

/// The default model's input side; other models carry theirs in their
/// [`Profile`].
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
    decode_tagged(bytes, max_pixels).map(|(rgb, _)| rgb)
}

/// [`decode_within`], then the picture turned upright: a phone holds its
/// sensor sideways for a portrait and says so in an orientation tag instead
/// of turning the pixels. Viewers honour the tag; a cutout written without
/// it would come out on its side. This is Pillow's `ImageOps.exif_transpose`,
/// which rembg's `remove` applies before anything else.
pub fn decode_upright(bytes: &[u8], max_pixels: u64) -> Result<RgbImage, CutoutError> {
    let (rgb, orientation) = decode_tagged(bytes, max_pixels)?;
    if orientation == Orientation::NoTransforms {
        return Ok(rgb);
    }
    let mut img = DynamicImage::ImageRgb8(rgb);
    img.apply_orientation(orientation);
    Ok(img.into_rgb8())
}

/// The pixels as stored, and the orientation the file asks for.
fn decode_tagged(bytes: &[u8], max_pixels: u64) -> Result<(RgbImage, Orientation), CutoutError> {
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
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| CutoutError::Decode(e.to_string()))?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let img =
        DynamicImage::from_decoder(decoder).map_err(|e| CutoutError::Decode(e.to_string()))?;
    Ok((img.into_rgb8(), orientation))
}

/// The orientation in a JPEG's Exif segment (the APP1 payload), when it has
/// one: tag 0x0112 in the first directory of the TIFF structure inside.
fn exif_orientation(app1: &[u8]) -> Option<Orientation> {
    let tiff = app1.strip_prefix(b"Exif\0\0")?;
    let big = match tiff.get(..2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let u16_at = |at: usize| {
        let b: [u8; 2] = tiff.get(at..at + 2)?.try_into().ok()?;
        Some(if big {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    };
    let u32_at = |at: usize| {
        let b: [u8; 4] = tiff.get(at..at + 4)?.try_into().ok()?;
        Some(if big {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let dir = u32_at(4)? as usize;
    for i in 0..u16_at(dir)? as usize {
        let entry = dir.checked_add(2 + i * 12)?;
        if u16_at(entry)? == 0x0112 {
            // A SHORT, stored in the first two bytes of the value field.
            return Orientation::from_exif(u16_at(entry + 8)? as u8);
        }
    }
    None
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

fn decode_jpeg(bytes: &[u8], max_pixels: u64) -> Result<(RgbImage, Orientation), CutoutError> {
    // libjpeg reports errors by longjmp, which the mozjpeg crate turns into an
    // unwind. Catch it here so a corrupt file is an error, never a dead process;
    // this only works because the crate is not built with panic=abort.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_jpeg_inner(bytes, max_pixels)
    }))
    .unwrap_or_else(|_| Err(CutoutError::Decode("corrupt JPEG".into())))
}

fn decode_jpeg_inner(
    bytes: &[u8],
    max_pixels: u64,
) -> Result<(RgbImage, Orientation), CutoutError> {
    let decompress = mozjpeg::Decompress::builder()
        .with_markers(&[mozjpeg::Marker::APP(1)])
        .from_mem(bytes)
        .map_err(|e| CutoutError::Decode(e.to_string()))?;
    let orientation = decompress
        .markers()
        .find_map(|m| exif_orientation(m.data))
        .unwrap_or(Orientation::NoTransforms);
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
    let rgb = RgbImage::from_raw(w, h, raw)
        .ok_or_else(|| CutoutError::Decode("jpeg buffer size".into()))?;
    Ok((rgb, orientation))
}

/// The RGB picture at the default model's size.
pub fn to_model_size(rgb: &RgbImage) -> Result<RgbImage, CutoutError> {
    to_side(rgb, SIDE)
}

/// The RGB picture resized to a `side` by `side` square.
pub fn to_side(rgb: &RgbImage, side: u32) -> Result<RgbImage, CutoutError> {
    let data = resize_lanczos(
        rgb.as_raw(),
        rgb.width() as usize,
        rgb.height() as usize,
        3,
        side as usize,
        side as usize,
    );
    RgbImage::from_raw(side, side, data)
        .ok_or_else(|| CutoutError::Resize("bad buffer size".into()))
}

/// NCHW float32 with rembg's normalisation for isnet: `/255`, mean 0.5, std 1.
pub fn tensor_of(rgb: &RgbImage) -> Vec<f32> {
    tensor_with(rgb, &Profile::ISNET)
}

/// NCHW float32, scaled the way the model's profile says: `/255`, minus the
/// mean, over the standard deviation, channel by channel.
pub fn tensor_with(rgb: &RgbImage, profile: &Profile) -> Vec<f32> {
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let raw = rgb.as_raw();
    let plane = w * h;
    let mut out = vec![0f32; 3 * plane];
    for c in 0..3 {
        let (mean, std) = (profile.mean[c], profile.std[c]);
        let channel = &mut out[c * plane..(c + 1) * plane];
        for (i, v) in channel.iter_mut().enumerate() {
            *v = (raw[i * 3 + c] as f32 / 255.0 - mean) / std;
        }
    }
    out
}

/// rembg's post: rescale the map to 0..1 by its own min and max, then uint8
/// the way numpy's `astype("uint8")` does it, by truncation.
pub fn mask_of(plane: &[f32]) -> Vec<u8> {
    mask_with(plane, false)
}

/// [`mask_of`], with a sigmoid first for models whose output is logits
/// (BiRefNet), as rembg's `BiRefNetSessionGeneral` does.
pub fn mask_with(plane: &[f32], sigmoid: bool) -> Vec<u8> {
    let read = |p: f32| if sigmoid { 1.0 / (1.0 + (-p).exp()) } else { p };
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for &p in plane {
        let p = read(p);
        lo = lo.min(p);
        hi = hi.max(p);
    }
    let d = (hi - lo).max(1e-6);
    plane
        .iter()
        .map(|&p| ((read(p) - lo) / d * 255.0) as u8)
        .collect()
}

/// The default model's mask back at the picture's size.
pub fn mask_to_size(mask: &[u8], w: u32, h: u32) -> Result<Vec<u8>, CutoutError> {
    mask_from_side(mask, SIDE, w, h)
}

/// A `side` by `side` mask back at the picture's size.
pub fn mask_from_side(mask: &[u8], side: u32, w: u32, h: u32) -> Result<Vec<u8>, CutoutError> {
    if mask.len() != (side * side) as usize {
        return Err(CutoutError::Resize("mask is not the model's size".into()));
    }
    Ok(resize_lanczos(
        mask,
        side as usize,
        side as usize,
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
    /// Turn the picture upright by its orientation tag first (the default).
    pub upright: bool,
    /// Clean the old background's colour out of the soft edges
    /// ([`crate::refine`]). `None` means only when there is a new
    /// background to show it against.
    pub refine: Option<bool>,
    /// Put the subject on this colour instead of on nothing; the output is
    /// then opaque.
    pub background: Option<[u8; 3]>,
    /// Trim the output to the subject.
    pub crop: bool,
    /// Which of the model's choices to run, by name; `None` is the default.
    pub model: Option<String>,
    /// When the result looks wrong ([`crate::facts::Facts::looks_wrong`]), run the
    /// model's other choice too and keep its answer if that one looks right.
    pub auto: bool,
    /// Where to look for the subject: the whole picture, a box, or around a
    /// point.
    pub focus: Focus,
    /// For the mask output: scale it down so its long side is at most this.
    pub mask_size: Option<u32>,
}

/// Where to look for the subject. Coordinates are fractions of the picture,
/// 0 to 1, from the top left.
///
/// A scene with several things in it has no single subject, and a model
/// asked about the whole of it picks one, or a part, or the background. Run
/// on a region around one thing, the same model cuts it out cleanly (#33).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Focus {
    /// The whole picture.
    #[default]
    Whole,
    /// The subject inside this box, `x, y, w, h`. The model sees the box
    /// with a margin; outside the box the result is transparent.
    Box(f64, f64, f64, f64),
    /// The subject at this point, `x, y`: a box around it that grows until
    /// the subject no longer touches its edge.
    Point(f64, f64),
}

/// Margin added round a box on each side, as a share of the box's size.
const BOX_MARGIN: f64 = 0.08;
/// A point's first box, as a share of the picture's shorter side.
const POINT_START: f64 = 0.4;
/// How much a point's box grows each time the subject touches its edge.
const POINT_GROWTH: f64 = 1.4;
/// Model runs a point may take at most; the last is the whole picture.
const POINT_RUNS: u32 = 5;

impl Default for Options {
    fn default() -> Options {
        Options {
            output: Output::Png,
            png_fast: false,
            max_pixels: 0,
            upright: true,
            refine: None,
            background: None,
            crop: false,
            model: None,
            auto: false,
            focus: Focus::Whole,
            mask_size: None,
        }
    }
}

/// A colour from `RRGGBB` or `RGB` hex, with or without a leading `#`.
pub fn colour_of(text: &str) -> Option<[u8; 3]> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = |s: &str| u8::from_str_radix(s, 16).ok();
    match hex.len() {
        6 => Some([n(&hex[0..2])?, n(&hex[2..4])?, n(&hex[4..6])?]),
        3 => {
            let d = |i: usize| n(&hex[i..i + 1]).map(|v| v * 17);
            Some([d(0)?, d(1)?, d(2)?])
        }
        _ => None,
    }
}

/// Alpha below this is not the subject when looking for its edges: the
/// mask of a busy background is seldom exactly zero everywhere.
const CROP_FLOOR: u8 = 13;

/// The smallest rectangle holding every pixel of the subject, as
/// `(x, y, width, height)`; `None` when the mask is empty.
pub fn subject_box(alpha: &[u8], w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
    let (w, h) = (w as usize, h as usize);
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        let row = &alpha[y * w..(y + 1) * w];
        let Some(first) = row.iter().position(|&a| a >= CROP_FLOOR) else {
            continue;
        };
        let last = row.iter().rposition(|&a| a >= CROP_FLOOR).unwrap_or(first);
        x0 = x0.min(first);
        x1 = x1.max(last);
        y0 = y0.min(y);
        y1 = y;
    }
    (x1 >= x0 && y1 >= y0).then(|| {
        (
            x0 as u32,
            y0 as u32,
            (x1 - x0 + 1) as u32,
            (y1 - y0 + 1) as u32,
        )
    })
}

/// The subject over a solid colour: opaque RGB.
fn flatten(rgb: &RgbImage, alpha: &[u8], background: [u8; 3]) -> Vec<u8> {
    let mut out = rgb.as_raw().clone();
    for (px, &a) in out.as_chunks_mut::<3>().0.iter_mut().zip(alpha) {
        let a = a as u32;
        for c in 0..3 {
            // Rounded, exact at both ends.
            px[c] = ((px[c] as u32 * a + background[c] as u32 * (255 - a) + 127) / 255) as u8;
        }
    }
    out
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
    /// The edge colour clean-up, when asked for.
    pub refine: u32,
    /// Composing and encoding the output.
    pub encode: u32,
}

impl std::fmt::Display for Stages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "decode={}ms resize={}ms model={}ms ",
            self.decode, self.resize, self.model
        )?;
        if self.refine > 0 {
            write!(f, "refine={}ms ", self.refine)?;
        }
        write!(f, "encode={}ms", self.encode)
    }
}

/// A finished removal.
#[derive(Debug)]
pub struct Cutout {
    /// The encoded output.
    pub bytes: Vec<u8>,
    /// The output's width.
    pub width: u32,
    /// The output's height.
    pub height: u32,
    /// Where the time went.
    pub stages: Stages,
    /// The model whose answer this is.
    pub model: String,
    /// What the mask says about the cutout, measured on the whole picture
    /// before any crop.
    pub facts: crate::facts::Facts,
    /// `model=auto` ran the other model too.
    pub retried: bool,
    /// How many times a model ran.
    pub runs: u32,
}

/// A rectangle of the picture in pixels: `x, y, w, h`.
type Region = (u32, u32, u32, u32);

/// A box in fractions as pixels, with `margin` (a share of its size) added
/// on each side, kept inside the picture and at least one pixel.
fn region_of(b: (f64, f64, f64, f64), margin: f64, w: u32, h: u32) -> Region {
    let (bx, by, bw, bh) = b;
    let (fw, fh) = (w as f64, h as f64);
    let x0 = ((bx - bw * margin) * fw).floor().clamp(0.0, fw - 1.0);
    let y0 = ((by - bh * margin) * fh).floor().clamp(0.0, fh - 1.0);
    let x1 = ((bx + bw * (1.0 + margin)) * fw).ceil().clamp(x0 + 1.0, fw);
    let y1 = ((by + bh * (1.0 + margin)) * fh).ceil().clamp(y0 + 1.0, fh);
    (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32)
}

/// A square of `side` pixels centred on a point, moved to lie inside the
/// picture and cut to it where it is larger.
fn square_at(px: f64, py: f64, side: f64, w: u32, h: u32) -> Region {
    let (fw, fh) = (w as f64, h as f64);
    let sw = side.min(fw).max(1.0);
    let sh = side.min(fh).max(1.0);
    let x0 = (px * fw - sw / 2.0).clamp(0.0, fw - sw);
    let y0 = (py * fh - sh / 2.0).clamp(0.0, fh - sh);
    (x0 as u32, y0 as u32, sw as u32, sh as u32)
}

/// Whether the subject reaches an edge of `region` that is not also an edge
/// of the picture: then the region has cut it off.
fn touches_inner_edge(mask: &[u8], region: Region, w: u32, h: u32) -> bool {
    let (rx, ry, rw, rh) = region;
    let band_x = (rw / 64).max(2).min(rw);
    let band_y = (rh / 64).max(2).min(rh);
    let solid = |x: u32, y: u32| mask[(y * rw + x) as usize] > 127;
    let any = |xs: std::ops::Range<u32>, ys: std::ops::Range<u32>| {
        ys.clone().any(|y| xs.clone().any(|x| solid(x, y)))
    };
    (rx > 0 && any(0..band_x, 0..rh))
        || (ry > 0 && any(0..rw, 0..band_y))
        || (rx + rw < w && any(rw - band_x..rw, 0..rh))
        || (ry + rh < h && any(0..rw, rh - band_y..rh))
}

/// Run one model on one region; the mask of that region at its own size.
fn run_on(
    rgb: &RgbImage,
    region: Region,
    model: &Model,
    choice: usize,
    stages: &mut Stages,
) -> Result<Vec<u8>, CutoutError> {
    let ms = |t: std::time::Instant| t.elapsed().as_millis() as u32;
    let (rx, ry, rw, rh) = region;
    let t = std::time::Instant::now();
    let profile = model.profile_of(choice);
    let whole = region == (0, 0, rgb.width(), rgb.height());
    let tensor = if whole {
        tensor_with(&to_side(rgb, profile.side)?, &profile)
    } else {
        let part = image::imageops::crop_imm(rgb, rx, ry, rw, rh).to_image();
        tensor_with(&to_side(&part, profile.side)?, &profile)
    };
    stages.resize += ms(t);
    let t = std::time::Instant::now();
    let plane = model
        .infer_with(choice, tensor)
        .map_err(CutoutError::Model)?;
    stages.model += ms(t);
    let t = std::time::Instant::now();
    let mask = mask_from_side(&mask_with(&plane, profile.sigmoid), profile.side, rw, rh)?;
    stages.resize += ms(t);
    Ok(mask)
}

/// The mask of the whole picture for one model and a focus, and how many
/// times the model ran.
fn segment(
    rgb: &RgbImage,
    focus: Focus,
    model: &Model,
    choice: usize,
    stages: &mut Stages,
) -> Result<(Vec<u8>, u32), CutoutError> {
    let (w, h) = rgb.dimensions();
    let whole: Region = (0, 0, w, h);
    let (region, mask, runs) = match focus {
        Focus::Whole => (whole, run_on(rgb, whole, model, choice, stages)?, 1),
        Focus::Box(x, y, bw, bh) => {
            let region = region_of((x, y, bw, bh), BOX_MARGIN, w, h);
            let mut mask = run_on(rgb, region, model, choice, stages)?;
            // Outside the box itself (the margin included) is not the subject.
            let inner = region_of((x, y, bw, bh), 0.0, w, h);
            let (rx, ry, rw, _) = region;
            for (i, a) in mask.iter_mut().enumerate() {
                let (px, py) = (rx + i as u32 % rw, ry + i as u32 / rw);
                if px < inner.0
                    || py < inner.1
                    || px >= inner.0 + inner.2
                    || py >= inner.1 + inner.3
                {
                    *a = 0;
                }
            }
            (region, mask, 1)
        }
        Focus::Point(px, py) => {
            let mut side = POINT_START * w.min(h) as f64;
            let mut runs = 0;
            loop {
                let region = if runs + 1 >= POINT_RUNS {
                    whole
                } else {
                    square_at(px, py, side, w, h)
                };
                let mask = run_on(rgb, region, model, choice, stages)?;
                runs += 1;
                if region == whole || !touches_inner_edge(&mask, region, w, h) {
                    break (region, mask, runs);
                }
                side *= POINT_GROWTH;
            }
        }
    };
    if region == whole {
        return Ok((mask, runs));
    }
    let t = std::time::Instant::now();
    let (rx, ry, rw, rh) = region;
    let mut full = vec![0u8; (w * h) as usize];
    for row in 0..rh {
        let to = ((ry + row) * w + rx) as usize;
        let from = (row * rw) as usize;
        full[to..to + rw as usize].copy_from_slice(&mask[from..from + rw as usize]);
    }
    stages.resize += t.elapsed().as_millis() as u32;
    Ok((full, runs))
}

/// The whole job: bytes of a photo in, the chosen output out, with the
/// picture's size and the time each stage took.
pub fn cutout_with(bytes: &[u8], model: &Model, opts: &Options) -> Result<Cutout, CutoutError> {
    let ms = |t: std::time::Instant| t.elapsed().as_millis() as u32;
    let mut stages = Stages::default();

    let t = std::time::Instant::now();
    let mut rgb = if opts.upright {
        decode_upright(bytes, opts.max_pixels)?
    } else {
        decode_within(bytes, opts.max_pixels)?
    };
    let (mut w, mut h) = rgb.dimensions();
    stages.decode = ms(t);

    let mut choice = match opts.model.as_deref() {
        None => 0,
        Some(name) => model.choice(name).ok_or_else(|| {
            CutoutError::Model(format!("{name} is not one of the models offered"))
        })?,
    };
    let (mut alpha, mut runs) = segment(&rgb, opts.focus, model, choice, &mut stages)?;
    let mut facts = crate::facts::facts(&alpha, w as usize, h as usize);
    let mut retried = false;
    if opts.auto && facts.looks_wrong() {
        if let Some(other) = (0..model.names().len()).find(|&i| i != choice) {
            let (second, more) = segment(&rgb, opts.focus, model, other, &mut stages)?;
            runs += more;
            retried = true;
            let second_facts = crate::facts::facts(&second, w as usize, h as usize);
            if !second_facts.looks_wrong() {
                (alpha, facts, choice) = (second, second_facts, other);
            }
        }
    }

    let colour = opts.output != Output::MaskPng;
    if colour && opts.refine.unwrap_or(opts.background.is_some()) {
        let t = std::time::Instant::now();
        crate::refine::decontaminate(&mut rgb, &alpha);
        stages.refine = ms(t).max(1);
    }

    let t = std::time::Instant::now();
    if opts.crop {
        if let Some((x, y, cw, ch)) = subject_box(&alpha, w, h) {
            rgb = image::imageops::crop_imm(&rgb, x, y, cw, ch).to_image();
            alpha = (y..y + ch)
                .flat_map(|row| {
                    let at = (row * w + x) as usize;
                    alpha[at..at + cw as usize].iter().copied()
                })
                .collect();
            (w, h) = (cw, ch);
        }
    }
    let bytes = match (opts.output, opts.background) {
        (Output::MaskPng, _) => {
            drop(rgb);
            match opts.mask_size {
                Some(n) if n > 0 && n < w.max(h) => {
                    let scale = n as f64 / w.max(h) as f64;
                    let sw = ((w as f64 * scale).round() as u32).max(1);
                    let sh = ((h as f64 * scale).round() as u32).max(1);
                    let small =
                        resize_lanczos(&alpha, w as usize, h as usize, 1, sw as usize, sh as usize);
                    (w, h) = (sw, sh);
                    mask_png(&small, w, h, opts.png_fast)?
                }
                _ => mask_png(&alpha, w, h, opts.png_fast)?,
            }
        }
        (output, Some(background)) => {
            let flat = flatten(&rgb, &alpha, background);
            drop(rgb);
            drop(alpha);
            if output == Output::Webp {
                let mut out = Vec::with_capacity(flat.len() / 4);
                image::codecs::webp::WebPEncoder::new_lossless(&mut out)
                    .write_image(&flat, w, h, ExtendedColorType::Rgb8)
                    .map_err(|e| CutoutError::Encode(e.to_string()))?;
                out
            } else {
                encode_png(&flat, w, h, ExtendedColorType::Rgb8, opts.png_fast)?
            }
        }
        (Output::Png | Output::Webp, None) => {
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
        model: model.name_of(choice).to_string(),
        facts,
        retried,
        runs,
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
        ..Options::default()
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
    fn a_profile_scales_each_channel_its_own_way() {
        let mut img = RgbImage::new(1, 1);
        img.put_pixel(0, 0, image::Rgb([255, 0, 128]));
        let t = tensor_with(&img, &Profile::BIREFNET);
        assert!((t[0] - (1.0 - 0.485) / 0.229).abs() < 1e-5);
        assert!((t[1] - (0.0 - 0.456) / 0.224).abs() < 1e-5);
        assert!((t[2] - (128.0 / 255.0 - 0.406) / 0.225).abs() < 1e-5);
        assert_eq!(tensor_with(&img, &Profile::ISNET), tensor_of(&img));
    }

    #[test]
    fn logits_go_through_a_sigmoid_before_the_rescale() {
        // Logits -20, 0, 20 are 0, one half and 1 to within rounding.
        assert_eq!(mask_with(&[-20.0, 0.0, 20.0], true), vec![0, 127, 255]);
        // Without it the same numbers are just a linear ramp.
        assert_eq!(mask_with(&[-20.0, 10.0, 20.0], false), vec![0, 191, 255]);
        // A sigmoid squeezes; a ramp of logits is not a ramp of alpha.
        assert_eq!(mask_with(&[-20.0, 10.0, 20.0], true), vec![0, 254, 255]);
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

    /// `testdata/sample.jpg` with an Exif segment saying how to turn it.
    pub(crate) fn jpeg_tagged(orientation: u8, big_endian: bool) -> Vec<u8> {
        let jpeg = std::fs::read("testdata/sample.jpg").unwrap();
        let mut tiff: Vec<u8> = Vec::new();
        if big_endian {
            tiff.extend(b"MM\0\x2A\0\0\0\x08\0\x01");
            tiff.extend([0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, orientation, 0, 0]);
        } else {
            tiff.extend(b"II\x2A\0\x08\0\0\0\x01\0");
            tiff.extend([0x12, 0x01, 3, 0, 1, 0, 0, 0, orientation, 0, 0, 0]);
        }
        tiff.extend([0, 0, 0, 0]);
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend((tiff.len() as u16 + 8).to_be_bytes());
        out.extend(b"Exif\0\0");
        out.extend(tiff);
        out.extend(&jpeg[2..]);
        out
    }

    #[test]
    fn a_phone_portrait_comes_out_upright() {
        let plain = decode(&std::fs::read("testdata/sample.jpg").unwrap()).unwrap();
        assert_eq!(plain.dimensions(), (960, 1200));
        // Tag 6: the stored picture must be turned a quarter clockwise.
        for big_endian in [true, false] {
            let tagged = jpeg_tagged(6, big_endian);
            // As stored: the tag changes nothing.
            assert_eq!(decode(&tagged).unwrap(), plain);
            let upright = decode_upright(&tagged, 0).unwrap();
            assert_eq!(upright.dimensions(), (1200, 960));
            // Stored top-left ends up top-right.
            assert_eq!(upright.get_pixel(1199, 0), plain.get_pixel(0, 0));
            assert_eq!(upright.get_pixel(0, 0), plain.get_pixel(0, 1199));
        }
        // Tag 3: upside down. Tag 1 and no tag: as stored.
        let flipped = decode_upright(&jpeg_tagged(3, true), 0).unwrap();
        assert_eq!(flipped.get_pixel(959, 1199), plain.get_pixel(0, 0));
        assert_eq!(decode_upright(&jpeg_tagged(1, true), 0).unwrap(), plain);
        let untagged = std::fs::read("testdata/sample.jpg").unwrap();
        assert_eq!(decode_upright(&untagged, 0).unwrap(), plain);
    }

    #[test]
    fn a_broken_exif_segment_is_no_orientation() {
        assert_eq!(exif_orientation(b"Exif\0\0"), None);
        assert_eq!(exif_orientation(b"Exif\0\0MM\0\x2A\xFF\xFF\xFF\xFF"), None);
        assert_eq!(
            exif_orientation(b"http://ns.adobe.com/xap/1.0/\0<x/>"),
            None
        );
        // An orientation outside 1..8 is ignored.
        let mut odd = b"Exif\0\0MM\0\x2A\0\0\0\x08\0\x01".to_vec();
        odd.extend([0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 9, 0, 0]);
        assert_eq!(exif_orientation(&odd), None);
    }

    #[test]
    fn colours_parse_from_hex() {
        assert_eq!(colour_of("ff8000"), Some([255, 128, 0]));
        assert_eq!(colour_of("#FFF"), Some([255, 255, 255]));
        assert_eq!(colour_of("0a0"), Some([0, 170, 0]));
        for bad in ["", "ffff", "gggggg", "white", "+12345", "ff80001"] {
            assert_eq!(colour_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_subject_box_ignores_faint_noise() {
        // 6 by 4: a faint pixel in a corner, the subject in the middle.
        let mut alpha = vec![0u8; 24];
        alpha[0] = 5;
        alpha[6 + 2] = 255;
        alpha[12 + 3] = 40;
        assert_eq!(subject_box(&alpha, 6, 4), Some((2, 1, 2, 2)));
        assert_eq!(subject_box(&[0, 3, 12, 0], 2, 2), None);
    }

    #[test]
    fn flattening_is_exact_at_both_ends() {
        let mut img = RgbImage::new(3, 1);
        for px in img.pixels_mut() {
            *px = image::Rgb([200, 100, 0]);
        }
        let flat = flatten(&img, &[255, 0, 128], [10, 20, 30]);
        assert_eq!(&flat[0..3], &[200, 100, 0]);
        assert_eq!(&flat[3..6], &[10, 20, 30]);
        assert_eq!(&flat[6..9], &[105, 60, 15]);
    }

    #[test]
    fn a_box_becomes_pixels_with_its_margin_inside_the_picture() {
        assert_eq!(
            region_of((0.25, 0.25, 0.5, 0.5), 0.0, 100, 200),
            (25, 50, 50, 100)
        );
        // 8% of a 50 by 100 box is 4 and 8 pixels each side.
        assert_eq!(
            region_of((0.25, 0.25, 0.5, 0.5), 0.08, 100, 200),
            (21, 42, 58, 116)
        );
        // At the picture's edge the margin is cut off, never outside.
        assert_eq!(
            region_of((0.0, 0.0, 1.0, 1.0), 0.08, 100, 200),
            (0, 0, 100, 200)
        );
    }

    #[test]
    fn a_point_square_stays_inside_the_picture() {
        assert_eq!(square_at(0.5, 0.5, 40.0, 100, 200), (30, 80, 40, 40));
        assert_eq!(square_at(0.0, 1.0, 40.0, 100, 200), (0, 160, 40, 40));
        // Larger than the picture: the picture.
        assert_eq!(square_at(0.5, 0.5, 500.0, 100, 200), (0, 0, 100, 200));
    }

    #[test]
    fn a_subject_cut_off_by_the_region_is_noticed_but_not_at_the_picture_edge() {
        // A 20 by 20 region whose right column is solid.
        let mut mask = vec![0u8; 400];
        for y in 0..20 {
            mask[y * 20 + 19] = 255;
        }
        // In the middle of a larger picture: the subject goes on past it.
        assert!(touches_inner_edge(&mask, (10, 10, 20, 20), 100, 100));
        // A 100 by 20 picture whose right edge the region shares, top and
        // bottom too: nothing was cut off.
        assert!(!touches_inner_edge(&mask, (80, 0, 20, 20), 100, 20));
        assert!(!touches_inner_edge(&[0u8; 400], (10, 10, 20, 20), 100, 100));
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
