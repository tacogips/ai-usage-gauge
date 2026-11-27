//! Platform-independent template tray artwork.
use ai_usage_core::types::{MenuBarStatusEntry, PieChartId, RateLimitProvider};
use std::io::Cursor;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};
const N: usize = 36;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedTrayIcon {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone)]
pub struct TrayRenderState {
    pub entries: Vec<MenuBarStatusEntry>,
    pub is_usage_unavailable: bool,
    pub has_warnings: bool,
}
impl PartialEq for TrayRenderState {
    fn eq(&self, b: &Self) -> bool {
        self.is_usage_unavailable == b.is_usage_unavailable
            && self.has_warnings == b.has_warnings
            && self.entries.len() == b.entries.len()
            && self.entries.iter().zip(&b.entries).all(|(a, b)| {
                a.id == b.id
                    && a.fraction.map(f64::to_bits) == b.fraction.map(f64::to_bits)
                    && a.show_icon == b.show_icon
                    && a.ratio_label == b.ratio_label
                    && a.is_budget_unset == b.is_budget_unset
                    && a.is_budget_unknown == b.is_budget_unknown
            })
    }
}
impl Eq for TrayRenderState {}
#[derive(Clone)]
pub struct Artwork {
    pub solid: Vec<u8>,
    pub interior: Vec<u8>,
    pub outline: Vec<u8>,
    pub cumulative_interior_by_row: Vec<f64>,
}
impl Artwork {
    fn new(solid: Vec<u8>) -> Self {
        let mut interior = vec![0; N * N];
        for y in 1..N - 1 {
            for x in 1..N - 1 {
                let mut m = 255;
                for yy in y - 1..=y + 1 {
                    for xx in x - 1..=x + 1 {
                        m = m.min(solid[yy * N + xx]);
                    }
                }
                interior[y * N + x] = m;
            }
        }
        let outline = solid
            .iter()
            .zip(&interior)
            .map(|(s, i)| s.saturating_sub(*i))
            .collect();
        let mut rows: Vec<f64> = (0..N)
            .rev()
            .map(|y| {
                interior[y * N..(y + 1) * N]
                    .iter()
                    .map(|v| f64::from(*v))
                    .sum()
            })
            .collect();
        let total: f64 = rows.iter().sum();
        let mut sum = 0.0;
        for row in &mut rows {
            sum += *row;
            *row = if total > 0.0 { sum / total } else { 0.0 };
        }
        Self {
            solid,
            interior,
            outline,
            cumulative_interior_by_row: rows,
        }
    }
    pub fn fill_height_fraction(&self, fraction: f64) -> f64 {
        let f = normalize(fraction);
        if f == 0.0 || f == 1.0 {
            return f;
        }
        let mut prev = 0.0;
        for (i, &v) in self.cumulative_interior_by_row.iter().enumerate() {
            if v >= f {
                return (i as f64 + (f - prev) / (v - prev)) / N as f64;
            }
            prev = v;
        }
        f
    }
    fn render(&self, fraction: Option<f64>) -> Vec<u8> {
        let mut a = self.outline.clone();
        if let Some(f) = fraction {
            let h = self.fill_height_fraction(f) * N as f64;
            for y in 0..N {
                let c = (h - (N - 1 - y) as f64).clamp(0.0, 1.0);
                for x in 0..N {
                    let i = y * N + x;
                    a[i] = a[i].max((f64::from(self.solid[i]) * c).round() as u8);
                }
            }
        } else {
            for y in 4..32 {
                for x in 4..32 {
                    if (x as i32 + y as i32 - 35).abs() <= 1 {
                        a[y * N + x] = 255;
                    }
                }
            }
        }
        a
    }
}
fn normalize(f: f64) -> f64 {
    if f.is_finite() {
        f.clamp(0.0, 1.0)
    } else {
        0.0
    }
}
fn png_art(data: &[u8]) -> Artwork {
    let mut d = png::Decoder::new(Cursor::new(data));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = d.read_info().expect("PNG header");
    let mut b = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut b).expect("PNG frame");
    let c = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::GrayscaleAlpha => 2,
        _ => panic!("PNG alpha"),
    };
    let (w, h) = (info.width as usize, info.height as usize);
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        for x in 0..w {
            if b[(y * w + x) * c + c - 1] > 5 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    let mut s = vec![0; N * N];
    for y in 0..N {
        for x in 0..N {
            let mut sum = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x0 + (x * 4 + sx) * (x1 - x0 + 1) / (N * 4);
                    let py = y0 + (y * 4 + sy) * (y1 - y0 + 1) / (N * 4);
                    sum += u32::from(b[(py * w + px) * c + c - 1]);
                }
            }
            s[y * N + x] = (sum / 16) as u8;
        }
    }
    Artwork::new(s)
}
fn gemini_art() -> Artwork {
    let mut p = PathBuilder::new();
    p.move_to(18.0, 35.0);
    p.cubic_to(21.0, 25.0, 25.0, 21.0, 35.0, 18.0);
    p.cubic_to(25.0, 15.0, 21.0, 11.0, 18.0, 1.0);
    p.cubic_to(15.0, 11.0, 11.0, 15.0, 1.0, 18.0);
    p.cubic_to(11.0, 21.0, 15.0, 25.0, 18.0, 35.0);
    p.close();
    let mut pix = Pixmap::new(36, 36).unwrap();
    let mut paint = Paint::default();
    paint.set_color_rgba8(0, 0, 0, 255);
    pix.fill_path(
        &p.finish().unwrap(),
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    Artwork::new(pix.data().as_chunks::<4>().0.iter().map(|p| p[3]).collect())
}
pub struct TrayIconRenderer {
    pub claude: Artwork,
    pub codex: Artwork,
    pub gemini: Artwork,
    warning_mask: Vec<u8>,
}
impl Default for TrayIconRenderer {
    fn default() -> Self {
        Self::new()
    }
}
impl TrayIconRenderer {
    pub fn new() -> Self {
        Self {
            claude: png_art(include_bytes!("../assets/claude-template.png")),
            codex: png_art(include_bytes!("../assets/chatgpt-template.png")),
            gemini: gemini_art(),
            warning_mask: render_warning_mask(),
        }
    }
    pub fn render_strip(&self, state: &TrayRenderState) -> Option<RenderedTrayIcon> {
        let mut entries: Vec<_> = state
            .entries
            .iter()
            .filter(|e| e.show_icon || e.ratio_label.is_some())
            .collect();
        entries.sort_by_key(|e| PieChartId::ALL.iter().position(|id| *id == e.id).unwrap());
        let width = entries
            .iter()
            .map(|e| {
                usize::from(e.show_icon) * N
                    + e.ratio_label
                        .as_ref()
                        .map_or(0, |s| 6 + s.chars().count() * 12)
            })
            .sum::<usize>()
            + entries.len().saturating_sub(1) * 14;
        if width == 0 {
            return None;
        }
        let mut rgba = vec![0; width * N * 4];
        let mut x = 0;
        for e in entries {
            if e.show_icon {
                let alpha_owned;
                let alpha: &[u8] = if e.id == PieChartId::Budget {
                    if state.is_usage_unavailable || state.has_warnings {
                        &self.warning_mask
                    } else {
                        alpha_owned = budget(e);
                        &alpha_owned
                    }
                } else {
                    alpha_owned = match e.id.provider() {
                        Some(RateLimitProvider::Claude) => &self.claude,
                        Some(RateLimitProvider::Gemini) => &self.gemini,
                        _ => &self.codex,
                    }
                    .render(e.fraction);
                    &alpha_owned
                };
                for y in 0..N {
                    for col in 0..N {
                        rgba[(y * width + x + col) * 4 + 3] = alpha[y * N + col];
                    }
                }
                x += N;
            }
            if let Some(label) = &e.ratio_label {
                x += 6;
                for ch in label.chars() {
                    glyph(&mut rgba, width, x, ch);
                    x += 12;
                }
            }
            x += 14;
        }
        Some(RenderedTrayIcon {
            rgba,
            width: width as u32,
            height: N as u32,
        })
    }
}
fn render_warning_mask() -> Vec<u8> {
    let mut out = vec![0; N * N];
    for y in 0..N {
        for x in 0..N {
            let mut sum = 0.0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x as f64 + (sx as f64 + 0.5) / 4.0;
                    let py = y as f64 + (sy as f64 + 0.5) / 4.0;
                    let edge = |ax: f64, ay: f64, bx: f64, by: f64| {
                        let dx = bx - ax;
                        let dy = by - ay;
                        let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy))
                            .clamp(0.0, 1.0);
                        ((px - ax - t * dx).powi(2) + (py - ay - t * dy).powi(2)).sqrt() < 1.5
                    };
                    if edge(18.0, 3.0, 34.0, 33.0)
                        || edge(34.0, 33.0, 2.0, 33.0)
                        || edge(2.0, 33.0, 18.0, 3.0)
                        || (px - 18.0).abs() < 1.6 && (15.0..26.0).contains(&py)
                        || (px - 18.0).abs() < 1.8 && (28.0..32.0).contains(&py)
                    {
                        sum += 1.0;
                    }
                }
            }
            out[y * N + x] = (sum / 16.0 * 255.0_f64).round() as u8;
        }
    }
    out
}

