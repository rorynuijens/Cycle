//! The fullscreen ride HUD: the route cockpit's readings, drawn inside the 3D
//! view in the world's own style — cream stickers outlined like everything in
//! the world, with Fredoka numbers. Drawn as part of the world, it takes the
//! world's exemption from Adwaita's colours and type (CLAUDE.md §1.6); every
//! colour is named in [`crate::data::palette::HUD`].
//!
//! Laid out with Pango onto a Cairo image that the 3D view lays over its
//! frame. Painted when a reading changes — about once a second — never per
//! frame.

use std::path::PathBuf;
use std::sync::OnceLock;

use pango::prelude::*;

use crate::data::athlete::{power_zone_index, ZONE_COLORS};
use crate::data::palette::{Rgb, BASE, HUD};
use crate::data::route::Route;
use crate::ui::ride_card::format_ride_time;

/// Fredoka (SIL OFL 1.1, `data/fonts/`), carried in the binary so the HUD
/// looks the same in a dev build, the flatpak and anywhere else.
const FREDOKA: &[u8] = include_bytes!("../../../data/fonts/Fredoka.ttf");
const FAMILY: &str = "Fredoka, Adwaita Sans, sans-serif";

/// The HUD is laid out on a sheet this size and scaled to fit the view, so it
/// reads the same at any size; a wider view gets a wider sheet, not a bigger
/// HUD.
const SHEET_W: f64 = 1920.0;
const SHEET_H: f64 = 1080.0;

/// Gaps from the sheet's edges, and between stacked tiles.
const EDGE: f64 = 48.0;
const TOP: f64 = 32.0;
const STACK_GAP: f64 = 18.0;
const TILE_W: f64 = 236.0;
const TILE_H: f64 = 124.0;
const POWER_W: f64 = 420.0;
const BAND_H: f64 = 168.0;
/// The profile band's reading column.
const BAND_STATS_W: f64 = 210.0;

/// How many heights of the route the profile band is drawn from.
pub const PROFILE_SAMPLES: usize = 240;

/// What the HUD shows: the same readings as the route cockpit.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HudReadings {
    pub route_name: String,
    pub power_watts: Option<u32>,
    pub ftp_watts: u32,
    pub speed_kmh: f32,
    pub to_go_m: f32,
    pub heart_rate_bpm: Option<u32>,
    pub cadence_rpm: Option<u32>,
    pub gradient_pct: f32,
    pub elapsed_secs: u32,
    pub climbed_m: f32,
    /// How far along the route the rider is, 0–1.
    pub progress: f32,
    /// Racing a ghost: how far behind it the rider is, in seconds (negative
    /// ahead), and how far along the route it is, 0–1.
    pub ghost_gap_secs: Option<f32>,
    pub ghost_progress: Option<f32>,
}

/// The route's heights at [`PROFILE_SAMPLES`] even steps from start to finish,
/// for the profile band. Empty for a route with no length.
pub fn profile(route: &Route) -> Vec<f32> {
    let total = route.total_distance_m;
    if !(total.is_finite() && total > 0.0) || route.points.len() < 2 {
        return Vec::new();
    }
    (0..PROFILE_SAMPLES)
        .map(|k| route.elevation_at(total * k as f32 / (PROFILE_SAMPLES - 1) as f32))
        .collect()
}

/// The distance left, as a number and its unit: kilometres to one decimal,
/// and under a kilometre metres to the nearest ten — the last stretch is
/// where a rider counts.
pub fn to_go(metres: f32) -> (String, &'static str) {
    let m = if metres.is_finite() {
        metres.max(0.0)
    } else {
        0.0
    };
    let tens = (m / 10.0).round() * 10.0;
    if tens >= 1_000.0 {
        (format!("{:.1}", m / 1_000.0), "km")
    } else {
        (format!("{tens:.0}"), "m")
    }
}

/// "FINISH IN …" over the last stretch: the metres left as the to-go tile
/// counts them, never "0 m" while there is road left; nothing further out,
/// or once over the line.
pub fn finish_in(metres: f32) -> Option<String> {
    if !(metres.is_finite() && metres > 0.0 && metres <= FINISH_IN_M) {
        return None;
    }
    let (left, _) = to_go(metres.max(5.0));
    Some(format!("{left} m"))
}

/// The finish is announced from this far out, in metres.
pub const FINISH_IN_M: f32 = 500.0;

/// What the card over the road says once the rider is over the line.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedCard {
    pub route_name: String,
    pub elapsed_secs: u32,
    pub distance_m: f32,
    pub avg_power_watts: Option<u32>,
    pub climbed_m: f32,
    pub avg_heart_rate_bpm: Option<u32>,
}

impl FinishedCard {
    /// The numbers on the card, as `(caption, value, unit)`: only those the
    /// ride has, as on the ride card.
    pub fn tiles(&self) -> Vec<(&'static str, String, &'static str)> {
        let mut tiles = vec![
            ("TIME", format_ride_time(u64::from(self.elapsed_secs)), ""),
            (
                "DISTANCE",
                format!("{:.1}", self.distance_m.max(0.0) / 1_000.0),
                "km",
            ),
        ];
        if let Some(w) = self.avg_power_watts {
            tiles.push(("AVG POWER", w.to_string(), "W"));
        }
        if self.elapsed_secs > 0 {
            let kmh = self.distance_m.max(0.0) / self.elapsed_secs as f32 * 3.6;
            tiles.push(("AVG SPEED", format!("{kmh:.1}"), "km/h"));
        }
        tiles.push(("CLIMBED", format!("{:.0}", self.climbed_m.max(0.0)), "m"));
        if let Some(bpm) = self.avg_heart_rate_bpm {
            tiles.push(("AVG HEART RATE", bpm.to_string(), "bpm"));
        }
        tiles
    }
}

/// The finished card, on the sheet: its size, and where its button row sits
/// down from the card's top. The button itself is a real one, laid over the
/// view there.
const CARD_W: f64 = 940.0;
const CARD_H: f64 = 660.0;
const CARD_HEADER: f64 = 56.0;
const CARD_BUTTON_Y: f64 = 584.0;
const CARD_TILE_H: f64 = 106.0;

/// How far below the view's middle the finished card's button sits, in
/// pixels, for a view `width` × `height`.
pub fn finish_button_drop(width: i32, height: i32) -> f64 {
    let (scale, _, _) = sheet(width, height);
    (CARD_BUTTON_Y - CARD_H / 2.0) * scale
}

