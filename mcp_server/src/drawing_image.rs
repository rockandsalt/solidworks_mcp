//! The render_drawing tool: a picture of a drawing sheet as SolidWorks
//! prints it (the add-in has SolidWorks save the sheet as an image), cropped
//! to a view or region, scaled to fit, with rulers in sheet millimeters so
//! positions in the picture read the same as inspect_drawing's positions and
//! add_drawing_dimensions' at_mm.

use image::{GenericImageView, ImageFormat, Rgb, RgbImage, imageops};
use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{MM_PER_M, round};

/// Longest side of the picture by default: about what a model sees in full.
pub const DEFAULT_MAX_PX: u32 = 1568;
const MIN_MAX_PX: u32 = 400;
const MAX_MAX_PX: u32 = 3000;
/// SolidWorks draws 1-px lines at any resolution, which scaling down would
/// average to light gray, so the image is asked for at the resolution the
/// picture needs -- but SolidWorks won't export below 100 dpi (asked for 60
/// or 94, it wrote 100), so big sheets are still scaled down (`shrink`) --
/// nor above 300 (asked for 433, it fell back to another setting), so a
/// small region comes out smaller than max_px.
const MIN_DPI: i32 = 100;
pub const MAX_DPI: i32 = 300;
/// For a view whose size isn't known yet.
pub const VIEW_DPI: i32 = MAX_DPI;
/// Room around a view's outline for its dimensions and notes, mm.
const VIEW_MARGIN_MM: f64 = 20.0;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RenderDrawingParams {
    /// The sheet to draw, e.g. "Sheet2". Defaults to the active sheet, or the sheet `view` is on.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Zoom to one view, e.g. "Drawing View1": its outline plus 20 mm around it for its
    /// dimensions.
    #[serde(default)]
    pub view: Option<String>,
    /// Zoom to a region of the sheet: two opposite corners in sheet mm from the lower-left
    /// corner, e.g. [[60, 110], [140, 190]].
    #[serde(default)]
    pub region_mm: Option<[[f64; 2]; 2]>,
    /// Longest side of the picture in pixels, 400..3000. Defaults to 1568.
    #[serde(default)]
    pub max_px: Option<u32>,
}

/// A rectangle on the sheet, meters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub fn width_m(&self) -> f64 {
        self.x1 - self.x0
    }
    pub fn height_m(&self) -> f64 {
        self.y1 - self.y0
    }
    fn intersect(&self, o: &Rect) -> Option<Rect> {
        let r = Rect { x0: self.x0.max(o.x0), y0: self.y0.max(o.y0), x1: self.x1.min(o.x1), y1: self.y1.min(o.y1) };
        (r.width_m() > 0.0 && r.height_m() > 0.0).then_some(r)
    }
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

pub fn max_px(params: &RenderDrawingParams) -> Result<u32, McpError> {
    match params.max_px {
        None => Ok(DEFAULT_MAX_PX),
        Some(px) if (MIN_MAX_PX..=MAX_MAX_PX).contains(&px) => Ok(px),
        Some(px) => Err(invalid(format!("max_px must be {MIN_MAX_PX}..{MAX_MAX_PX}, got {px}"))),
    }
}

/// The region asked for with region_mm, meters.
pub fn region(params: &RenderDrawingParams) -> Result<Option<Rect>, McpError> {
    let Some([a, b]) = params.region_mm else {
        return Ok(None);
    };
    if params.view.is_some() {
        return Err(invalid("give view or region_mm, not both"));
    }
    if !a.iter().chain(&b).all(|v| v.is_finite()) {
        return Err(invalid("region_mm must be numbers"));
    }
    let r = Rect {
        x0: a[0].min(b[0]) / MM_PER_M,
        y0: a[1].min(b[1]) / MM_PER_M,
        x1: a[0].max(b[0]) / MM_PER_M,
        y1: a[1].max(b[1]) / MM_PER_M,
    };
    if r.width_m() < 0.005 || r.height_m() < 0.005 {
        return Err(invalid("region_mm must be at least 5 x 5 mm"));
    }
    Ok(Some(r))
}