fn budget(e: &MenuBarStatusEntry) -> Vec<u8> {
    let f = if e.is_budget_unset {
        0.0
    } else if e.is_budget_unknown {
        0.25
    } else {
        normalize(e.fraction.unwrap_or(0.0))
    };
    let mut out = vec![0; N * N];
    for y in 0..N {
        for x in 0..N {
            let mut sum = 0.0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x as f64 + (sx as f64 + 0.5) / 4.0;
                    let py = y as f64 + (sy as f64 + 0.5) / 4.0;
                    let dx = px - 18.0;
                    let dy = py - 18.0;
                    if dx * dx + dy * dy <= 196.0 {
                        let angle = dx.atan2(-dy).rem_euclid(std::f64::consts::TAU)
                            / std::f64::consts::TAU;
                        sum += if angle < f || e.is_budget_unset && dx.abs() < 1.5 && dy.abs() < 10.0
                        {
                            1.0
                        } else {
                            0.35
                        };
                    }
                }
            }
            out[y * N + x] = (sum / 16.0 * 255.0_f64).round() as u8;
        }
    }
    out
}
fn glyph(rgba: &mut [u8], width: usize, x: usize, ch: char) {
    let rows: [u8; 7] = match ch {
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        '%' => [17, 2, 4, 8, 17, 0, 0],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '.' => [0, 0, 0, 0, 0, 12, 12],
        _ => [0; 7],
    };
    for (y, row) in rows.iter().enumerate() {
        for col in 0..5 {
            if row & (1 << (4 - col)) != 0 {
                for dy in 0..2 {
                    for dx in 0..2 {
                        rgba[((10 + y * 2 + dy) * width + x + col * 2 + dx) * 4 + 3] =
                            if dx == 0 { 192 } else { 255 };
                    }
                }
            }
        }
    }
}
#[derive(Default)]
pub struct TrayPresentationDedup {
    icon: Option<TrayRenderState>,
    title: Option<String>,
    tooltip: Option<String>,
}
impl TrayPresentationDedup {
    pub fn update_icon(&mut self, v: &TrayRenderState) -> bool {
        if self.icon.as_ref() == Some(v) {
            false
        } else {
            self.icon = Some(v.clone());
            true
        }
    }
    pub fn update_title(&mut self, v: &str) -> bool {
        if self.title.as_deref() == Some(v) {
            false
        } else {
            self.title = Some(v.to_owned());
            true
        }
    }
    pub fn update_tooltip(&mut self, v: &str) -> bool {
        if self.tooltip.as_deref() == Some(v) {
            false
        } else {
            self.tooltip = Some(v.to_owned());
            true
        }
    }
}