/// The gradient on the sign: a whole per cent, no "-0%" on the flat.
pub fn gradient(pct: f32) -> String {
    let whole = if pct.is_finite() { pct.round() } else { 0.0 };
    if whole == 0.0 {
        "0%".into()
    } else {
        format!("{whole:.0}%")
    }
}

/// The power zone the rider is in, 0–6, if there is power and an FTP.
pub fn zone(power_watts: Option<u32>, ftp_watts: u32) -> Option<usize> {
    let w = power_watts?;
    (ftp_watts > 0).then(|| power_zone_index(w, ftp_watts))
}

const ZONE_NAMES: [&str; 7] = [
    "Z1 · RECOVERY",
    "Z2 · ENDURANCE",
    "Z3 · TEMPO",
    "Z4 · THRESHOLD",
    "Z5 · VO₂ MAX",
    "Z6 · ANAEROBIC",
    "Z7 · SPRINT",
];

/// The sheet for a view `width` × `height` pixels: its scale to the view,
/// and its own size — [`SHEET_W`] × [`SHEET_H`] stretched to the view's
/// shape, never squashed.
pub fn sheet(width: i32, height: i32) -> (f64, f64, f64) {
    let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let scale = (w / SHEET_W).min(h / SHEET_H);
    (scale, w / scale, h / scale)
}

/// How far up from the bottom of a view `width` × `height` pixels anything
/// laid over the HUD must sit to stay clear of the profile band, in pixels.
pub fn controls_clearance(width: i32, height: i32) -> f64 {
    let (scale, _, _) = sheet(width, height);
    (TOP + BAND_H + 7.0 + 24.0) * scale
}

/// Lays the HUD out. Holds its own font map, with Fredoka in it, so nothing
/// outside the HUD sees the font.
pub struct RideHud {
    fonts: pango::FontMap,
}

impl Default for RideHud {
    fn default() -> Self {
        Self::new()
    }
}

impl RideHud {
    pub fn new() -> Self {
        let fonts = pangocairo::FontMap::new();
        match font_file() {
            Some(path) => {
                if let Err(e) = fonts.add_font_file(path) {
                    tracing::warn!("HUD font could not be loaded: {e}");
                }
            }
            None => tracing::warn!("HUD font could not be written to the cache"),
        }
        Self { fonts }
    }

    /// The HUD for a view `width` × `height` pixels, on a clear image.
    pub fn render(
        &self,
        width: i32,
        height: i32,
        r: &HudReadings,
        profile: &[f32],
    ) -> Option<cairo::ImageSurface> {
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, width.max(1), height.max(1)).ok()?;
        {
            let cr = cairo::Context::new(&surface).ok()?;
            self.paint(&cr, width, height, r, profile);
        }
        surface.flush();
        Some(surface)
    }

    fn paint(
        &self,
        cr: &cairo::Context,
        width: i32,
        height: i32,
        r: &HudReadings,
        profile: &[f32],
    ) {
        let (scale, w, h) = sheet(width, height);
        cr.scale(scale, scale);
        let pctx = self.fonts.create_context();
        pangocairo::functions::update_context(cr, &pctx);
        let t = Text { cr, pctx: &pctx };

        self.route_flag(&t, &r.route_name);

        let speed = format!("{:.1}", r.speed_kmh.max(0.0));
        tile(&t, EDGE, 112.0, "SPEED", None, &speed, "km/h", false);
        let (left, unit) = to_go(r.to_go_m);
        tile(
            &t,
            EDGE,
            112.0 + TILE_H + STACK_GAP,
            "TO GO",
            None,
            &left,
            unit,
            false,
        );

        let right = w - EDGE - TILE_W;
        let (hr, hr_dim) = reading(r.heart_rate_bpm);
        tile(
            &t,
            right,
            TOP,
            "HEART RATE",
            Some(heart),
            &hr,
            "bpm",
            hr_dim,
        );
        let (cad, cad_dim) = reading(r.cadence_rpm);
        tile(
            &t,
            right,
            TOP + TILE_H + STACK_GAP,
            "CADENCE",
            None,
            &cad,
            "rpm",
            cad_dim,
        );
        if let Some(gap) = r.ghost_gap_secs {
            ghost_tile(&t, right, TOP + 2.0 * (TILE_H + STACK_GAP), gap);
        }

        power_panel(&t, w, r);
        gradient_sign(&t, 60.0, h - 214.0 - 214.0, r.gradient_pct);
        profile_band(&t, w, h, r, profile);
        if let Some(left) = finish_in(r.to_go_m) {
            finish_banner(&t, w, &left);
        }
    }

    /// The card over the road once the rider is over the line, the world
    /// dimmed behind it, for a view `width` × `height`.
    pub fn render_finished(
        &self,
        width: i32,
        height: i32,
        card: &FinishedCard,
    ) -> Option<cairo::ImageSurface> {
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, width.max(1), height.max(1)).ok()?;
        {
            let cr = cairo::Context::new(&surface).ok()?;
            let (scale, w, h) = sheet(width, height);
            cr.scale(scale, scale);
            let pctx = self.fonts.create_context();
            pangocairo::functions::update_context(&cr, &pctx);
            let t = Text {
                cr: &cr,
                pctx: &pctx,
            };
            finished_card(&t, w, h, card);
        }
        surface.flush();
        Some(surface)
    }

    fn route_flag(&self, t: &Text, name: &str) {
        let label = t.layout(name, 22.0, pango::Weight::Semibold);
        let (tw, _) = label.size();
        let tw = f64::from(tw) / f64::from(pango::SCALE);
        let (x, y, hgt) = (EDGE, TOP, 52.0);
        sticker(
            t.cr,
            x,
            y,
            14.0 + 28.0 + 12.0 + tw + 20.0,
            hgt,
            18.0,
            5.0,
            5.0,
        );
        let cr = t.cr;
        // A pennant on a pole.
        let (px, py) = (x + 14.0 + 6.0, y + 12.0);
        set(cr, HUD.ink);
        cr.set_line_width(3.0);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.move_to(px, py + 24.0);
        cr.line_to(px, py + 2.0);
        let _ = cr.stroke();
        cr.move_to(px, py + 3.0);
        cr.line_to(px + 16.0, py + 3.0);
        cr.line_to(px + 12.0, py + 8.0);
        cr.line_to(px + 16.0, py + 13.0);
        cr.line_to(px, py + 13.0);
        cr.close_path();
        fill_and_outline(cr, HUD.flag, 2.5);
        t.draw(
            &label,
            x + 14.0 + 28.0 + 12.0,
            y + (hgt - 10.0 - 26.0) / 2.0 + 2.0,
            HUD.ink,
        );
    }
}