/// Resolution at which a region `size_m` long comes closest to `max_px`
/// pixels without going over.
pub fn dpi_for(size_m: f64, max_px: u32) -> i32 {
    let dpi = f64::from(max_px) / (size_m * MM_PER_M / 25.4);
    (dpi.floor() as i32).clamp(MIN_DPI, MAX_DPI)
}

/// Whether an image whose crop spans `crop_px` pixels is worth asking again
/// at another resolution: too coarse to fill the picture, or big enough that
/// scaling it down would wash out the lines.
pub fn needs_other_dpi(crop_px: f64, max_px: u32) -> bool {
    crop_px < 0.9 * f64::from(max_px) || crop_px > 1.15 * f64::from(max_px)
}

fn outline_rect(v: &proto::SheetViewOutline) -> Option<Rect> {
    let o = v.outline.as_ref()?;
    let (min, max) = (o.min.as_ref()?, o.max.as_ref()?);
    Some(Rect { x0: min.x, y0: min.y, x1: max.x, y1: max.y })
}

/// What to show of the sheet in `reply`, meters.
pub fn crop_rect(
    reply: &proto::RenderDrawingResponse,
    view: Option<&str>,
    region: Option<Rect>,
) -> Result<Rect, McpError> {
    let sheet = Rect { x0: 0.0, y0: 0.0, x1: reply.width, y1: reply.height };
    let wanted = match (view, region) {
        (Some(name), _) => {
            let v = reply.views.iter().find(|v| v.name == name).ok_or_else(|| {
                let names: Vec<&str> = reply.views.iter().map(|v| v.name.as_str()).collect();
                invalid(format!(
                    "sheet {} has no view {name:?} (it has: {}) -- give the sheet the view is on",
                    reply.sheet_name,
                    names.join(", ")
                ))
            })?;
            let o = outline_rect(v).ok_or_else(|| invalid(format!("SolidWorks gave no outline for {name}")))?;
            let m = VIEW_MARGIN_MM / MM_PER_M;
            Rect { x0: o.x0 - m, y0: o.y0 - m, x1: o.x1 + m, y1: o.y1 + m }
        }
        (None, Some(r)) => r,
        (None, None) => sheet,
    };
    wanted.intersect(&sheet).ok_or_else(|| {
        invalid(format!(
            "that region is off the sheet ({} x {} mm)",
            round(reply.width * MM_PER_M, 1),
            round(reply.height * MM_PER_M, 1)
        ))
    })
}

pub struct Picture {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Picture pixels per sheet millimeter.
    pub px_per_mm: f64,
    /// The sheet image's proportions didn't match the sheet's.
    pub distorted: bool,
}

/// Decodes SolidWorks' image of the whole sheet.
pub fn decode(reply: &proto::RenderDrawingResponse) -> Result<RgbImage, McpError> {
    let format = if reply.format == "jpg" { ImageFormat::Jpeg } else { ImageFormat::Png };
    let image = image::load_from_memory_with_format(&reply.image, format).map_err(|err| {
        McpError::internal_error(format!("couldn't read the {} SolidWorks saved: {err}", reply.format), None)
    })?;
    Ok(image.to_rgb8())
}

/// Pixels of `image` (the whole sheet) per sheet meter, x and y.
fn sheet_scale(image: &RgbImage, reply: &proto::RenderDrawingResponse) -> (f64, f64) {
    (f64::from(image.width()) / reply.width, f64::from(image.height()) / reply.height)
}

/// How many pixels of the sheet image the crop spans along its longer side.
pub fn crop_pixels(image: &RgbImage, reply: &proto::RenderDrawingResponse, crop: Rect) -> f64 {
    let (sx, sy) = sheet_scale(image, reply);
    (crop.width_m() * sx).max(crop.height_m() * sy)
}