/// Writes Fredoka to the app's cache once, for Pango to load: it takes a
/// file, not bytes. The path is kept, so later HUDs load the same file.
fn font_file() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = glib::user_cache_dir().join("cycle").join("fonts");
        let path = dir.join("Fredoka.ttf");
        let current = std::fs::metadata(&path).is_ok_and(|m| m.len() == FREDOKA.len() as u64);
        if !current {
            if let Err(e) =
                std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, FREDOKA))
            {
                tracing::warn!("Could not write the HUD font: {e}");
                return None;
            }
        }
        Some(path)
    })
    .as_ref()
}

/// A reading's digits, or a dash — and whether it is the dash.
fn reading(value: Option<u32>) -> (String, bool) {
    match value {
        Some(v) => (v.to_string(), false),
        None => ("—".into(), true),
    }
}

/// Pango text on the HUD's Cairo context.
struct Text<'a> {
    cr: &'a cairo::Context,
    pctx: &'a pango::Context,
}

impl Text<'_> {
    fn layout(&self, text: &str, px: f64, weight: pango::Weight) -> pango::Layout {
        let layout = pango::Layout::new(self.pctx);
        let mut font = pango::FontDescription::from_string(FAMILY);
        font.set_absolute_size(px * f64::from(pango::SCALE));
        font.set_weight(weight);
        layout.set_font_description(Some(&font));
        // Numbers that change every second hold still: every digit one width.
        let attrs = pango::AttrList::new();
        attrs.insert(pango::AttrFontFeatures::new("tnum"));
        layout.set_attributes(Some(&attrs));
        layout.set_text(text);
        layout
    }

    /// A caption: small capitals, spaced out.
    fn caption(&self, text: &str) -> pango::Layout {
        let layout = self.layout(text, 18.0, pango::Weight::Semibold);
        let attrs = layout.attributes().unwrap_or_default();
        attrs.insert(pango::AttrInt::new_letter_spacing(
            (0.08 * 18.0 * f64::from(pango::SCALE)) as i32,
        ));
        layout.set_attributes(Some(&attrs));
        layout
    }

    fn width(layout: &pango::Layout) -> f64 {
        f64::from(layout.size().0) / f64::from(pango::SCALE)
    }

    fn baseline(layout: &pango::Layout) -> f64 {
        f64::from(layout.baseline()) / f64::from(pango::SCALE)
    }

    /// `layout` with its top left at `(x, y)`.
    fn draw(&self, layout: &pango::Layout, x: f64, y: f64, colour: Rgb) {
        set(self.cr, colour);
        self.cr.move_to(x, y);
        pangocairo::functions::show_layout(self.cr, layout);
    }

    /// `layout` sitting on the baseline `y`.
    fn on_baseline(&self, layout: &pango::Layout, x: f64, y: f64, colour: Rgb) {
        self.draw(layout, x, y - Self::baseline(layout), colour);
    }
}

fn set(cr: &cairo::Context, rgb: Rgb) {
    cr.set_source_rgb(f64::from(rgb[0]), f64::from(rgb[1]), f64::from(rgb[2]));
}

fn fill_and_outline(cr: &cairo::Context, face: Rgb, line: f64) {
    set(cr, face);
    let _ = cr.fill_preserve();
    set(cr, HUD.ink);
    cr.set_line_width(line);
    cr.set_line_join(cairo::LineJoin::Round);
    let _ = cr.stroke();
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    let pi = std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -pi / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, pi / 2.0);
    cr.arc(x + r, y + h - r, r, pi / 2.0, pi);
    cr.arc(x + r, y + r, r, pi, 1.5 * pi);
    cr.close_path();
}

/// A cream sticker: outlined like the world, on a hard shadow.
#[allow(clippy::too_many_arguments)]
fn sticker(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    radius: f64,
    border: f64,
    shadow: f64,
) {
    rounded(cr, x, y + shadow, w, h, radius);
    set(cr, HUD.ink);
    let _ = cr.fill();
    rounded(cr, x, y, w, h, radius);
    fill_and_outline(cr, HUD.panel, border);
}

/// A small icon drawn before a caption.
type Icon = fn(&cairo::Context, f64, f64);

fn heart(cr: &cairo::Context, x: f64, y: f64) {
    cr.move_to(x + 11.0, y + 18.5);
    cr.curve_to(x + 11.0, y + 18.5, x + 2.0, y + 13.0, x + 2.0, y + 6.8);
    cr.curve_to(x + 2.0, y + 1.0, x + 9.0, y + 0.0, x + 11.0, y + 4.6);
    cr.curve_to(x + 13.0, y + 0.0, x + 20.0, y + 1.0, x + 20.0, y + 6.8);
    cr.curve_to(x + 20.0, y + 13.0, x + 11.0, y + 18.5, x + 11.0, y + 18.5);
    cr.close_path();
    fill_and_outline(cr, HUD.heart, 2.5);
}

/// One reading on its own sticker: caption, number and unit.
#[allow(clippy::too_many_arguments)]
fn tile(
    t: &Text,
    x: f64,
    y: f64,
    caption: &str,
    icon: Option<Icon>,
    value: &str,
    unit: &str,
    dim: bool,
) {
    sticker(t.cr, x, y, TILE_W, TILE_H, 24.0, 5.0, 7.0);
    let mut cx = x + 22.0;
    if let Some(icon) = icon {
        icon(t.cr, cx, y + 18.0);
        cx += 30.0;
    }
    t.draw(&t.caption(caption), cx, y + 14.0, HUD.label);
    let number = t.layout(value, 68.0, pango::Weight::Bold);
    let base = y + TILE_H - 20.0;
    t.on_baseline(&number, x + 22.0, base, if dim { HUD.dim } else { HUD.ink });
    let unit = t.layout(unit, 24.0, pango::Weight::Semibold);
    t.on_baseline(
        &unit,
        x + 22.0 + Text::width(&number) + 8.0,
        base,
        HUD.label,
    );
}

/// The gap to the ghost, on its own sticker under the cadence: green ahead,
/// red behind.
fn ghost_tile(t: &Text, x: f64, y: f64, gap_secs: f32) {
    sticker(t.cr, x, y, TILE_W, TILE_H, 24.0, 5.0, 7.0);
    t.draw(&t.caption("GHOST"), x + 22.0, y + 14.0, HUD.label);
    let text = crate::data::ghost::format_gap(gap_secs);
    let colour = match text.chars().next() {
        Some('+') => HUD.sign,
        Some('−') => HUD.ridden,
        _ => HUD.ink,
    };
    let number = t.layout(&text, 68.0, pango::Weight::Bold);
    t.on_baseline(&number, x + 22.0, y + TILE_H - 20.0, colour);
}

fn zone_rgb(z: usize) -> Rgb {
    let (r, g, b) = ZONE_COLORS[z.min(ZONE_COLORS.len() - 1)];
    [r as f32, g as f32, b as f32]
}

/// Power, large, at the top in the middle: the zones it has reached lit in
/// their colours, and its zone named.
fn power_panel(t: &Text, w: f64, r: &HudReadings) {
    let cr = t.cr;
    let x = w / 2.0 - POWER_W / 2.0;
    let zone = zone(r.power_watts, r.ftp_watts);
    let height = if zone.is_some() { 286.0 } else { 236.0 };
    sticker(cr, x, TOP, POWER_W, height, 32.0, 6.0, 9.0);
    let caption = t.caption("POWER");
    t.draw(
        &caption,
        w / 2.0 - Text::width(&caption) / 2.0,
        TOP + 16.0,
        HUD.label,
    );
    let (digits, dim) = reading(r.power_watts);
    let number = t.layout(&digits, 132.0, pango::Weight::Bold);
    let unit = t.layout("W", 34.0, pango::Weight::Semibold);
    let span = Text::width(&number) + 10.0 + Text::width(&unit);
    let base = TOP + 158.0;
    let nx = w / 2.0 - span / 2.0;
    t.on_baseline(&number, nx, base, if dim { HUD.dim } else { HUD.ink });
    t.on_baseline(&unit, nx + Text::width(&number) + 10.0, base, HUD.label);

    // The seven zones, those reached lit.
    let inner = POWER_W - 56.0;
    let gap = 6.0;
    let seg = (inner - gap * 6.0) / 7.0;
    let mid = TOP + 192.0;
    for k in 0..7 {
        let here = zone == Some(k);
        let sh = if here { 22.0 } else { 14.0 };
        rounded(
            cr,
            x + 28.0 + k as f64 * (seg + gap),
            mid - sh / 2.0,
            seg,
            sh,
            6.0,
        );
        let lit = zone.is_some_and(|z| k <= z);
        fill_and_outline(
            cr,
            if lit { zone_rgb(k) } else { HUD.zone_off },
            if here { 4.0 } else { 3.0 },
        );
    }
    if let Some(z) = zone {
        let name = t.layout(ZONE_NAMES[z], 20.0, pango::Weight::Bold);
        let pw = Text::width(&name) + 32.0;
        rounded(cr, w / 2.0 - pw / 2.0, TOP + 218.0, pw, 38.0, 19.0);
        fill_and_outline(cr, zone_rgb(z), 4.0);
        // Dark on the light zones, light on the two darkest, so the name reads.
        let ink = if z >= 5 { HUD.panel } else { HUD.ink };
        t.draw(&name, w / 2.0 - Text::width(&name) / 2.0, TOP + 223.0, ink);
    }
}

/// A row of chequers across `x..x + w`, `square` high, starting dark.
fn chequers(cr: &cairo::Context, x: f64, y: f64, w: f64, rows: usize, square: f64) {
    set(cr, HUD.sign_face);
    cr.rectangle(x, y, w, rows as f64 * square);
    let _ = cr.fill();
    set(cr, BASE.chequer);
    for row in 0..rows {
        let mut cx = x + (row % 2) as f64 * square;
        while cx < x + w {
            cr.rectangle(cx, y + row as f64 * square, square.min(x + w - cx), square);
            cx += 2.0 * square;
        }
    }
    let _ = cr.fill();
}

/// "FINISH IN 180 m" on a yellow sticker with chequered ends, tilted a
/// little, under the power panel.
fn finish_banner(t: &Text, w: f64, left: &str) {
    let cr = t.cr;
    let words = t.layout("FINISH IN", 34.0, pango::Weight::Bold);
    let attrs = words.attributes().unwrap_or_default();
    attrs.insert(pango::AttrInt::new_letter_spacing(
        (0.08 * 34.0 * f64::from(pango::SCALE)) as i32,
    ));
    words.set_attributes(Some(&attrs));
    let number = t.layout(left, 44.0, pango::Weight::Bold);
    let (end, pad, gap, hgt) = (40.0, 26.0, 14.0, 70.0);
    let inner = Text::width(&words) + gap + Text::width(&number);
    let bw = 2.0 * end + 2.0 * pad + inner;
    let (cx, cy) = (w / 2.0, FINISH_BANNER_Y + hgt / 2.0);
    cr.save().ok();
    cr.translate(cx, cy);
    cr.rotate(-2f64.to_radians());
    let (x, y) = (-bw / 2.0, -hgt / 2.0);
    rounded(cr, x, y + 7.0, bw, hgt, 18.0);
    set(cr, HUD.ink);
    let _ = cr.fill();
    rounded(cr, x, y, bw, hgt, 18.0);
    cr.save().ok();
    cr.clip_preserve();
    set(cr, HUD.flag);
    let _ = cr.fill_preserve();
    cr.new_path();
    let square = hgt / 7.0;
    for ex in [x, x + bw - end] {
        for row in 0..7 {
            let k = row % 2;
            set(cr, if k == 0 { BASE.chequer } else { HUD.sign_face });
            cr.rectangle(ex, y + row as f64 * square, end / 2.0, square);
            let _ = cr.fill();
            set(cr, if k == 0 { HUD.sign_face } else { BASE.chequer });
            cr.rectangle(ex + end / 2.0, y + row as f64 * square, end / 2.0, square);
            let _ = cr.fill();
        }
    }
    cr.restore().ok();
    rounded(cr, x, y, bw, hgt, 18.0);
    set(cr, HUD.ink);
    cr.set_line_width(5.0);
    let _ = cr.stroke();
    let base = y + hgt / 2.0 + 15.0;
    let tx = x + end + pad;
    t.on_baseline(&words, tx, base, HUD.ink);
    t.on_baseline(&number, tx + Text::width(&words) + gap, base, HUD.ink);
    cr.restore().ok();
}

/// The top of the "FINISH IN" sticker, on the sheet: below the power panel.
const FINISH_BANNER_Y: f64 = 372.0;