/// Crops `image` (the whole sheet) to `crop`, scales it to fit `max_px` and
/// adds the rulers.
pub fn picture(
    image: &RgbImage,
    reply: &proto::RenderDrawingResponse,
    crop: Rect,
    max_px: u32,
) -> Result<Picture, McpError> {
    let (sx, sy) = sheet_scale(image, reply);
    let distorted = (sx / sy - 1.0).abs() > 0.03;
    // Image rows run down from the sheet's top edge.
    let px0 = (crop.x0 * sx).floor().max(0.0) as u32;
    let px1 = ((crop.x1 * sx).ceil() as u32).min(image.width());
    let py0 = ((reply.height - crop.y1) * sy).floor().max(0.0) as u32;
    let py1 = (((reply.height - crop.y0) * sy).ceil() as u32).min(image.height());
    if px1 <= px0 || py1 <= py0 {
        return Err(invalid("that region is empty"));
    }
    let cropped = image.view(px0, py0, px1 - px0, py1 - py0).to_image();
    let scale = (f64::from(max_px) / f64::from(cropped.width().max(cropped.height()))).min(1.0);
    let (w, h) = (
        ((f64::from(cropped.width()) * scale).round() as u32).max(1),
        ((f64::from(cropped.height()) * scale).round() as u32).max(1),
    );
    let scaled = if scale < 1.0 { shrink(&cropped, w, h) } else { cropped };

    // Sheet meters -> picture pixels (before the ruler margin).
    let shown = Rect {
        x0: f64::from(px0) / sx,
        y0: reply.height - f64::from(py1) / sy,
        x1: f64::from(px1) / sx,
        y1: reply.height - f64::from(py0) / sy,
    };
    let canvas = with_rulers(&scaled, shown);
    let mut png = Vec::new();
    canvas
        .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|err| McpError::internal_error(format!("couldn't encode the picture: {err}"), None))?;
    Ok(Picture {
        png,
        width: canvas.width(),
        height: canvas.height(),
        px_per_mm: f64::from(scaled.width()) / (shown.width_m() * MM_PER_M),
        distorted,
    })
}

/// Scales `image` down to `w` x `h`, keeping thin dark lines dark: the
/// average of each output pixel's source pixels, pulled most of the way to
/// the darkest of them.
fn shrink(image: &RgbImage, w: u32, h: u32) -> RgbImage {
    const DARKEN: f64 = 0.7;
    let average = imageops::resize(image, w, h, imageops::FilterType::Triangle);
    let (sw, sh) = image.dimensions();
    let span = |i: u32, n: u32, src: u32| {
        let lo = (u64::from(i) * u64::from(src) / u64::from(n)) as u32;
        let hi = ((u64::from(i + 1) * u64::from(src)).div_ceil(u64::from(n)) as u32).clamp(lo + 1, src);
        lo..hi
    };
    let mut out = average;
    for y in 0..h {
        let ys = span(y, h, sh);
        for x in 0..w {
            let mut darkest = [255u8; 3];
            for sy in ys.clone() {
                for sx in span(x, w, sw) {
                    let p = image.get_pixel(sx, sy).0;
                    if u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])
                        < u32::from(darkest[0]) + u32::from(darkest[1]) + u32::from(darkest[2])
                    {
                        darkest = p;
                    }
                }
            }
            let px = out.get_pixel_mut(x, y);
            for c in 0..3 {
                let avg = f64::from(px.0[c]);
                px.0[c] = (avg - DARKEN * (avg - f64::from(darkest[c])).max(0.0)).round() as u8;
            }
        }
    }
    out
}

// ---- Rulers -----------------------------------------------------------------------