/// The card once over the line: the world dimmed, a chequered header,
/// FINISHED!, the route, and the ride's numbers in tiles.
fn finished_card(t: &Text, w: f64, h: f64, card: &FinishedCard) {
    let cr = t.cr;
    set(cr, HUD.ink);
    cr.paint_with_alpha(0.38).ok();

    let (x, y) = (w / 2.0 - CARD_W / 2.0, h / 2.0 - CARD_H / 2.0);
    rounded(cr, x, y + 12.0, CARD_W, CARD_H, 36.0);
    set(cr, HUD.ink);
    let _ = cr.fill();
    rounded(cr, x, y, CARD_W, CARD_H, 36.0);
    set(cr, HUD.panel);
    let _ = cr.fill_preserve();
    cr.save().ok();
    cr.clip();
    chequers(cr, x, y, CARD_W, 2, CARD_HEADER / 2.0);
    set(cr, HUD.ink);
    cr.rectangle(x, y + CARD_HEADER, CARD_W, 6.0);
    let _ = cr.fill();
    cr.restore().ok();
    rounded(cr, x, y, CARD_W, CARD_H, 36.0);
    set(cr, HUD.ink);
    cr.set_line_width(6.0);
    let _ = cr.stroke();

    let title = t.layout("FINISHED!", 104.0, pango::Weight::Bold);
    let attrs = title.attributes().unwrap_or_default();
    attrs.insert(pango::AttrInt::new_letter_spacing(
        (0.04 * 104.0 * f64::from(pango::SCALE)) as i32,
    ));
    title.set_attributes(Some(&attrs));
    t.on_baseline(
        &title,
        w / 2.0 - Text::width(&title) / 2.0,
        y + 182.0,
        HUD.ink,
    );
    let route = t.layout(&card.route_name, 28.0, pango::Weight::Medium);
    route.set_width((CARD_W - 112.0) as i32 * pango::SCALE);
    route.set_ellipsize(pango::EllipsizeMode::End);
    t.on_baseline(
        &route,
        w / 2.0 - Text::width(&route) / 2.0,
        y + 226.0,
        HUD.label,
    );

    let tiles = card.tiles();
    let (cols, gap) = (3usize, 18.0);
    let tile_w = (CARD_W - 112.0 - gap * (cols as f64 - 1.0)) / cols as f64;
    for (k, (caption, value, unit)) in tiles.iter().enumerate() {
        let (row, col) = (k / cols, k % cols);
        // A short last row sits in the middle.
        let in_row = (tiles.len() - row * cols).min(cols) as f64;
        let row_w = in_row * tile_w + (in_row - 1.0) * gap;
        let tx = w / 2.0 - row_w / 2.0 + col as f64 * (tile_w + gap);
        let ty = y + 262.0 + row as f64 * (CARD_TILE_H + gap);
        sticker(cr, tx, ty, tile_w, CARD_TILE_H, 24.0, 5.0, 5.0);
        t.draw(&t.caption(caption), tx + 22.0, ty + 14.0, HUD.label);
        let number = t.layout(value, 56.0, pango::Weight::Bold);
        let base = ty + CARD_TILE_H - 18.0;
        t.on_baseline(&number, tx + 22.0, base, HUD.ink);
        if !unit.is_empty() {
            let u = t.layout(unit, 24.0, pango::Weight::Semibold);
            t.on_baseline(&u, tx + 22.0 + Text::width(&number) + 8.0, base, HUD.label);
        }
    }
}

/// The gradient as a road sign: a red-bordered triangle with the slope drawn
/// in it, falling the other way on a descent.
fn gradient_sign(t: &Text, x: f64, y: f64, pct: f32) {
    let cr = t.cr;
    rounded(cr, x + 80.0, y + 120.0, 16.0, 92.0, 4.0);
    fill_and_outline(cr, HUD.post, 4.0);
    cr.move_to(x + 88.0, y + 10.0);
    cr.line_to(x + 166.0, y + 146.0);
    cr.curve_to(
        x + 170.0,
        y + 154.0,
        x + 168.0,
        y + 154.0,
        x + 160.0,
        y + 154.0,
    );
    cr.line_to(x + 16.0, y + 154.0);
    cr.curve_to(x + 8.0, y + 154.0, x + 6.0, y + 154.0, x + 10.0, y + 146.0);
    cr.close_path();
    fill_and_outline(cr, HUD.sign_face, 5.0);
    cr.move_to(x + 88.0, y + 32.0);
    cr.line_to(x + 148.0, y + 138.0);
    cr.line_to(x + 28.0, y + 138.0);
    cr.close_path();
    set(cr, HUD.sign);
    cr.set_line_width(10.0);
    let _ = cr.stroke();
    // The slope: steeper with the grade, up to the sign's own.
    let rise = (f64::from(pct.abs()) * 3.0).clamp(2.0, 28.0);
    let (lo, hi) = (x + 52.0, x + 124.0);
    cr.move_to(lo, y + 128.0);
    cr.line_to(hi, y + 128.0);
    if pct >= 0.0 {
        cr.line_to(hi, y + 128.0 - rise);
    } else {
        cr.line_to(lo, y + 128.0 - rise);
    }
    cr.close_path();
    set(cr, HUD.ink);
    let _ = cr.fill();
    let label = t.layout(&gradient(pct), 30.0, pango::Weight::Bold);
    t.on_baseline(
        &label,
        x + 88.0 - Text::width(&label) / 2.0,
        y + 118.0 - rise.max(10.0),
        HUD.ink,
    );
}