const RULER_LEFT: u32 = 60;
const RULER_BOTTOM: u32 = 30;
const INK: Rgb<u8> = Rgb([40, 40, 40]);
const GRID: Rgb<u8> = Rgb([170, 200, 255]);
/// Digit glyphs, 5 x 7, one row per byte (bit 4 = leftmost column).
const DIGITS: [[u8; 7]; 10] = [
    [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
    [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
    [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
    [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
    [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
    [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
    [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
    [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
    [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
];
/// Glyph pixels are drawn this many picture pixels square.
const GLYPH_SCALE: u32 = 2;
const GLYPH_ADVANCE: u32 = 6 * GLYPH_SCALE;

/// Width in pixels of a number drawn with `draw_number`.
fn number_width(n: u32) -> u32 {
    n.to_string().len() as u32 * GLYPH_ADVANCE - GLYPH_SCALE
}

fn draw_number(canvas: &mut RgbImage, n: u32, x: i64, y: i64) {
    for (i, c) in n.to_string().bytes().enumerate() {
        let glyph = &DIGITS[usize::from(c - b'0')];
        let gx = x + i as i64 * i64::from(GLYPH_ADVANCE);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if bits & (0x10 >> col) == 0 {
                    continue;
                }
                for dy in 0..GLYPH_SCALE {
                    for dx in 0..GLYPH_SCALE {
                        let px = gx + i64::from(col * GLYPH_SCALE + dx);
                        let py = y + (row as i64) * i64::from(GLYPH_SCALE) + i64::from(dy);
                        if px >= 0 && py >= 0 && (px as u32) < canvas.width() && (py as u32) < canvas.height() {
                            canvas.put_pixel(px as u32, py as u32, INK);
                        }
                    }
                }
            }
        }
    }
}

/// The smallest of 1, 2, 5, 10, 20, 50, ... mm at least `min_px` pixels long.
pub fn step_mm(px_per_mm: f64, min_px: f64) -> u32 {
    let mut base = 1;
    loop {
        for m in [1, 2, 5] {
            if f64::from(base * m) * px_per_mm >= min_px {
                return base * m;
            }
        }
        base *= 10;
        if base > 100_000 {
            return base;
        }
    }
}

/// `image` (showing `shown` of the sheet) with mm rulers along its left and
/// bottom edges and light grid lines at the labeled ticks, drawn only over
/// blank (near-white) pixels.
fn with_rulers(image: &RgbImage, shown: Rect) -> RgbImage {
    let (w, h) = image.dimensions();
    let mut canvas = RgbImage::from_pixel(w + RULER_LEFT, h + RULER_BOTTOM, Rgb([255, 255, 255]));
    imageops::replace(&mut canvas, image, i64::from(RULER_LEFT), 0);

    let px_per_mm_x = f64::from(w) / (shown.width_m() * MM_PER_M);
    let px_per_mm_y = f64::from(h) / (shown.height_m() * MM_PER_M);
    let minor = step_mm(px_per_mm_x.min(px_per_mm_y), 6.0);
    let label = step_mm(px_per_mm_x.min(px_per_mm_y), 56.0);
    let blank = |p: &Rgb<u8>| p.0.iter().all(|&c| c >= 235);

    // x along the bottom.
    let (mm0, mm1) = (shown.x0 * MM_PER_M, shown.x1 * MM_PER_M);
    let mut t = (mm0 / f64::from(minor)).ceil() as i64 * i64::from(minor);
    while (t as f64) <= mm1 {
        let x = i64::from(RULER_LEFT) + ((t as f64 - mm0) * px_per_mm_x).round() as i64;
        let labeled = t >= 0 && t % i64::from(label) == 0;
        if x < canvas.width() as i64 {
            let len = if labeled { 8 } else { 4 };
            for dy in 0..len {
                canvas.put_pixel(x as u32, h + dy, INK);
            }
            if labeled {
                for y in 0..h {
                    if blank(canvas.get_pixel(x as u32, y)) {
                        canvas.put_pixel(x as u32, y, GRID);
                    }
                }
                let n = t as u32;
                draw_number(&mut canvas, n, x - i64::from(number_width(n) / 2), i64::from(h) + 12);
            }
        }
        t += i64::from(minor);
    }

    // y up the left side.
    let (mm0, mm1) = (shown.y0 * MM_PER_M, shown.y1 * MM_PER_M);
    let mut t = (mm0 / f64::from(minor)).ceil() as i64 * i64::from(minor);
    while (t as f64) <= mm1 {
        let y = i64::from(h) - 1 - ((t as f64 - mm0) * px_per_mm_y).round() as i64;
        let labeled = t >= 0 && t % i64::from(label) == 0;
        if y >= 0 {
            let len = if labeled { 8 } else { 4 };
            for dx in 0..len {
                canvas.put_pixel(RULER_LEFT - 1 - dx, y as u32, INK);
            }
            if labeled {
                for x in RULER_LEFT..canvas.width() {
                    if blank(canvas.get_pixel(x, y as u32)) {
                        canvas.put_pixel(x, y as u32, GRID);
                    }
                }
                let n = t as u32;
                let gy = (y - 7 * i64::from(GLYPH_SCALE) / 2).clamp(0, i64::from(h) - 7 * i64::from(GLYPH_SCALE));
                draw_number(&mut canvas, n, i64::from(RULER_LEFT) - 12 - i64::from(number_width(n)), gy);
            }
        }
        t += i64::from(minor);
    }
    canvas
}

/// The reply's text: what's shown and how to read it.
pub fn reply_text(
    reply: &proto::RenderDrawingResponse,
    document: &str,
    crop: Rect,
    picture: &Picture,
) -> String {
    let mm = |m: f64| round(m * MM_PER_M, 1);
    let label = step_mm(picture.px_per_mm, 56.0);
    let mut text = format!(
        "{} of {document} ({} x {} mm) as SolidWorks prints it, x {}..{} mm, y {}..{} mm: {} x {} px, {:.2} \
         px per mm. The rulers (left, bottom) and the light blue grid lines (every {label} mm) are sheet \
         coordinates in mm from the lower-left corner -- the same as inspect_drawing's positions and \
         add_drawing_dimensions' at_mm.",
        reply.sheet_name,
        mm(reply.width),
        mm(reply.height),
        mm(crop.x0),
        mm(crop.x1),
        mm(crop.y0),
        mm(crop.y1),
        picture.width,
        picture.height,
        picture.px_per_mm,
    );
    if !reply.views.is_empty() {
        text += "\nViews on this sheet (outline, mm):";
        for v in &reply.views {
            if let Some(o) = outline_rect(v) {
                text += &format!("\n  {}: x {}..{}, y {}..{}", v.name, mm(o.x0), mm(o.x1), mm(o.y0), mm(o.y1));
            }
        }
    }
    if picture.distorted {
        text += "\nwarning: SolidWorks' image doesn't have the sheet's proportions, so the rulers are approximate";
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(width_px: u32, height_px: u32) -> (proto::RenderDrawingResponse, RgbImage) {
        let mut image = RgbImage::from_pixel(width_px, height_px, Rgb([255, 255, 255]));
        // A black pixel 100 mm right and 50 mm up of the lower-left corner
        // (sheet 420 x 297 mm).
        let x = (0.100 / 0.420 * f64::from(width_px)) as u32;
        let y = height_px - 1 - (0.050 / 0.297 * f64::from(height_px)) as u32;
        image.put_pixel(x, y, Rgb([0, 0, 0]));
        let view = proto::SheetViewOutline {
            name: "Drawing View1".into(),
            outline: Some(proto::Box3 {
                min: Some(proto::Vec3 { x: 0.08, y: 0.13, z: 0.0 }),
                max: Some(proto::Vec3 { x: 0.12, y: 0.17, z: 0.0 }),
            }),
        };
        let reply = proto::RenderDrawingResponse {
            sheet_name: "Sheet1".into(),
            width: 0.420,
            height: 0.297,
            dpi: 150,
            format: "png".into(),
            image: Vec::new(),
            views: vec![view],
            document_title: "Draw1".into(),
        };
        (reply, image)
    }

    #[test]
    fn crops_views_and_regions_within_the_sheet() {
        let (r, _) = reply(2480, 1754);
        let v = crop_rect(&r, Some("Drawing View1"), None).unwrap();
        assert!((v.x0 - 0.06).abs() < 1e-12 && (v.y1 - 0.19).abs() < 1e-12, "{v:?}");
        assert!(crop_rect(&r, Some("Drawing View9"), None).is_err());
        let p = RenderDrawingParams { region_mm: Some([[400.0, 280.0], [500.0, 350.0]]), ..Default::default() };
        let clipped = crop_rect(&r, None, region(&p).unwrap()).unwrap();
        assert!((clipped.x1 - 0.420).abs() < 1e-12 && (clipped.y1 - 0.297).abs() < 1e-12);
        let off = RenderDrawingParams { region_mm: Some([[500.0, 0.0], [600.0, 10.0]]), ..Default::default() };
        assert!(crop_rect(&r, None, region(&off).unwrap()).is_err());
        let tiny = RenderDrawingParams { region_mm: Some([[0.0, 0.0], [1.0, 1.0]]), ..Default::default() };
        assert!(region(&tiny).is_err());
    }

    #[test]
    fn picture_keeps_sheet_coordinates() {
        let (r, image) = reply(2480, 1754);
        // Region 90..110 x 40..60 mm: the black pixel lands in the middle.
        let crop = Rect { x0: 0.090, y0: 0.040, x1: 0.110, y1: 0.060 };
        let p = picture(&image, &r, crop, 1568).unwrap();
        assert!(!p.distorted);
        let decoded = image::load_from_memory(&p.png).unwrap().to_rgb8();
        assert_eq!((decoded.width(), decoded.height()), (p.width, p.height));
        let dark: Vec<(u32, u32)> = decoded
            .enumerate_pixels()
            .filter(|(x, y, px)| *x >= RULER_LEFT && *y < p.height - RULER_BOTTOM && px.0[0] < 100)
            .map(|(x, y, _)| (x - RULER_LEFT, y))
            .collect();
        assert_eq!(dark.len(), 1, "{dark:?}");
        let (w, h) = (p.width - RULER_LEFT, p.height - RULER_BOTTOM);
        let (dx, dy) = (f64::from(dark[0].0) / f64::from(w), f64::from(dark[0].1) / f64::from(h));
        assert!((dx - 0.5).abs() < 0.05 && (dy - 0.5).abs() < 0.05, "{dark:?} in {w} x {h}");
        // Whole sheet, scaled down to fit.
        let whole = Rect { x0: 0.0, y0: 0.0, x1: 0.420, y1: 0.297 };
        let p = picture(&image, &r, whole, 1000).unwrap();
        assert_eq!(p.width - RULER_LEFT, 1000);
        assert!((p.px_per_mm - 1000.0 / 420.0).abs() < 0.01);
    }

    #[test]
    fn shrinking_keeps_thin_lines_dark() {
        // A 1-px black line in a 1000 px image, shrunk to 300 px.
        let mut image = RgbImage::from_pixel(1000, 100, Rgb([255, 255, 255]));
        for x in 0..1000 {
            image.put_pixel(x, 50, Rgb([0, 0, 0]));
        }
        let small = shrink(&image, 300, 30);
        let darkest = (0..30).map(|y| small.get_pixel(150, y).0[0]).min().unwrap();
        assert!(darkest < 100, "{darkest}");
        assert_eq!(small.get_pixel(150, 5).0, [255, 255, 255]);
    }

    #[test]
    fn steps_and_resolution() {
        assert_eq!(step_mm(3.7, 6.0), 2);
        assert_eq!(step_mm(3.7, 56.0), 20);
        assert_eq!(step_mm(20.0, 56.0), 5);
        // Within what SolidWorks exports: 100..300 dpi.
        assert_eq!(dpi_for(0.300, 1568), 132);
        assert_eq!(dpi_for(0.100, 1568), MAX_DPI);
        assert_eq!(dpi_for(1.189, 1568), MIN_DPI);
        assert_eq!(dpi_for(0.010, 1568), MAX_DPI);
        assert!(!needs_other_dpi(1500.0, 1568) && needs_other_dpi(1000.0, 1568) && needs_other_dpi(2480.0, 1568));
        let p = RenderDrawingParams { max_px: Some(100), ..Default::default() };
        assert!(max_px(&p).is_err());
    }
}