/// The whole route as a grassy hill, the ridden part darker, the rider on it
/// and the finish flag at its end; elapsed time and the climb beside it.
fn profile_band(t: &Text, w: f64, h: f64, r: &HudReadings, profile: &[f32]) {
    let cr = t.cr;
    let (x, y) = (EDGE, h - TOP - BAND_H);
    let bw = w - 2.0 * EDGE;
    sticker(cr, x, y, bw, BAND_H, 24.0, 5.0, 7.0);

    let sx = x + 24.0;
    t.draw(&t.caption("ELAPSED"), sx, y + 18.0, HUD.label);
    let elapsed = t.layout(
        &format_ride_time(u64::from(r.elapsed_secs)),
        44.0,
        pango::Weight::Bold,
    );
    t.on_baseline(&elapsed, sx, y + 80.0, HUD.ink);
    t.draw(&t.caption("CLIMBED"), sx, y + 92.0, HUD.label);
    let climbed = t.layout(
        &format!("{:.0}", r.climbed_m.max(0.0)),
        44.0,
        pango::Weight::Bold,
    );
    t.on_baseline(&climbed, sx, y + 152.0, HUD.ink);
    let unit = t.layout("m", 20.0, pango::Weight::Semibold);
    t.on_baseline(
        &unit,
        sx + Text::width(&climbed) + 6.0,
        y + 152.0,
        HUD.label,
    );

    let (x0, x1) = (sx + BAND_STATS_W + 24.0, x + bw - 24.0);
    let (top, floor) = (y + 30.0, y + BAND_H - 18.0);
    let points = profile_points(profile, x0, x1, top, floor);
    if points.len() < 2 {
        return;
    }
    let outline = |cr: &cairo::Context| {
        cr.move_to(x0, floor);
        for &(px, py) in &points {
            cr.line_to(px, py);
        }
        cr.line_to(x1, floor);
        cr.close_path();
    };
    outline(cr);
    set(cr, HUD.profile);
    let _ = cr.fill();
    let progress = f64::from(r.progress.clamp(0.0, 1.0));
    let at = x0 + (x1 - x0) * progress;
    let _ = cr.save();
    cr.rectangle(x0, top - 40.0, at - x0, floor - top + 40.0);
    cr.clip();
    outline(cr);
    set(cr, HUD.ridden);
    let _ = cr.fill();
    let _ = cr.restore();
    cr.move_to(points[0].0, points[0].1);
    for &(px, py) in &points[1..] {
        cr.line_to(px, py);
    }
    set(cr, HUD.ink);
    cr.set_line_width(4.0);
    cr.set_line_join(cairo::LineJoin::Round);
    let _ = cr.stroke();
    cr.move_to(x0, floor);
    cr.line_to(x1, floor);
    let _ = cr.stroke();

    // The finish flag, chequered.
    let (fx, fy) = (x1 - 6.0, top - 14.0);
    cr.set_line_width(4.0);
    cr.move_to(fx, fy + 50.0);
    cr.line_to(fx, fy);
    let _ = cr.stroke();
    cr.rectangle(fx, fy, 30.0, 21.0);
    fill_and_outline(cr, HUD.sign_face, 3.0);
    set(cr, HUD.ink);
    for (i, j) in [(0, 0), (2, 0), (1, 1), (0, 2), (2, 2)] {
        cr.rectangle(fx + f64::from(i) * 10.0, fy + f64::from(j) * 7.0, 10.0, 7.0);
    }
    let _ = cr.fill();

    // The ghost, under the rider: a pale ball on a pin.
    if let Some(g) = r.ghost_progress {
        let gx = x0 + (x1 - x0) * f64::from(g.clamp(0.0, 1.0));
        let ground = height_at(&points, gx).unwrap_or(floor);
        set(cr, BASE.ghost_trim);
        cr.rectangle(gx - 2.5, ground - 24.0, 5.0, 24.0);
        let _ = cr.fill();
        cr.arc(gx, ground - 24.0 - 14.0, 14.0, 0.0, std::f64::consts::TAU);
        set(cr, BASE.ghost);
        let _ = cr.fill_preserve();
        set(cr, BASE.ghost_trim);
        cr.set_line_width(4.0);
        let _ = cr.stroke();
    }

    // The rider: a ball in their jersey's colour on a pin.
    let ground = height_at(&points, at).unwrap_or(floor);
    set(cr, HUD.ink);
    cr.rectangle(at - 2.5, ground - 24.0, 5.0, 24.0);
    let _ = cr.fill();
    cr.arc(
        at,
        ground + 4.0 - 24.0 - 17.0,
        17.0,
        0.0,
        std::f64::consts::TAU,
    );
    set(cr, HUD.ink);
    let _ = cr.fill();
    cr.arc(at, ground - 24.0 - 17.0, 17.0, 0.0, std::f64::consts::TAU);
    fill_and_outline(cr, HUD.marker, 5.0);
}

/// The profile's heights as points across `x0..x1`, the highest at `top` and
/// the lowest two thirds of the way down to `floor` — a flat route still
/// reads as land.
fn profile_points(profile: &[f32], x0: f64, x1: f64, top: f64, floor: f64) -> Vec<(f64, f64)> {
    let finite: Vec<f32> = profile.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.len() < 2 {
        return Vec::new();
    }
    let lo = finite.iter().copied().fold(f32::MAX, f32::min);
    let hi = finite.iter().copied().fold(f32::MIN, f32::max);
    let span = f64::from(hi - lo).max(1.0);
    let low_y = top + (floor - top) * 2.0 / 3.0;
    let n = finite.len() - 1;
    finite
        .iter()
        .enumerate()
        .map(|(k, &v)| {
            let px = x0 + (x1 - x0) * k as f64 / n as f64;
            let py = low_y - (low_y - top) * f64::from(v - lo) / span;
            (px, py)
        })
        .collect()
}

/// The profile's height at `x`, between its points.
fn height_at(points: &[(f64, f64)], x: f64) -> Option<f64> {
    let k = points.partition_point(|p| p.0 <= x);
    match (points.get(k.wrapping_sub(1)), points.get(k)) {
        (Some(a), Some(b)) if b.0 > a.0 => Some(a.1 + (b.1 - a.1) * (x - a.0) / (b.0 - a.0)),
        (Some(a), _) => Some(a.1),
        (None, Some(b)) => Some(b.1),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::route::RoutePoint;

    #[test]
    fn should_count_down_in_metres_only_under_a_kilometre() {
        assert_eq!(to_go(5_640.0), ("5.6".into(), "km"));
        assert_eq!(to_go(1_000.0), ("1.0".into(), "km"));
        // 995 m rounds to the next ten, a whole kilometre: say so in km.
        assert_eq!(to_go(995.0), ("1.0".into(), "km"));
        assert_eq!(to_go(994.9), ("990".into(), "m"));
        assert_eq!(to_go(184.0), ("180".into(), "m"));
        assert_eq!(to_go(4.0), ("0".into(), "m"));
        assert_eq!(to_go(-30.0), ("0".into(), "m"));
        assert_eq!(to_go(f32::NAN), ("0".into(), "m"));
    }

    #[test]
    fn should_announce_the_finish_only_over_the_last_500_m() {
        assert_eq!(finish_in(500.1), None);
        assert_eq!(finish_in(500.0), Some("500 m".into()));
        // The same metres as the to-go tile beside it.
        assert_eq!(finish_in(184.0), Some(format!("{} m", to_go(184.0).0)));
        // Never "0 m" with road still to ride; nothing once over the line.
        assert_eq!(finish_in(2.0), Some("10 m".into()));
        assert_eq!(finish_in(0.0), None);
        assert_eq!(finish_in(-3.0), None);
        assert_eq!(finish_in(f32::NAN), None);
    }

    fn finished() -> FinishedCard {
        FinishedCard {
            route_name: "Hill".into(),
            elapsed_secs: 1_908,
            distance_m: 10_120.0,
            avg_power_watts: Some(171),
            climbed_m: 142.4,
            avg_heart_rate_bpm: Some(148),
        }
    }

    #[test]
    fn should_card_every_number_the_ride_has_and_none_it_has_not() {
        let tiles = finished().tiles();
        let got: Vec<(&str, &str, &str)> =
            tiles.iter().map(|(c, v, u)| (*c, v.as_str(), *u)).collect();
        assert_eq!(
            got,
            vec![
                ("TIME", "31:48", ""),
                ("DISTANCE", "10.1", "km"),
                ("AVG POWER", "171", "W"),
                // 10.12 km in 1908 s.
                ("AVG SPEED", "19.1", "km/h"),
                ("CLIMBED", "142", "m"),
                ("AVG HEART RATE", "148", "bpm"),
            ]
        );
        let bare = FinishedCard {
            avg_power_watts: None,
            avg_heart_rate_bpm: None,
            elapsed_secs: 0,
            ..finished()
        };
        let captions: Vec<&str> = bare.tiles().iter().map(|t| t.0).collect();
        assert_eq!(captions, vec!["TIME", "DISTANCE", "CLIMBED"]);
    }

    #[test]
    fn should_sit_the_finish_button_on_the_cards_button_row() {
        assert_eq!(finish_button_drop(1920, 1080), 254.0);
        assert_eq!(finish_button_drop(3840, 2160), 508.0);
    }

    #[test]
    fn should_dim_the_world_round_the_finished_card() {
        let hud = RideHud::new();
        let mut s = hud
            .render_finished(1920, 1080, &finished())
            .expect("a card");
        let corner = pixel(&mut s, 20, 20);
        assert!(
            (90..=104).contains(&corner[3]),
            "the world dimmed by {}",
            corner[3]
        );
        // The card's face, between its first tiles and its edge.
        assert!(close(pixel(&mut s, 960 - 470 + 30, 540), rgb8(HUD.panel)));
        // Its chequered header: a dark square at the top left.
        assert!(close(
            pixel(&mut s, 960 - 470 + 20, 540 - 330 + 20),
            rgb8(BASE.chequer)
        ));
    }

    #[test]
    fn should_hang_the_finish_banner_out_only_near_the_finish() {
        let hud = RideHud::new();
        let near = HudReadings {
            to_go_m: 180.0,
            ..riding()
        };
        let mut s = hud.render(1920, 1080, &near, &[]).expect("a HUD");
        // The sticker's yellow, above its words.
        assert!(close(pixel(&mut s, 960, 381), rgb8(HUD.flag)));
        let mut s = hud.render(1920, 1080, &riding(), &[]).expect("a HUD");
        assert_eq!(pixel(&mut s, 960, 381)[3], 0);
    }

    #[test]
    fn should_sign_a_gradient_in_whole_per_cents_with_no_minus_zero() {
        assert_eq!(gradient(4.2), "4%");
        assert_eq!(gradient(4.5), "5%");
        assert_eq!(gradient(-0.4), "0%");
        assert_eq!(gradient(0.4), "0%");
        assert_eq!(gradient(-3.6), "-4%");
        assert_eq!(gradient(f32::INFINITY), "0%");
    }

    #[test]
    fn should_name_a_zone_only_with_power_and_an_ftp() {
        assert_eq!(zone(None, 211), None);
        assert_eq!(zone(Some(180), 0), None);
        // 211 W at an FTP of 211 is 100 %: threshold, Z4.
        assert_eq!(zone(Some(211), 211), Some(3));
    }

    #[test]
    fn should_scale_the_sheet_to_fit_and_widen_it_for_a_wide_view() {
        assert_eq!(sheet(1920, 1080), (1.0, 1920.0, 1080.0));
        assert_eq!(sheet(3840, 2160), (2.0, 1920.0, 1080.0));
        // An ultrawide gets a wider sheet at the same scale, not a bigger HUD.
        assert_eq!(sheet(2560, 1080), (1.0, 2560.0, 1080.0));
        let (s, w, h) = sheet(1280, 1024);
        assert!((s - 1280.0 / 1920.0).abs() < 1e-9);
        assert!((w - 1920.0).abs() < 1e-9 && h > 1080.0);
        // A view with no size yet is not a division by zero.
        assert!(sheet(0, 0).0.is_finite());
    }

    fn hill() -> Route {
        let pts: Vec<RoutePoint> = (0..=10)
            .map(|i| RoutePoint {
                lat: 52.0 + f64::from(i) * 0.001,
                lng: 6.0,
                elevation_m: if i <= 5 {
                    i as f32 * 10.0
                } else {
                    (10 - i) as f32 * 10.0
                },
                distance_m: i as f32 * 100.0,
                gradient: 0.0,
            })
            .collect();
        Route {
            name: "Hill".into(),
            total_distance_m: 1_000.0,
            total_gain_m: 50.0,
            points: pts,
        }
    }

    #[test]
    fn should_keep_the_controls_clear_of_the_profile_band() {
        assert_eq!(controls_clearance(1920, 1080), 231.0);
        assert_eq!(controls_clearance(3840, 2160), 462.0);
    }

    #[test]
    fn should_sample_the_profile_from_start_to_finish() {
        let p = profile(&hill());
        assert_eq!(p.len(), PROFILE_SAMPLES);
        assert_eq!(p[0], 0.0);
        assert_eq!(p[PROFILE_SAMPLES - 1], 0.0);
        let top = p.iter().copied().fold(f32::MIN, f32::max);
        assert!((top - 50.0).abs() < 0.5, "the top is at {top}");
    }

    #[test]
    fn should_draw_a_flat_route_as_land_not_a_line_on_the_floor() {
        let pts = profile_points(&[3.0; 10], 0.0, 900.0, 0.0, 90.0);
        assert!(pts.iter().all(|p| (p.1 - 60.0).abs() < 1e-9), "{pts:?}");
        let pts = profile_points(&[0.0, 50.0, 0.0], 0.0, 100.0, 0.0, 90.0);
        assert_eq!(pts, vec![(0.0, 60.0), (50.0, 0.0), (100.0, 60.0)]);
        assert!(profile_points(&[f32::NAN, 1.0], 0.0, 1.0, 0.0, 1.0).is_empty());
    }

    #[test]
    fn should_find_the_profile_height_between_its_points() {
        let pts = [(0.0, 60.0), (50.0, 0.0), (100.0, 60.0)];
        assert_eq!(height_at(&pts, 25.0), Some(30.0));
        assert_eq!(height_at(&pts, 100.0), Some(60.0));
        assert_eq!(height_at(&pts, -5.0), Some(60.0));
        assert_eq!(height_at(&pts, 500.0), Some(60.0));
        assert_eq!(height_at(&[], 1.0), None);
    }

    fn pixel(s: &mut cairo::ImageSurface, x: i32, y: i32) -> [u8; 4] {
        let stride = s.stride() as usize;
        let data = s.data().expect("the surface's pixels");
        let i = y as usize * stride + x as usize * 4;
        // Cairo's ARGB32 lies in memory as B, G, R, A on a little-endian host.
        [data[i + 2], data[i + 1], data[i], data[i + 3]]
    }

    fn rgb8(c: Rgb) -> [u8; 3] {
        c.map(|v| (v * 255.0).round() as u8)
    }

    fn close(a: [u8; 4], b: [u8; 3]) -> bool {
        a[..3].iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 2) && a[3] == 255
    }

    fn riding() -> HudReadings {
        HudReadings {
            route_name: "Hill".into(),
            power_watts: Some(180),
            ftp_watts: 211,
            speed_kmh: 27.4,
            to_go_m: 5_600.0,
            heart_rate_bpm: Some(142),
            cadence_rpm: None,
            gradient_pct: 4.2,
            elapsed_secs: 872,
            climbed_m: 62.0,
            progress: 0.44,
            ghost_gap_secs: None,
            ghost_progress: None,
        }
    }

    #[test]
    fn should_show_the_ghost_only_while_racing_one() {
        let hud = RideHud::new();
        // The ghost tile's face, under the cadence tile on the right.
        let (x, y) = (
            1920 - 48 - 236 + 160,
            (TOP + 2.0 * (TILE_H + STACK_GAP) + 20.0) as i32,
        );
        let mut s = hud.render(1920, 1080, &riding(), &[]).expect("a HUD");
        assert_eq!(pixel(&mut s, x, y)[3], 0, "no ghost, no tile");
        let racing = HudReadings {
            ghost_gap_secs: Some(-8.0),
            ghost_progress: Some(0.4),
            ..riding()
        };
        let mut s = hud.render(1920, 1080, &racing, &[]).expect("a HUD");
        assert!(close(pixel(&mut s, x, y), rgb8(HUD.panel)));
    }

    /// Writes the HUD to a PNG to look at:
    ///
    /// ```text
    /// CYCLE_HUD_OUT=/tmp/hud.png cargo test render_hud_png -- --ignored
    /// ```
    #[test]
    #[ignore]
    fn render_hud_png() {
        let out = std::env::var("CYCLE_HUD_OUT").expect("CYCLE_HUD_OUT");
        let hud = RideHud::new();
        // CYCLE_HUD_TO_GO=<metres> for the finish banner; CYCLE_HUD_FINISHED=1
        // for the card.
        let to_go_m = std::env::var("CYCLE_HUD_TO_GO")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5_600.0);
        let s = if std::env::var("CYCLE_HUD_FINISHED").is_ok_and(|v| v == "1") {
            hud.render_finished(1920, 1080, &finished())
        } else {
            let r = HudReadings {
                to_go_m,
                ..riding()
            };
            hud.render(1920, 1080, &r, &profile(&hill()))
        }
        .expect("a HUD");
        let mut file = std::fs::File::create(out).expect("the PNG file");
        s.write_to_png(&mut file).expect("written");
    }

    #[test]
    fn should_leave_the_world_showing_between_the_stickers() {
        let hud = RideHud::new();
        let mut s = hud
            .render(1920, 1080, &riding(), &profile(&hill()))
            .expect("a HUD");
        // The middle of the view is the road: nothing drawn there.
        assert_eq!(pixel(&mut s, 960, 600)[3], 0);
        assert_eq!(pixel(&mut s, 400, 500)[3], 0);
        // The power panel's face, top middle, and a tile's, top left.
        assert!(close(pixel(&mut s, 960, 40), rgb8(HUD.panel)));
        assert!(close(pixel(&mut s, 270, 150), rgb8(HUD.panel)));
        // The profile band along the bottom.
        assert!(close(pixel(&mut s, 70, 1040), rgb8(HUD.panel)));
    }

    #[test]
    fn should_light_the_zones_up_to_the_riders_and_no_further() {
        let hud = RideHud::new();
        let mut s = hud.render(1920, 1080, &riding(), &[]).expect("a HUD");
        // 180 W at 211 is 85 %: tempo, Z3. Segment centres along the bar.
        let seg = (POWER_W - 56.0 - 36.0) / 7.0;
        let at =
            |k: usize| (960.0 - POWER_W / 2.0 + 28.0 + k as f64 * (seg + 6.0) + seg / 2.0) as i32;
        let y = (TOP + 192.0) as i32;
        assert!(close(pixel(&mut s, at(0), y), rgb8(zone_rgb(0))));
        assert!(close(pixel(&mut s, at(2), y), rgb8(zone_rgb(2))));
        assert!(close(pixel(&mut s, at(3), y), rgb8(HUD.zone_off)));
        assert!(close(pixel(&mut s, at(6), y), rgb8(HUD.zone_off)));
    }

    #[test]
    fn should_mark_the_ridden_part_of_the_profile() {
        let hud = RideHud::new();
        let flat = vec![10.0; PROFILE_SAMPLES];
        let mut s = hud.render(1920, 1080, &riding(), &flat).expect("a HUD");
        // Under the flat profile's line, just above the floor: ridden to the
        // left of the rider, still to ride to the right.
        let x0 = EDGE + 24.0 + BAND_STATS_W + 24.0;
        let x1 = 1920.0 - EDGE - 24.0;
        let y = (1080.0 - TOP - 18.0 - 10.0) as i32;
        let before = (x0 + (x1 - x0) * 0.2) as i32;
        let after = (x0 + (x1 - x0) * 0.8) as i32;
        assert!(close(pixel(&mut s, before, y), rgb8(HUD.ridden)));
        assert!(close(pixel(&mut s, after, y), rgb8(HUD.profile)));
    }
}
