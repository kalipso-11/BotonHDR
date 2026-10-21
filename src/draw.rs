//! Render del gadget, tip e icono de bandeja.
//!
//! Por qué no GDI+: la `gdiplus.dll` de este SO (Win 10.0.26300) es un
//! STUB — sus funciones devuelven Ok pero no pintan nada. Por eso el
//! render es 100% por software: composición por píxel en Rust sobre un
//! DIB premultiplied (SDF para formas, antialiasing de 1 px) y GDI puro
//! (`DrawTextW` en un DIB scratch) solo para rasterizar el texto con AA.
//!
//! Formato: el buffer es 32 bpp premultiplied ARGB (BGRA en memoria),
//! que es exactamente lo que piden `UpdateLayeredWindow(ULW_ALPHA)` y
//! los íconos de 32 bits: cero conversiones.
//!
//! Forma y opacidad (verificado píxel a píxel en vivo, Win11
//! 10.0.26300): el alpha per-píxel del ULW es REAL (bandas
//! α=255/128/64/16 se componen a 100/50/25/6 % de opacidad). PERO
//! `SetLayeredWindowAttributes(LWA_ALPHA)` NO lo preserva: aplana la
//! superficie contra negro antes de aplicar bAlpha (medido en vivo: con
//! bAlpha=20 % el wallpaper de las zonas α=0 salía a ~80 % de brillo).
//! Por eso el buffer se pinta con AA premultiplied natural — la forma
//! (octágono, pill, tip) vive en el propio buffer, sin SetWindowRgn — y
//! la opacidad global (20–100 %) se HORNEA en el buffer con
//! `Scanbuf::apply_opacity` antes de cada ULW (escala los canales
//! premultiplied: α=0 sigue 0 → transparencia real, sin oscurecer).
//!
//! Geometría: todo se calcula en px FÍSICOS del buffer (la ventana ya
//! está en px físicos: tamaño lógico × dpi/96). `scale` solo entra en
//! los pisos/offsets lógicos (p. ej. "13 px" del cartelito).

use crate::win as w;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{LazyLock, Mutex};

// ---------------------------------------------------------------------------
// Paleta y tamaños
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Color {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl Color {
    fn hex(s: &str) -> Self {
        let s = s.trim_start_matches('#');
        let b = s.as_bytes();
        Color {
            r: u8::from_str_radix(std::str::from_utf8(&b[0..2]).unwrap_or("00"), 16).unwrap_or(0),
            g: u8::from_str_radix(std::str::from_utf8(&b[2..4]).unwrap_or("00"), 16).unwrap_or(0),
            b: u8::from_str_radix(std::str::from_utf8(&b[4..6]).unwrap_or("00"), 16).unwrap_or(0),
            a: 255,
        }
    }
    fn alpha(self, a: u8) -> Color {
        Color { a, ..self }
    }
    /// Equivale a QColor::darker(118): cada canal / 1.18.
    fn dark(self) -> Color {
        let f = 100.0 / 118.0;
        Color {
            r: (self.r as f32 * f).round() as u8,
            g: (self.g as f32 * f).round() as u8,
            b: (self.b as f32 * f).round() as u8,
            a: 255,
        }
    }
}

/// Nombres de la paleta + hex (mismo orden que el menú "Color").
pub const COLORS: [(&str, &str); 7] = [
    ("Cian", "#22d3ee"),
    ("Verde", "#34d399"),
    ("Amarillo", "#facc15"),
    ("Naranja", "#fb923c"),
    ("Rojo", "#f87171"),
    ("Violeta", "#a78bfa"),
    ("Blanco", "#f1f5f9"),
];

/// Tamaños lógicos (ventana w×h).
pub const SIZES: [(&str, (i32, i32)); 4] = [
    ("Chico", (80, 100)),
    ("Medio", (96, 120)),
    ("Grande", (120, 150)),
    ("Enorme", (160, 200)),
];
pub const DEFAULT_SIZE: &str = "Grande";

pub fn size_of(name: &str) -> (i32, i32) {
    SIZES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
        .unwrap_or((120, 150))
}

/// Geometría del octágono para una ventana física de w×h px:
/// `(cx, cy, radio, paso)` (`paso` ya no se usa: el halo se eliminó
/// porque en modo color-key se veía como anillo negro; se conserva el
/// retorno para no romper call sites).
pub fn octagon_geom(w: i32, h: i32) -> (f32, f32, f32, i32) {
    let r = w as f32 * 0.383;
    let step = (r / 23.0).round() as i32;
    (w as f32 / 2.0, h as f32 - w as f32 * 0.49, r, step.max(2))
}

/// Vértices (px físicos de la ventana) del octágono. Mismos vértices que
/// el SDF `oct_field` (22.5° + k·45°, radio `r`). (Histórico: se usaban
/// para SetWindowRgn; hoy la forma vive en el canal alpha del buffer —
/// ver doc del módulo.)
pub fn gadget_octagon_points(w: i32, h: i32) -> [w::POINT; 8] {
    let (cx, cy, r, _) = octagon_geom(w, h);
    let mut pts = [w::POINT { x: 0, y: 0 }; 8];
    for (k, p) in pts.iter_mut().enumerate() {
        let ang = std::f32::consts::FRAC_PI_8 + (k as f32) * std::f32::consts::FRAC_PI_4;
        p.x = (cx + r * ang.cos()).round() as i32;
        p.y = (cy + r * ang.sin()).round() as i32;
    }
    pts
}

/// Polígono que aproxima un rectángulo redondeado (x, y, w, h, rad) con
/// 8 segmentos por cuarto de circunferencia. Recorrido horario desde
/// arriba-izq (en pantalla y crece hacia abajo, así los ángulos también
/// crecen). (Histórico: se usaba para las regiones de pill/tip; hoy solo
/// lo usan los tests: la forma vive en el canal alpha del buffer.)
pub fn rounded_rect_points(x: f32, y: f32, w: f32, h: f32, rad: f32) -> Vec<w::POINT> {
    let r = rad.min(w / 2.0).min(h / 2.0).max(0.0);
    let segs = 8usize;
    let mut pts: Vec<w::POINT> = Vec::with_capacity(4 * (segs + 1));
    // Esquinas: (centro_x, centro_y, ángulo inicial del cuarto). Con
    // (ccx + r·cos a, ccy + r·sin a) y y-hacia-abajo, el ángulo creciente
    // recorre el círculo en sentido horario (reloj). Cada cuarto va de
    // un lado del rectángulo al siguiente, compartiendo extremos:
    let corners: [(f32, f32, f32); 4] = [
        (x + r, y + r, std::f32::consts::PI), // izq→arriba (9h→12h)
        (x + w - r, y + r, 1.5 * std::f32::consts::PI), // arriba→der (12h→3h)
        (x + w - r, y + h - r, 2.0 * std::f32::consts::PI), // der→abajo (3h→6h)
        (x + r, y + h - r, 3.0 * std::f32::consts::PI), // abajo→izq (6h→9h)
    ];
    for (ccx, ccy, a0) in corners {
        for s in 0..=segs {
            let a = a0 + (s as f32 / segs as f32) * std::f32::consts::FRAC_PI_2;
            pts.push(w::POINT {
                x: (ccx + r * a.cos()).round() as i32,
                y: (ccy + r * a.sin()).round() as i32,
            });
        }
    }
    pts
}

/// Geometría de la forma del gadget: polígono del octágono + polígono
/// del pill de warning si hay cartelito (la pill vive en el margen
/// superior transparente, fuera del octágono). (Histórico: se usaba para
/// SetWindowRgn; hoy solo lo usan los tests — la forma vive en el canal
/// alpha del buffer.)
pub fn gadget_region_points(
    w: i32,
    h: i32,
    scale: f32,
    warning: Option<&str>,
) -> ([w::POINT; 8], Option<Vec<w::POINT>>) {
    let oct = gadget_octagon_points(w, h);
    let pill = warning.and_then(|text| {
        warning_pill_rect(w, h, scale, text)
            .map(|(lx, ly, lw, lh, _wpx)| {
                rounded_rect_points(lx, ly, lw, lh, (lh / 2.0).max(1.0))
            })
    });
    (oct, pill)
}

// ---------------------------------------------------------------------------
// Geometría por SDF (signed distance fields)
// ---------------------------------------------------------------------------

const A45: f32 = std::f32::consts::FRAC_PI_4; // 45°
const A22: f32 = std::f32::consts::FRAC_PI_4 / 2.0; // 22.5°
const COS22: f32 = 0.923_879_5; // cos(22.5°)

/// SDF del octágono regular (vértices a 22.5°+k·45°, radio `r`):
/// negativo dentro, 0 en el borde, positivo afuera. El borde tiene AA
/// natural de ~1 px al usarlo como `clamp(0.5 - f, 0, 1)`.
fn oct_field(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> f32 {
    let dx = x - cx;
    let dy = y - cy;
    let dist = (dx * dx + dy * dy).sqrt();
    if dist < 1e-6 {
        return -r;
    }
    let theta = dy.atan2(dx);
    let m = theta.rem_euclid(A45);
    // Distancia angular al eje del lado más cercano (0 = punto medio del
    // lado, ±22.5° = vértice).
    let a = if m <= A22 { m } else { m - A45 };
    dist - r * (COS22 / a.cos())
}

/// SDF de un rectángulo redondeado centrado en (cx, cy) de semi-extensos
/// (hw, hh) y radio `rad`. Negativo dentro, 0 en el borde, positivo
/// afuera.
///
/// Ojo: el término interior es `min(max(qx, qy), 0)` (fórmula estándar de
/// IQ). Con `min(min(qx, qy), 0)` el SDF queda más negativo en la banda de
/// los lados rectos y la forma se "hinchaba" hacia afuera: en el candado
/// el cuerpo filtraba blanco hacia arriba y rellenaba el hueco del arco.
fn rrect_field(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, rad: f32) -> f32 {
    let qx = (x - cx).abs() - (hw - rad);
    let qy = (y - cy).abs() - (hh - rad);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - rad
}

#[inline]
fn lerp8(a: u8, b: u8, t: f32) -> u8 {
    ((a as f32) + (b as f32 - a as f32) * t).round() as u8
}

/// Vista sobre un buffer 32 bpp premultiplied (BGRA en memoria).
#[derive(Clone, Copy)]
struct Px {
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Px {
    fn new(bits: *mut u8, w: i32, h: i32) -> Self {
        Px { bits, w, h }
    }
    #[inline]
    fn idx(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return None;
        }
        Some((y as usize * self.w as usize + x as usize) * 4)
    }
    /// Pinta el píxel (reemplazo). Canales ya premultiplied por `a`.
    #[inline]
    fn set(&self, x: i32, y: i32, b: u8, g: u8, r: u8, a: u8) {
        if let Some(i) = self.idx(x, y) {
            unsafe {
                let p = self.bits.add(i);
                p.write(b);
                p.add(1).write(g);
                p.add(2).write(r);
                p.add(3).write(a);
            }
        }
    }
    /// Composición "over" premultiplied: dst = src + dst·(1 − src_a).
    #[inline]
    fn over(&self, x: i32, y: i32, sb: u8, sg: u8, sr: u8, sa: u8) {
        if sa == 0 {
            return;
        }
        if let Some(i) = self.idx(x, y) {
            unsafe {
                let p = self.bits.add(i);
                let k = 255 - sa as i32;
                let db = (p.read() as i32 * k) / 255;
                let dg = (p.add(1).read() as i32 * k) / 255;
                let dr = (p.add(2).read() as i32 * k) / 255;
                let da = (p.add(3).read() as i32 * k) / 255;
                p.write((db + sb as i32).clamp(0, 255) as u8);
                p.add(1).write((dg + sg as i32).clamp(0, 255) as u8);
                p.add(2).write((dr + sr as i32).clamp(0, 255) as u8);
                p.add(3).write((da + sa as i32).clamp(0, 255) as u8);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// DIB destino (UpdateLayeredWindow consume el HBITMAP directamente)
// ---------------------------------------------------------------------------

/// Sección DIB de 32 bpp top-down; `bits` es premultiplied BGRA en memoria.
/// El `hbm` debe mantenerse vivo mientras se use el buffer (p. ej. como
/// fuente de `UpdateLayeredWindow`).
pub struct Scanbuf {
    hbm: *mut c_void,
    pub bits: *mut u8,
    pub w: i32,
    pub h: i32,
}

impl Scanbuf {
    /// HBITMAP subyacente (fuente directa de `UpdateLayeredWindow`).
    pub fn hbm(&self) -> *mut c_void {
        self.hbm
    }

    /// Opacidad global (0–100) aplicada EN EL MISMO buffer premultiplied:
    /// multiplica los 4 canales de cada píxel por pct/100. Como el buffer
    /// es premultiplied, escalar los 4 canales preserva la
    /// premultiplicación. α=0 sigue 0 (transparencia real) y los píxeles
    /// opacos se atenúan hacia el fondo. Es la opacidad REAL del gadget:
    /// `SetLayeredWindowAttributes` NO se usa porque aplana la superficie
    /// contra negro (oscurece las zonas transparentes).
    pub fn apply_opacity(&self, pct: i32) {
        let p = (pct as u16).clamp(0, 100);
        if p == 100 {
            return;
        }
        let n = (self.w as usize) * (self.h as usize) * 4;
        unsafe {
            for i in 0..n {
                *self.bits.add(i) = (*self.bits.add(i) as u16 * p / 100) as u8;
            }
        }
    }
    pub fn new(w: i32, h: i32) -> Option<Self> {
        unsafe {
            let mut info: w::BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<w::BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = w;
            info.bmiHeader.biHeight = -h; // top-down
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = w::BI_RGB;
            let mut bits: *mut u8 = std::ptr::null_mut();
            let hbm = w::CreateDIBSection(
                std::ptr::null_mut(),
                &info,
                w::DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            if hbm.is_null() || bits.is_null() {
                return None;
            }
            std::ptr::write_bytes(bits, 0, w as usize * h as usize * 4);
            Some(Scanbuf { hbm, bits, w, h })
        }
    }
}

impl Drop for Scanbuf {
    fn drop(&mut self) {
        unsafe {
            w::DeleteObject(self.hbm);
        }
    }
}

// ---------------------------------------------------------------------------
// Texto con GDI (raster AA en DIB scratch)
// ---------------------------------------------------------------------------
// GDI tiene afinidad de hilo: DC/HFONT se crean y destruyen DENTRO de cada
// llamada de render (el worker de toggle también pinta el icono de bandeja).
// Solo se cachean NÚMEROS (InkBox / tamaño), que son Send+Sync.

/// Fuente GDI: `CreateFontW` con altura = px (negativo = por celda),
/// calidad antialiased. Bahnschrift → Segoe UI.
fn make_font(px: i32, bold: bool) -> Option<*mut c_void> {
    let weight = if bold { w::FW_BOLD } else { w::FW_NORMAL };
    for name in ["Bahnschrift", "Segoe UI"] {
        let u = w::utf16(name);
        let f = unsafe {
            w::CreateFontW(
                -(px.max(1)),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                1, // DEFAULT_CHARSET
                0, // OUT_TT_PRECIS
                0, // CLIP_DEFAULT_PRECIS
                w::ANTIALIASED_QUALITY,
                0, // DEFAULT_PITCH | FF_DONTCARE
                u.as_ptr(),
            )
        };
        if !f.is_null() {
            return Some(f);
        }
    }
    None
}

/// DIB 32bpp scratch + DC listo para texto blanco sobre transparente.
struct TextScratch {
    hbm: *mut c_void,
    dc: *mut c_void,
    bits: *mut u8,
    w: i32,
    h: i32,
    old: *mut c_void, // objeto seleccionado al crear (se restaura en Drop)
}

impl TextScratch {
    fn new(w: i32, h: i32) -> Option<Self> {
        unsafe {
            let mut info: w::BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<w::BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = w.max(1);
            info.bmiHeader.biHeight = -(h.max(1));
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = w::BI_RGB;
            let mut bits: *mut u8 = std::ptr::null_mut();
            let hbm = w::CreateDIBSection(
                std::ptr::null_mut(),
                &info,
                w::DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            if hbm.is_null() || bits.is_null() {
                return None;
            }
            std::ptr::write_bytes(bits, 0, w.max(1) as usize * h.max(1) as usize * 4);
            let dc = w::CreateCompatibleDC(std::ptr::null_mut());
            if dc.is_null() {
                w::DeleteObject(hbm);
                return None;
            }
            let old = w::SelectObject(dc, hbm);
            w::SetBkMode(dc, w::TRANSPARENT);
            w::SetBkColor(dc, 0);
            // CRÍTICO: GDI pinta negro por defecto; el texto debe ser blanco.
            w::SetTextColor(dc, 0x00FFFFFF);
            Some(TextScratch {
                hbm,
                dc,
                bits,
                w: w.max(1),
                h: h.max(1),
                old,
            })
        }
    }

    /// Coverage del píxel del texto blanco: max(b,g,r). GDI escribe
    /// A=0x00 donde pinta y preserva los bytes afuera.
    #[inline]
    fn cover(&self, x: i32, y: i32) -> u8 {
        let i = (y as usize * self.w as usize + x as usize) * 4;
        let p = unsafe { self.bits.add(i) };
        unsafe { p.read().max(p.add(1).read()).max(p.add(2).read()) }
    }

    /// Rasteriza `text` (blanco) con la fuente de `px`. Devuelve la caja
    /// de tinta REAL (x, y, w, h en i32) en coordenadas absolutas del
    /// scratch (0,0 = esquina sup. izq. de la sección DIB).
    fn draw(&self, px: i32, bold: bool, text: &str) -> InkBox {
        let font = match make_font(px, bold) {
            Some(f) => f,
            None => return InkBox::EMPTY,
        };
        let u = w::utf16(text);
        let ox = 16i32;
        let oy = self.h / 2;
        let mut rect = w::RECT {
            left: ox,
            top: oy,
            right: ox + 4096,
            bottom: oy + 4096,
        };
        unsafe {
            let _ = w::SelectObject(self.dc, font);
            let _ = w::DrawTextW(
                self.dc,
                u.as_ptr(),
                (u.len() - 1) as i32,
                &mut rect,
                w::DT_LEFT | w::DT_SINGLELINE,
            );
            w::SelectObject(self.dc, self.old);
            w::DeleteObject(font);
        }
        scan_ink(self)
    }
}

impl Drop for TextScratch {
    fn drop(&mut self) {
        unsafe {
            if !self.old.is_null() {
                w::SelectObject(self.dc, self.old);
            }
            w::DeleteDC(self.dc);
            w::DeleteObject(self.hbm);
        }
    }
}

/// Caja de tinta `(x, y, w, h)` en coordenadas absolutas del scratch
/// (0,0 = esquina sup. izq. de la sección DIB; x a la derecha, y abajo).
#[derive(Clone, Copy, PartialEq)]
struct InkBox {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl InkBox {
    const EMPTY: InkBox = InkBox { x: 0, y: 0, w: 0, h: 0 };
}

fn scan_ink(scr: &TextScratch) -> InkBox {
    let mut minx: i32 = i32::MAX;
    let mut miny: i32 = i32::MAX;
    let mut maxx: i32 = -1;
    let mut maxy: i32 = -1;
    for y in 0..scr.h {
        for x in 0..scr.w {
            if scr.cover(x, y) > 8 {
                if x < minx {
                    minx = x;
                }
                if x > maxx {
                    maxx = x;
                }
                if y < miny {
                    miny = y;
                }
                if y > maxy {
                    maxy = y;
                }
            }
        }
    }
    if maxx < 0 {
        InkBox::EMPTY
    } else {
        InkBox {
            x: minx,
            y: miny,
            w: maxx - minx + 1,
            h: maxy - miny + 1,
        }
    }
}

static INK: LazyLock<Mutex<HashMap<(i32, bool, String), InkBox>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static TSIZE: LazyLock<Mutex<HashMap<(i32, bool, String), (f32, f32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Ancho×alto (px) de la caja de layout del texto con la fuente pedida.
pub fn text_size(px: i32, bold: bool, text: &str) -> (f32, f32) {
    let key = (px, bold, text.to_string());
    if let Some(hit) = TSIZE.lock().unwrap().get(&key) {
        return *hit;
    }
    let out = measure_raw(px, bold, text);
    TSIZE.lock().unwrap().insert(key, out);
    out
}

fn measure_raw(px: i32, bold: bool, text: &str) -> (f32, f32) {
    let font = match make_font(px, bold) {
        Some(f) => f,
        None => return (0.0, 0.0),
    };
    let scr = match TextScratch::new(1, 1) {
        Some(s) => s,
        None => {
            unsafe {
                w::DeleteObject(font);
            }
            return (0.0, 0.0);
        }
    };
    let u = w::utf16(text);
    let mut sz: w::SIZE;
    unsafe {
        sz = std::mem::zeroed();
        let _ = w::SelectObject(scr.dc, font);
        let _ = w::GetTextExtentPoint32W(
            scr.dc,
            u.as_ptr(),
            (u.len() - 1) as i32,
            &mut sz,
        );
        w::SelectObject(scr.dc, scr.old);
        w::DeleteObject(font);
    }
    (sz.cx as f32, sz.cy as f32)
}

/// Caja de tinta cacheada para (px, bold, text).
fn ink_bbox(px: i32, bold: bool, text: &str) -> InkBox {
    let key = (px, bold, text.to_string());
    if let Some(hit) = INK.lock().unwrap().get(&key) {
        return *hit;
    }
    let out = ink_measure_raw(px, bold, text);
    INK.lock().unwrap().insert(key, out);
    out
}

fn ink_measure_raw(px: i32, bold: bool, text: &str) -> InkBox {
    let (fw, _fh) = text_size(px, bold, text);
    let w = (fw * 1.25).ceil() as i32 + 48;
    let h = (px * 2).max(16) + 32;
    match TextScratch::new(w, h) {
        Some(scr) => scr.draw(px, bold, text),
        None => InkBox::EMPTY,
    }
}

/// Texto ya rasterizado (blanco) en su scratch, listo para componerse
/// con cualquier tinte.
struct RasterText {
    scr: TextScratch,
    ink: InkBox,
}

impl RasterText {
    fn new(px: i32, bold: bool, text: &str) -> Option<Self> {
        let ink = ink_bbox(px, bold, text);
        if ink.w <= 0 || ink.h <= 0 {
            return None;
        }
        let (fw, _fh) = text_size(px, bold, text);
        let w = (fw * 1.25).ceil() as i32 + 48;
        let h = (px * 2).max(16) + 32;
        let scr = TextScratch::new(w, h)?;
        let ink = scr.draw(px, bold, text);
        if ink.w <= 0 || ink.h <= 0 {
            return None;
        }
        Some(RasterText { scr, ink })
    }

    /// Composición en `dst` con tinte RGB (premultiplied por coverage).
    /// El centro de la tinta cae en (dx, dy).
    fn blit_center(&self, dst: &Px, dx: i32, dy: i32, tint: (u8, u8, u8)) {
        let offx = dx - self.ink.x - self.ink.w / 2;
        let offy = dy - self.ink.y - self.ink.h / 2;
        self.blit_at(dst, offx, offy, tint);
    }

    /// Composición en `dst` con la esquina sup. izq. de la tinta en (dx, dy).
    fn blit_topleft(&self, dst: &Px, dx: i32, dy: i32, tint: (u8, u8, u8)) {
        self.blit_at(dst, dx - self.ink.x, dy - self.ink.y, tint);
    }

    /// Composición centrada con estirado VERTICAL: `stretch_y > 1` alarga
    /// la tinta hacia arriba y abajo sin cambiar el ancho (pedido del
    /// usuario 2026-10-21: el texto de la bandeja "un poco más estirado").
    /// La cobertura se muestrea bilineal en Y (sin escalonado).
    ///
    /// Ojo (bug del primer intento, 2026-10-21): la tinta NO empieza en
    /// (0,0) de la scratch (p. ej. y=101 de 164); los offsets de
    /// `blit_center` (`dx - ink.x - ink.w/2`) son relativos a la esquina
    /// sup-izq de la scratch, no de la tinta. Acá se itera directamente
    /// sobre filas/columnas DESTINO y se mapea a la fuente:
    ///   - X: columna destino n ↔ píxel exacto `ink.x + n` de la fuente.
    ///   - Y: fila destino n ↔ `ink.y + (n+0.5)/s − 0.5` (centro de
    ///     píxel; la última fila destino muestrea la última de la fuente).
    fn blit_center_stretched(
        &self,
        dst: &Px,
        dx: i32,
        dy: i32,
        tint: (u8, u8, u8),
        stretch_y: f32,
    ) {
        if stretch_y <= 1.0 {
            self.blit_center(dst, dx, dy, tint);
            return;
        }
        let ih2 = ((self.ink.h as f32) * stretch_y).round().max(1.0) as i32;
        let s = ih2 as f32 / self.ink.h as f32;
        let dst_x0 = dx - self.ink.w / 2;
        let dst_y0 = dy - ih2 / 2;
        for y in dst_y0..dst_y0 + ih2 {
            let fy = self.ink.y as f32 + ((y - dst_y0) as f32 + 0.5) / s - 0.5;
            for x in dst_x0..dst_x0 + self.ink.w {
                let fx = (x - dst_x0 + self.ink.x) as f32;
                let cov = self.sample_cov(fx, fy);
                if cov == 0 {
                    continue;
                }
                // Mismo cuidado de desborde u8 que blit_at.
                let b = (tint.2 as u32 * cov as u32 / 255) as u8;
                let g = (tint.1 as u32 * cov as u32 / 255) as u8;
                let r = (tint.0 as u32 * cov as u32 / 255) as u8;
                dst.over(x, y, b, g, r, cov);
            }
        }
    }

    /// Cobertura bilineal en coords de fuente (x en píxel entero = índice
    /// directo; y fraccional por el estirado). Fuera de la scratch → 0.
    fn sample_cov(&self, fx: f32, fy: f32) -> u8 {
        let x0 = fx.floor() as i32;
        let y0 = fy.floor() as i32;
        let tx = (fx - x0 as f32).clamp(0.0, 1.0);
        let ty = (fy - y0 as f32).clamp(0.0, 1.0);
        let g = |x: i32, y: i32| -> f32 {
            if x < 0 || y < 0 || x >= self.scr.w || y >= self.scr.h {
                0.0
            } else {
                self.scr.cover(x, y) as f32
            }
        };
        let top = g(x0, y0) * (1.0 - tx) + g(x0 + 1, y0) * tx;
        let bot = g(x0, y0 + 1) * (1.0 - tx) + g(x0 + 1, y0 + 1) * tx;
        (top * (1.0 - ty) + bot * ty).round().max(0.0).min(255.0) as u8
    }

    fn blit_at(&self, dst: &Px, offx: i32, offy: i32, tint: (u8, u8, u8)) {
        let x0 = self.ink.x.max(0);
        let y0 = self.ink.y.max(0);
        let x1 = (self.ink.x + self.ink.w).min(self.scr.w).max(0);
        let y1 = (self.ink.y + self.ink.h).min(self.scr.h).max(0);
        for y in y0..y1 {
            for x in x0..x1 {
                let cov = self.scr.cover(x, y);
                if cov == 0 {
                    continue;
                }
                // Ojo: en u8, `tint * cov` desborda (release: wrap
                // silencioso) y el texto salía con RGB=0 (invisible).
                let b = (tint.2 as u32 * cov as u32 / 255) as u8;
                let g = (tint.1 as u32 * cov as u32 / 255) as u8;
                let r = (tint.0 as u32 * cov as u32 / 255) as u8;
                dst.over(offx + x, offy + y, b, g, r, cov);
            }
        }
    }
}

/// Mayor tamaño (≤ start_px, piso 9) donde `text` cabe en max_w×max_h.
fn fit_px(start_px: i32, bold: bool, text: &str, max_w: f32, max_h: f32) -> i32 {
    let mut px = start_px.max(9);
    while px > 9 {
        let (fw, fh) = text_size(px, bold, text);
        if fw > 0.0 && fw <= max_w && fh <= max_h {
            return px;
        }
        px -= 1;
    }
    9
}

// ---------------------------------------------------------------------------
// Candado (bloqueo de estado): SDF + blit
// ---------------------------------------------------------------------------

/// Coverage 0..1 (AA ~1 px) del candado CLÁSICO (silueta tipo candado
/// tradicional: cuerpo redondeado gordo, arco delgado, ojo de cerradura
/// con tallo ensanchado). `s` = ancho del cuerpo; origen (0,0) = tope
/// centro del CUERPO.
/// - Cuerpo: rect redondeado (alto 0.82s, radio 0.24s) bajo el origen:
///   gordo y con las esquinas claramente redondeadas.
/// - Cerradura: banda anular (radio 0.25s, grosor 0.12s) centrada en
///   (0, -0.08s), solo sobre el tope del cuerpo (ly <= 0): el arco sube
///   0.39s y las patas caen sobre el tope (el borde exterior de la pata
///   entra en la curva de la esquina del cuerpo: sin notch).
/// - Ojo: CÍRCULO (r 0.09s, piso 1.2 px, centro (0, 0.30s)) + TALLO
///   fino que se ensancha levemente (medio-ancho 0.035s → 0.055s, piso
///   0.5 px, hasta 0.50s), RESTADO del coverage: el hueco deja ver el
///   fondo (compuesto con "over", el recorte no se pinta). El tallo
///   queda ~45 % del diámetro del círculo: silueta clásica "ojo", no
///   una barra.
fn padlock_cov(lx: f32, ly: f32, s: f32) -> f32 {
    let bh2 = s * 0.41;
    let body = rrect_field(lx, ly, 0.0, bh2, s / 2.0, bh2, s * 0.24);
    // Cuerpo RELENADO: coverage = -SDF recortado (1 adentro, 0 afuera,
    // AA ~1 px).
    let mut lock = (-body).clamp(0.0, 1.0);
    // Cerradura: banda anular cortada en el tope del cuerpo (las patas
    // terminan ahí, sin hueco: el cuerpo completa la unión).
    if ly <= 0.0 {
        let dy = ly + s * 0.08;
        let d = (lx * lx + dy * dy).sqrt();
        let rc = s * 0.25;
        let ts = s * 0.12;
        let ring = (ts / 2.0 - (d - rc).abs() + 0.5).clamp(0.0, 1.0);
        lock = lock.max(ring);
    }
    // Ojo de cerradura clásico (recorte): círculo + tallo ensanchado.
    let kcy = s * 0.30;
    let kr = (s * 0.09).max(1.2);
    let dkc = (lx * lx + (ly - kcy) * (ly - kcy)).sqrt();
    let keyc = (kr - dkc + 0.5).clamp(0.0, 1.0);
    let y0 = s * 0.30;
    let y1 = s * 0.50;
    let mut key = keyc;
    if ly > y0 - 0.5 && ly < y1 + 0.5 {
        let t = ((ly - y0) / (y1 - y0)).clamp(0.0, 1.0);
        let hw = (s * 0.035 + s * 0.020 * t).max(0.5);
        key = key.max((hw - lx.abs() + 0.5).clamp(0.0, 1.0));
    }
    (lock - key).clamp(0.0, 1.0)
}

/// Pinta el candado en `color` (composición premultiplied "over") con
/// el tope-centro del cuerpo en (ox, oy).
fn draw_padlock(dst: &Px, ox: f32, oy: f32, s: f32, color: Color) {
    let x0 = (ox - s / 2.0 - 1.0).floor() as i32;
    let x1 = (ox + s / 2.0 + 1.0).ceil() as i32;
    let y0 = (oy - 0.39 * s - 1.0).floor() as i32;
    let y1 = (oy + 0.82 * s + 1.0).ceil() as i32;
    for y in y0..=y1 {
        if y < 0 || y >= dst.h {
            continue;
        }
        for x in x0..=x1 {
            if x < 0 || x >= dst.w {
                continue;
            }
            let cov = padlock_cov(x as f32 - ox, y as f32 - oy, s);
            if cov <= 0.0 {
                continue;
            }
            let a = (cov * 255.0).round() as u8;
            dst.over(
                x,
                y,
                (color.b as u32 * a as u32 / 255) as u8,
                (color.g as u32 * a as u32 / 255) as u8,
                (color.r as u32 * a as u32 / 255) as u8,
                a,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Gadget: octágono + badge (+ cartelito de advertencia)
// ---------------------------------------------------------------------------

/// Rect del cartelito de warning para `w`×`h` y texto `warn`:
/// `(lx, ly, lw, lh en px físicos, wpx = fuente final en px)`. MISMA
/// fórmula que lo usa el render (sección g) y la región
/// (`gadget_region_points`): una sola fuente de verdad.
fn warning_pill_rect(w: i32, h: i32, scale: f32, warn: &str) -> Option<(f32, f32, f32, f32, i32)> {
    let (cx, cy, r, _) = octagon_geom(w, h);
    let max_w = w as f32 - 8.0 * scale;
    let max_h = 12.0_f32.max((cy - r) - 8.0 * scale);
    let minpx = (5.0 * scale).round() as i32;
    let mut wpx = (13.0 * scale).round() as i32;
    let (mut lw, mut lh) = {
        let (fw, fh) = text_size(wpx, true, warn);
        (
            fw.ceil() as i32 + (10.0 * scale).round() as i32,
            fh.ceil() as i32 + (8.0 * scale).round() as i32,
        )
    };
    loop {
        if (lw as f32 <= max_w && lh as f32 <= max_h) || wpx <= minpx || wpx <= 1 {
            break;
        }
        wpx -= 1;
        let (fw, fh) = text_size(wpx, true, warn);
        lw = fw.ceil() as i32 + (10.0 * scale).round() as i32;
        lh = fh.ceil() as i32 + (8.0 * scale).round() as i32;
    }
    let lw = lw as f32;
    let lh = lh as f32;
    let lx = cx - lw / 2.0;
    let ly = (2.0 * scale).max((cy - r) - 6.0 * scale - lh);
    Some((lx, ly, lw, lh, wpx))
}

/// Render completo del gadget en `buf` (BGRA en memoria, de `w`×`h` px
/// físicos). `scale` = dpi/96 del monitor donde vive.
///
/// El buffer es premultiplied con alpha REAL por píxel (verificado con
/// probes: el compositor compone los α parciales de verdad): el cuerpo va
/// opaco (α=255), los contornos/glows llevan α parcial premultiplied y
/// lo de afuera de la forma queda α=0 → la ventana muestra exactamente lo
/// del buffer. La opacidad global (20–100 %) se hornea después con
/// `Scanbuf::apply_opacity` (worker de paint): transparente real, sin
/// oscurecer (SLWA NO se usa: aplana la superficie contra negro).
pub fn render_gadget_into(
    buf: &mut Scanbuf,
    scale: f32,
    state: &str,
    accent: &str,
    warning: Option<&str>,
    locked: bool,
) -> bool {
    let w = buf.w;
    let h = buf.h;
    if w <= 0 || h <= 0 {
        return false;
    }
    unsafe {
        std::ptr::write_bytes(buf.bits, 0, w as usize * h as usize * 4);
    }
    let dst = Px::new(buf.bits, w, h);

    let is_hdr = state == "HDR";
    let accent_c = Color::hex(accent);
    let glow = if is_hdr { accent_c } else { accent_c.dark() };
    let (cx, cy, r, _step) = octagon_geom(w, h); // px físicos

    // --- (a) halo: ELIMINADO. En modo color-key los anillos α 4–16 se
    // pintaban opacos-negros = el "borde negro" que veía el usuario.

    // --- (b) cuerpo: vidrio oscuro con degradé vertical, OPACO (α=255)
    // dentro del octágono; el contorno lo dibuja el borde (d) sobre el
    // α=0 exterior (sin región: la forma vive en el canal alpha).
    let (top, bottom) = if is_hdr {
        (Color::hex("#111c33"), Color::hex("#050b18"))
    } else {
        (Color::hex("#1f2937"), Color::hex("#0b1220"))
    };
    let bx0 = (cx - r).floor() as i32;
    let bx1 = (cx + r).ceil() as i32;
    let by0 = (cy - r).floor() as i32;
    let by1 = (cy + r).ceil() as i32;
    for y in by0..=by1 {
        if y < 0 || y >= h {
            continue;
        }
        let t = ((y as f32) - (cy - r)) / (2.0 * r);
        let t = t.clamp(0.0, 1.0);
        let cr = lerp8(top.r, bottom.r, t);
        let cg = lerp8(top.g, bottom.g, t);
        let cb = lerp8(top.b, bottom.b, t);
        for x in bx0..=bx1 {
            if x < 0 || x >= w {
                continue;
            }
            let f = oct_field(x as f32, y as f32, cx, cy, r);
            if f > 0.5 {
                continue;
            }
            dst.set(x, y, cb, cg, cr, 255);
        }
    }

    // --- (c) bisel interior sutil (blanco α26, 0.91r)
    let r_bevel = r * 0.91;
    let w_bevel = 1.0_f32.max(r / 48.0);
    let rx0 = (cx - r_bevel - 1.5).floor() as i32;
    let rx1 = (cx + r_bevel + 1.5).ceil() as i32;
    let ry0 = (cy - r_bevel - 1.5).floor() as i32;
    let ry1 = (cy + r_bevel + 1.5).ceil() as i32;
    for y in ry0..=ry1 {
        if y < 0 || y >= h {
            continue;
        }
        for x in rx0..=rx1 {
            if x < 0 || x >= w {
                continue;
            }
            let f = oct_field(x as f32, y as f32, cx, cy, r_bevel);
            let cov = (w_bevel / 2.0 - f.abs() + 0.5).clamp(0.0, 1.0);
            if cov <= 0.0 {
                continue;
            }
            let a = (cov * 255.0).round() as u8;
            dst.over(x, y, a, a, a, a); // blanco premultiplied
        }
    }

    // --- (d) borde exterior (glow)
    let w_border = (2.0 * scale_f(scale)).max(r / 26.0);
    for y in by0..=by1 {
        if y < 0 || y >= h {
            continue;
        }
        for x in bx0..=bx1 {
            if x < 0 || x >= w {
                continue;
            }
            let f = oct_field(x as f32, y as f32, cx, cy, r);
            let cov = (w_border / 2.0 - f.abs() + 0.5).clamp(0.0, 1.0);
            if cov <= 0.0 {
                continue;
            }
            let a = (cov * 255.0).round() as u8;
            dst.over(
                x,
                y,
                (glow.b as u32 * a as u32 / 255) as u8,
                (glow.g as u32 * a as u32 / 255) as u8,
                (glow.r as u32 * a as u32 / 255) as u8,
                a,
            );
        }
    }

    // --- (e) badge redondeado con el texto de estado
    let start_pt = 14.0_f32.max(r * 0.42) * scale;
    let start_px = (10.0_f32 * scale).max((start_pt * 4.0 / 3.0).round() as f32) as i32;
    let pxs = fit_px(start_px.max(9), true, state, r * 1.10, r * 0.75);
    let ink = ink_bbox(pxs, true, state);
    if ink.w > 0 && ink.h > 0 {
        let pad_x = (pxs as f32 * 0.45).round() as i32;
        let pad_y = (pxs as f32 * 0.32).round() as i32;
        let bw = ink.w + 2 * pad_x;
        let bh = ink.h + 2 * pad_y;
        let rad = 2.0_f32.max(bh as f32 * 0.30);
        let (btop, bbottom) = if is_hdr {
            (Color::hex("#0a1424"), Color::hex("#04090f"))
        } else {
            (Color::hex("#182231"), Color::hex("#0a111d"))
        };
        let bw2 = bw as f32 / 2.0;
        let bh2 = bh as f32 / 2.0;
        let g0x = (cx - bw2 - 1.0).floor() as i32;
        let g1x = (cx + bw2 + 1.0).ceil() as i32;
        let g0y = (cy - bh2 - 1.0).floor() as i32;
        let g1y = (cy + bh2 + 1.0).ceil() as i32;
        // relleno (degradé vertical sobre el badge)
        for y in g0y..=g1y {
            if y < 0 || y >= h {
                continue;
            }
            let t = ((y as f32) - (cy - bh2)) / (bh as f32);
            let t = t.clamp(0.0, 1.0);
            let cr = lerp8(btop.r, bbottom.r, t);
            let cg = lerp8(btop.g, bbottom.g, t);
            let cb = lerp8(btop.b, bbottom.b, t);
            for x in g0x..=g1x {
                if x < 0 || x >= w {
                    continue;
                }
                let f = rrect_field(x as f32, y as f32, cx, cy, bw2, bh2, rad);
                if f > 0.5 {
                    continue;
                }
                let a = ((0.5 - f) * 255.0).round() as u8;
                if a == 0 {
                    continue;
                }
                dst.over(
                    x,
                    y,
                    (cb as u32 * a as u32 / 255) as u8,
                    (cg as u32 * a as u32 / 255) as u8,
                    (cr as u32 * a as u32 / 255) as u8,
                    a,
                );
            }
        }
        // borde (glow)
        let w_bb = 1.2_f32.max(bh as f32 * 0.055);
        for y in g0y..=g1y {
            if y < 0 || y >= h {
                continue;
            }
            for x in g0x..=g1x {
                if x < 0 || x >= w {
                    continue;
                }
                let f = rrect_field(x as f32, y as f32, cx, cy, bw2, bh2, rad);
                let cov = (w_bb / 2.0 - f.abs() + 0.5).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let a = (cov * 255.0).round() as u8;
                dst.over(
                    x,
                    y,
                    (glow.b as u32 * a as u32 / 255) as u8,
                    (glow.g as u32 * a as u32 / 255) as u8,
                    (glow.r as u32 * a as u32 / 255) as u8,
                    a,
                );
            }
        }
        // (f) texto estado (blanco), tinta centrada 1 px más abajo
        if let Some(rt) = RasterText::new(pxs, true, state) {
            rt.blit_center(&dst, cx.round() as i32, (cy + 1.0 * scale).round() as i32, (255, 255, 255));
        }
    }

    // --- (f2) candado CLÁSICO: estado BLOQUEADO ("Bloquear SDR/HDR").
    // Entre el badge (tope ~0.37r) y el tope del octágono (0.924r):
    // tope del arco 0.889r (gap 0.035r), base del cuerpo 0.405r (gap
    // 0.035r al badge). BLANCO LISO (silueta única, como la referencia
    // de usuario): el contorno de acento a ~12 px solo sumaba ruido y
    // engordaba la forma.
    if locked {
        let s = r * 0.39;
        let oy = cy - 0.731 * r;
        draw_padlock(&dst, cx, oy, s, Color::hex("#ffffff"));
    }

    // --- (g) cartelito "Monitor no compatible" (pill roja, OPACA: la
    // forma vive en el canal alpha del buffer; afuera queda α=0)
    if let Some(warn) = warning {
        if let Some((lx, ly, lw, lh, wpx)) = warning_pill_rect(w, h, scale, warn) {
            let rad = (lh / 2.0).max(1.0);
            let hw = lw / 2.0;
            let hh = lh / 2.0;
            let cxy = ly + hh;
            let p0x = (lx - 1.0).floor() as i32;
            let p1x = (lx + lw + 1.0).ceil() as i32;
            let p0y = (ly - 1.0).floor() as i32;
            let p1y = (ly + lh + 1.0).ceil() as i32;
            for y in p0y..=p1y {
                if y < 0 || y >= h {
                    continue;
                }
                for x in p0x..=p1x {
                    if x < 0 || x >= w {
                        continue;
                    }
                    let f = rrect_field(x as f32, y as f32, lx + hw, cxy, hw, hh, rad);
                    // relleno #7f1d1d OPACO (α230 parcial = se veía oscuro
                    // en modo color-key; la región recorta el contorno)
                    if f <= 0.5 {
                        dst.set(x, y, 0x1d, 0x1d, 0x7f, 255);
                    }
                    // borde #fca5a5 (1.2 × scale) con α parcial
                    // premultiplied: la mitad exterior se compone de
                    // verdad (probe6), sin halo
                    let wb = 1.2 * scale;
                    let covb = (wb / 2.0 - f.abs() + 0.5).clamp(0.0, 1.0);
                    if covb > 0.0 {
                        let a = (covb * 255.0).round() as u8;
                        dst.over(
                            x,
                            y,
                            (0xa5 as u32 * a as u32 / 255) as u8,
                            (0xa5 as u32 * a as u32 / 255) as u8,
                            (0xfc as u32 * a as u32 / 255) as u8,
                            a,
                        );
                    }
                }
            }
            // texto #fecaca, tinta centrada
            if let Some(rt) = RasterText::new(wpx, true, warn) {
                rt.blit_center(&dst, cx.round() as i32, cxy.round() as i32, (0xca, 0xca, 0xfe));
            }
        }
    }

    // --- opacidad global: NO se escala el buffer (en modo color-key la
    // premultiplicación se veía como "oscuridad"): la opacidad la aplica
    // el worker de paint con SetLayeredWindowAttributes post-ULW.

    // Dump de depuración (premultiplied, listo para ULW_ALPHA).
    if let Ok(path) = std::env::var("BOTONHDR_DUMP_PX") {
        let _ = dump_bmp(&path, w, h, buf.bits);
    }
    true
}

/// `scale` saneado (nunca 0 ni negativo).
fn scale_f(scale: f32) -> f32 {
    scale.max(0.5)
}

/// Versión que devuelve el buffer copiado (tests / dumps).
pub fn render_gadget(
    w: i32,
    h: i32,
    scale: f32,
    state: &str,
    accent: &str,
    warning: Option<&str>,
    locked: bool,
) -> Option<Vec<u8>> {
    let mut buf = Scanbuf::new(w, h)?;
    if !render_gadget_into(&mut buf, scale, state, accent, warning, locked) {
        return None;
    }
    let mut out = vec![0u8; buf.w as usize * buf.h as usize * 4];
    unsafe {
        std::ptr::copy_nonoverlapping(buf.bits, out.as_mut_ptr(), out.len());
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Tooltip de la bandeja (2 líneas, esquinas redondeadas)
// ---------------------------------------------------------------------------

const TIP_L2: &str = "click derecho para configurar";

/// Colores del tip según el tema de Windows (AppsUseLightTheme).
pub fn tip_colors(dark: bool) -> (Color, Color, Color, Color) {
    if dark {
        (
            Color::hex("#2b2b2b"),
            Color::hex("#4a4a4a"),
            Color::hex("#f0f0f0"),
            Color::hex("#9a9a9a"),
        )
    } else {
        (
            Color::hex("#f2f2f2"),
            Color::hex("#c4c4c4"),
            Color::hex("#1a1a1a"),
            Color::hex("#5f5f5f"),
        )
    }
}

/// w×h físicos del tip para `scale` (mismos márgenes que la versión Qt:
/// 12/7/13/8, espaciado 3, borde 1 px, radios 5 px).
pub fn tip_layout(scale: f32, state: &str) -> (i32, i32) {
    let line1 = if state == "HDR" { "HDR On" } else { "HDR Off" };
    let (w1, h1) = text_size((15.0 * scale).round() as i32, true, line1);
    let (w2, h2) = text_size((11.0 * scale).round() as i32, false, TIP_L2);
    let cw = w1.max(w2).ceil() as i32;
    let ch = (h1.ceil() as i32) + ((3.0 * scale).round() as i32) + (h2.ceil() as i32);
    let w = cw + (12.0 * scale).round() as i32 + (13.0 * scale).round() as i32 + 2;
    let h = ch + (7.0 * scale).round() as i32 + (8.0 * scale).round() as i32 + 2;
    (w, h)
}

/// Dibuja el tip en `buf`.
pub fn render_tip(buf: &mut Scanbuf, scale: f32, state: &str, dark: bool) -> bool {
    let w = buf.w;
    let h = buf.h;
    if w <= 0 || h <= 0 {
        return false;
    }
    unsafe {
        std::ptr::write_bytes(buf.bits, 0, w as usize * h as usize * 4);
    }
    let dst = Px::new(buf.bits, w, h);
    let (bg, border, fg, sub) = tip_colors(dark);
    let line1 = if state == "HDR" { "HDR On" } else { "HDR Off" };
    let px1 = (15.0 * scale).round() as i32;
    let px2 = (11.0 * scale).round() as i32;
    let rad = 5.0 * scale;

    // fondo OPACO (α=255) + borde (1 px, redondeado 5 px). La forma
    // (rectángulo redondeado) vive en el canal alpha: α=0 afuera del
    // radio → la ventana muestra la forma con contornos limpios.
    for y in 0..h {
        for x in 0..w {
            let f = rrect_field(
                x as f32,
                y as f32,
                (w - 1) as f32 / 2.0,
                (h - 1) as f32 / 2.0,
                (w - 1) as f32 / 2.0,
                (h - 1) as f32 / 2.0,
                rad,
            );
            if f > 0.5 {
                continue;
            }
            dst.set(x, y, bg.b, bg.g, bg.r, 255);
            let wb = 1.0 * scale;
            let covb = (wb / 2.0 - f.abs() + 0.5).clamp(0.0, 1.0);
            if covb > 0.0 {
                let ab = (covb * 255.0).round() as u8;
                // u32: en u8, `border * ab` desborda y el borde salía negro.
                // La mitad exterior del borde lleva α parcial premultiplied
                // y se compone de verdad (probe6): borde suave sin halo.
                dst.over(
                    x,
                    y,
                    (border.b as u32 * ab as u32 / 255) as u8,
                    (border.g as u32 * ab as u32 / 255) as u8,
                    (border.r as u32 * ab as u32 / 255) as u8,
                    ab,
                );
            }
        }
    }

    // líneas de texto (izquierda, alineadas al borde interior)
    let x0 = (1.0 * scale + 12.0 * scale) as i32;
    let y0 = (1.0 * scale + 7.0 * scale) as i32;
    if let Some(rt) = RasterText::new(px1, true, line1) {
        rt.blit_topleft(&dst, x0, y0, (fg.r, fg.g, fg.b));
        let y1 = y0 + rt.ink.h;
        if let Some(rt2) = RasterText::new(px2, false, TIP_L2) {
            rt2.blit_topleft(&dst, x0, y1 + (3.0 * scale) as i32, (sub.r, sub.g, sub.b));
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Icono de bandeja: rectángulo redondeado con texto
// ---------------------------------------------------------------------------

/// HICON de `size`×`size` con el rectángulo redondeado y el texto
/// HDR/SDR (renderizado a 128 px).
pub fn make_tray_icon(state: &str, accent: &str, size: i32, locked: bool) -> Option<*mut c_void> {
    let size = size.max(16);
    let mut dib = Scanbuf::new(size, size)?;
    render_tray_into(&mut dib, state, accent, locked);

    if let Ok(path) = std::env::var("BOTONHDR_DUMP_TRAY") {
        let _ = dump_bmp(&path, dib.w, dib.h, dib.bits);
    }

    // HICON: color 32 bpp + máscara monochrome en cero (Win10/11 usa el
    // canal alfa del bitmap de color).
    let mask = unsafe { w::CreateBitmap(size, size, 1, 1, std::ptr::null()) };
    if mask.is_null() {
        return None;
    }
    let info = w::ICONINFO {
        fIcon: 1,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: dib.hbm,
    };
    let icon = unsafe { w::CreateIconIndirect(&info) };
    unsafe {
        w::DeleteObject(dib.hbm);
        w::DeleteObject(mask);
    }
    Some(icon)
}

/// Dibuja el icono de bandeja en un `Scanbuf` cuadrado (se separa de
/// `make_tray_icon` para que los tests verifiquen píxeles sin crear
/// HICON). Con `locked` agrega chip oscuro + candado en la esquina
/// sup-derecha (separa el candado del borde acento).
pub fn render_tray_into(dib: &mut Scanbuf, state: &str, accent: &str, locked: bool) {
    let size = dib.w;
    let is_hdr = state == "HDR";
    let accent_c = Color::hex(accent);
    let glow = if is_hdr { accent_c } else { accent_c.dark() };
    let dst = Px::new(dib.bits, size, size);
    let m = 1.0_f32.max(size as f32 * 0.005);
    let rw = size as f32 - 2.0 * m;
    let rh = size as f32 - 2.0 * m;
    let rad = size as f32 * 0.18;
    let (top, bottom) = if is_hdr {
        (Color::hex("#111c33"), Color::hex("#050b18"))
    } else {
        (Color::hex("#1f2937"), Color::hex("#0b1220"))
    };
    let penw = 1.5_f32.max(size as f32 / 30.0);
    let innerw = 1.0_f32.max(size as f32 / 64.0);
    let inset = penw + innerw;
    let rad_i = 1.0_f32.max(rad - inset * 1.5);
    let c = size as f32 / 2.0;
    let hw = rw / 2.0;
    let hh = rh / 2.0;

    for y in 0..size {
        for x in 0..size {
            let fx = x as f32;
            let fy = y as f32;
            // relleno (degradé vertical)
            let f = rrect_field(fx, fy, c, c, hw, hh, rad);
            if f <= 0.5 {
                let a = ((0.5 - f) * 255.0).round() as u8;
                let t = ((fy - m) / rh).clamp(0.0, 1.0);
                let cr = lerp8(top.r, bottom.r, t);
                let cg = lerp8(top.g, bottom.g, t);
                let cb = lerp8(top.b, bottom.b, t);
                dst.over(
                    x,
                    y,
                    (cb as u32 * a as u32 / 255) as u8,
                    (cg as u32 * a as u32 / 255) as u8,
                    (cr as u32 * a as u32 / 255) as u8,
                    a,
                );
            }
            // bisel interno blanco α26
            let fi = rrect_field(
                fx,
                fy,
                c,
                c,
                hw - inset,
                hh - inset,
                rad_i,
            );
            let covi = (innerw / 2.0 - fi.abs() + 0.5).clamp(0.0, 1.0);
            if covi > 0.0 {
                let ai = (covi * 26.0).round() as u8;
                dst.over(x, y, ai, ai, ai, ai);
            }
            // borde exterior (glow)
            let fob = f; // reutiliza el SDF del relleno (mismo rect)
            let covb = (penw / 2.0 - fob.abs() + 0.5).clamp(0.0, 1.0);
            if covb > 0.0 {
                let ab = (covb * 255.0).round() as u8;
                dst.over(
                    x,
                    y,
                    (glow.b as u32 * ab as u32 / 255) as u8,
                    (glow.g as u32 * ab as u32 / 255) as u8,
                    (glow.r as u32 * ab as u32 / 255) as u8,
                    ab,
                );
            }
        }
    }

    // chip + candado (bloqueo activo): esquina sup-derecha, DENTRO del
    // relleno (el chip oscuro corta el borde acento que pasa por la
    // esquina). Se dibuja ANTES del texto: el texto ocupa el centro y no
    // toca la esquina.
    if locked {
        let bs = size as f32;
        let (chx, chy) = (bs * 0.835, bs * 0.17);
        let (chw, chh) = (bs * 0.125, bs * 0.10);
        let chr = bs * 0.065;
        let wch = 1.0_f32.max(bs * 0.01);
        let p0x = (chx - chw - 1.0).floor() as i32;
        let p1x = (chx + chw + 1.0).ceil() as i32;
        let p0y = (chy - chh - 1.0).floor() as i32;
        let p1y = (chy + chh + 1.0).ceil() as i32;
        for y in p0y..=p1y {
            if y < 0 || y >= size {
                continue;
            }
            for x in p0x..=p1x {
                if x < 0 || x >= size {
                    continue;
                }
                let f = rrect_field(x as f32, y as f32, chx, chy, chw, chh, chr);
                if f > 0.5 {
                    continue;
                }
                // relleno oscuro (el del degradé inferior del pill)
                let a = ((0.5 - f) * 255.0).round() as u8;
                if a > 0 {
                    dst.over(
                        x,
                        y,
                        (bottom.b as u32 * a as u32 / 255) as u8,
                        (bottom.g as u32 * a as u32 / 255) as u8,
                        (bottom.r as u32 * a as u32 / 255) as u8,
                        a,
                    );
                }
                // borde del chip (acento, fino)
                let covb = (wch / 2.0 - f.abs() + 0.5).clamp(0.0, 1.0);
                if covb > 0.0 {
                    let ab = (covb * 255.0).round() as u8;
                    dst.over(
                        x,
                        y,
                        (glow.b as u32 * ab as u32 / 255) as u8,
                        (glow.g as u32 * ab as u32 / 255) as u8,
                        (glow.r as u32 * ab as u32 / 255) as u8,
                        ab,
                    );
                }
            }
        }
        // Candado BLANCO dentro del chip (el chip oscuro da el contraste;
        // clásico: tope 0.076bs y base 0.264bs, dentro del chip
        // 0.07..0.27bs; cuerpo 0.155bs con márgenes ~0.048bs laterales).
        draw_padlock(&dst, bs * 0.835, bs * 0.137, bs * 0.155, Color::hex("#ffffff"));
    }

    // texto: el más grande que quepa POST-ESTIRADO, tinta centrada.
    // Márgenes holgados: "HDR" mide ~1.98 px de ancho por px de fuente,
    // así que a 128 el ANCHO limita a ~62 px (antes 57: el usuario lo
    // veía "muy chico"). Estirado vertical 1.5x (pedido 2026-10-21,
    // subido de 1.18 → 1.25 → 1.5 a pedido del usuario): el alto se cuenta
    // al ajustar (max_h / estirado) para que la tinta estirada siempre cabe.
    const TRAY_TEXT_STRETCH_Y: f32 = 1.5;
    let max_w = rw - size as f32 * 0.015;
    let max_h = rh - size as f32 * 0.03;
    let px = fit_px(
        (size as f32 * 0.52) as i32,
        true,
        state,
        max_w,
        max_h / TRAY_TEXT_STRETCH_Y,
    );
    if let Some(rt) = RasterText::new(px, true, state) {
        rt.blit_center_stretched(
            &dst,
            (size / 2) as i32,
            (size / 2) as i32,
            (255, 255, 255),
            TRAY_TEXT_STRETCH_Y,
        );
    }
}

// ---------------------------------------------------------------------------
// BMP dump de depuración (validar premultiplicación / antialiasing)
// ---------------------------------------------------------------------------

fn dump_bmp(path: &str, w: i32, h: i32, bits: *const u8) -> std::io::Result<()> {
    let stride = (w as usize * 4 + 3) & !3;
    let row = w as usize * 4;
    let mut out = Vec::with_capacity(54 + stride * h as usize);
    let size = 54u32 + (stride * h as usize) as u32;
    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(54u32).to_le_bytes());
    // BITMAPINFOHEADER (bottom-up)
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&((row * h as usize) as u32).to_le_bytes());
    out.extend_from_slice(&((-2835_i32).to_le_bytes())); // 35 ppi
    out.extend_from_slice(&((-2835_i32).to_le_bytes()));
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    let mut rowbuf = vec![0u8; stride];
    for y in (0..h as usize).rev() {
        let base = y * row;
        for x in 0..row {
            let v = unsafe { *bits.add(base + x) };
            rowbuf[x] = v;
        }
        for x in row..stride {
            rowbuf[x] = 0;
        }
        out.extend_from_slice(&rowbuf);
    }
    std::fs::write(path, out)
}

// ---------------------------------------------------------------------------
// Tests de regresión del render (GDI de texto: la caja de tinta debe
// estar en coordenadas absolutas del scratch, sino el blit lee una
// región vacía y el texto sale invisible)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// El texto del badge (SDR) debe pintarse cerca del centro del gadget.
    #[test]
    fn gadget_texto_estado_visible() {
        let (w, h) = (96i32, 120i32);
        let mut buf = Scanbuf::new(w, h).expect("scratch");
        assert!(render_gadget_into(&mut buf, 1.0, "SDR", "#22d3ee", None, false));
        // Ventana sobre el badge (texto blanco), lejos del bisel blanco del
        // octágono (radio ~0.91r): si se mide en el centro genérico, el
        // bisel da falsos positivos (fue lo que pasó en la revisión previa).
        let (cx, cy, _r, _s) = octagon_geom(w, h);
        let mut white = 0u32;
        unsafe {
            for y in ((cy - 7.0) as i32)..((cy + 9.0) as i32) {
                for x in ((cx - 14.0) as i32)..((cx + 14.0) as i32) {
                    let i = (y as usize * w as usize + x as usize) * 4;
                    let b = *buf.bits.add(i) as i32;
                    let g = *buf.bits.add(i + 1) as i32;
                    let r = *buf.bits.add(i + 2) as i32;
                    if r > 150 && g > 150 && b > 150 {
                        white += 1;
                    }
                }
            }
        }
        assert!(white > 30, "texto SDR invisible: {white} px blancos");
    }

    /// El tip debe pintar ambas líneas (HDR Off + subtítulo).
    #[test]
    fn tip_texto_visible() {
        let (w, h) = tip_layout(1.0, "SDR");
        let mut buf = Scanbuf::new(w, h).expect("scratch");
        assert!(render_tip(&mut buf, 1.0, "SDR", true));
        let (mut fg, mut sub) = (0u32, 0u32);
        unsafe {
            for y in 0..h {
                for x in 0..w {
                    let i = (y as usize * w as usize + x as usize) * 4;
                    let b = *buf.bits.add(i) as i32;
                    let g = *buf.bits.add(i + 1) as i32;
                    let r = *buf.bits.add(i + 2) as i32;
                    if r > 220 && g > 220 && b > 220 {
                        fg += 1;
                    } else if (120..200).contains(&r) && (r - g).abs() < 12 && (g - b).abs() < 12 {
                        sub += 1;
                    }
                }
            }
        }
        assert!(fg > 50, "línea 1 del tip invisible: {fg}");
        assert!(sub > 30, "línea 2 del tip invisible: {sub}");
    }

    /// El cartelito "HDR No compatible" (pill roja) debe pintarse en el
    /// margen superior de la ventana (por encima del tope del octágono):
    /// es lo que el usuario ve cuando el toggle falla.
    #[test]
    fn warning_pill_visible() {
        let (w, h) = (96i32, 120i32);
        let mut buf = Scanbuf::new(w, h).expect("scratch");
        assert!(render_gadget_into(
            &mut buf,
            1.0,
            "SDR",
            "#34d399",
            Some("HDR No compatible"),
            false
        ));
        // Relleno #7f1d1d / borde #fca5a5 / texto #fecaca: en el margen
        // superior (y < tope del octágono) hay que ver píxeles con R >> G.
        let (_cx, cy, r, _s) = octagon_geom(w, h);
        let top = (cy - r).floor() as i32;
        let mut red = 0u32;
        let mut max_y = -1i32;
        unsafe {
            for y in 0..top {
                for x in 0..w {
                    let i = (y as usize * w as usize + x as usize) * 4;
                    let g = *buf.bits.add(i + 1) as i32;
                    let r = *buf.bits.add(i + 2) as i32;
                    let a = *buf.bits.add(i + 3) as i32;
                    if r > 60 && (r - g) > 40 && a > 60 {
                        red += 1;
                        max_y = max_y.max(y);
                    }
                }
            }
        }
        assert!(red > 40, "pill no pintada: {red} px rojos en el margen superior (top={top})");
        assert!(max_y < top, "pill invade el octágono: llega a y={max_y}, top={top}");
    }

    /// El cuerpo del gadget debe pintarse OPACO (α=255) dentro del
    /// octágono: la transparencia la da SLWA post-ULW, no el buffer
    /// (en modo color-key, alfa parcial = color premultiplied oscuro).
    #[test]
    fn gadget_cuerpo_opaco_sin_escalado() {
        let (w, h) = (96i32, 120i32);
        let mut buf = Scanbuf::new(w, h).expect("scratch");
        assert!(render_gadget_into(&mut buf, 1.0, "SDR", "#22d3ee", None, false));
        let (cx, cy, r, _) = octagon_geom(w, h);
        // Punto entre el badge (≤0.55r) y el bisel (0.91r): cuerpo puro.
        let px = (cx + 0.7 * r).round() as i32;
        let py = cy.round() as i32;
        let i = (py as usize * w as usize + px as usize) * 4;
        let (b, g, rr, a) = unsafe {
            (*buf.bits.add(i), *buf.bits.add(i + 1), *buf.bits.add(i + 2), *buf.bits.add(i + 3))
        };
        assert_eq!(a, 255, "cuerpo no opaco: a={a}");
        // Degradé SDR #1f2937→#0b1220: valores oscuros pero NO premultiplied
        // por opacidad (sin el bug, el centro ~50% del degradé da ~#161e2b).
        assert!(rr > 10 && g > 15 && b > 25, "cuerpo premultiplied/oscuro: ({rr},{g},{b})");
        // Esquina de la ventana: afuera del octágono = transparente.
        let corner = (0usize) * 4;
        assert_eq!(unsafe { *buf.bits.add(corner + 3) }, 0, "esquina no transparente");
    }

    /// La región del gadget: 8 vértices dentro de la ventana; con warning
    /// agrega el polígono de la pill (arriba del octágono, dentro de la
    /// ventana) y sin warning no agrega nada.
    #[test]
    fn gadget_region_puntos() {
        let (w, h) = (96i32, 120i32);
        let (oct, pill) = gadget_region_points(w, h, 1.0, None);
        assert!(pill.is_none(), "sin warning no debe haber pill");
        for p in oct {
            assert!(p.x >= 0 && p.x < w && p.y >= 0 && p.y < h, "vértice fuera: {p:?}");
        }
        let (oct2, pill2) = gadget_region_points(w, h, 1.0, Some("HDR No compatible"));
        assert_eq!(oct2, oct, "el octágono no cambia con warning");
        let Some(pts) = pill2 else {
            panic!("con warning debe haber pill")
        };
        assert!(pts.len() >= 32, "pill con pocos puntos: {}", pts.len());
        let (_lx, _ly, _lw, _lh, _wpx) = warning_pill_rect(w, h, 1.0, "HDR No compatible")
            .expect("pill rect");
        let (_cx, cy, r, _) = octagon_geom(w, h);
        for p in &pts {
            assert!(p.x >= 0 && p.x < w && p.y >= 0 && p.y < h, "pill fuera: {p:?}");
            // La pill queda por encima del tope del octágono (margen).
            assert!(p.y < (cy - r).round() as i32 + 2, "pill invade octágono: y={}", p.y);
        }
        // rounded_rect_points: 4 cuartos × 9 puntos = 36, y el primer punto
        // debe ser (x, y+rad) → el inicio de la esquina superior-izquierda.
        let rr = rounded_rect_points(10.0, 20.0, 100.0, 30.0, 6.0);
        assert_eq!(rr.len(), 36);
        assert_eq!((rr[0].x, rr[0].y), (10, 26), "inicio del polígono: {:?}", rr[0]);
    }

    /// El fondo del tip debe ser opaco (α=255) dentro de la forma.
    #[test]
    fn tip_fondo_opaco() {
        let (w, h) = tip_layout(1.0, "SDR");
        let mut buf = Scanbuf::new(w, h).expect("scratch");
        assert!(render_tip(&mut buf, 1.0, "SDR", true));
        let i = ((h / 2) as usize * w as usize + (w / 2) as usize) * 4;
        let (b, g, rr, a) = unsafe {
            (*buf.bits.add(i), *buf.bits.add(i + 1), *buf.bits.add(i + 2), *buf.bits.add(i + 3))
        };
        assert_eq!(a, 255, "fondo del tip no opaco: a={a}");
        // oscuro (tema dark #2b2b2b): no premultiplied
        assert!(rr > 30 && g > 30 && b > 30, "fondo premultiplied: ({rr},{g},{b})");
    }

    /// El icono de bandeja debe pintar el texto blanco centrado.
    #[test]
    fn tray_icon_texto_visible() {
        let mut dib = Scanbuf::new(128, 128).expect("scratch");
        render_tray_into(&mut dib, "SDR", "#22d3ee", false);
        let mut white = 0u32;
        unsafe {
            for y in 48..80 {
                for x in 36..92 {
                    let i = (y as usize * 128 + x as usize) * 4;
                    let b = *dib.bits.add(i) as i32;
                    let g = *dib.bits.add(i + 1) as i32;
                    let r = *dib.bits.add(i + 2) as i32;
                    let a = *dib.bits.add(i + 3) as i32;
                    if r > 200 && g > 200 && b > 200 && a > 200 {
                        white += 1;
                    }
                }
            }
        }
        assert!(white > 50, "texto del icono invisible: {white} px");
    }

    /// El candado (bloqueo activo, BLANCO liso) debe pintarse arriba del
    /// badge (tope-centro del octágono); sin bloqueo esa banda debe
    /// quedar limpia. La banda medida [cy-0.82r, cy-0.53r] × [cx±0.2r]
    /// no la tocan el bisel (tope 0.841r), el borde (r) ni el badge
    /// (tope cy-0.37r): el filtro de blanco no da falsos positivos.
    #[test]
    fn gadget_candado_bloqueo() {
        let (w, h) = (96i32, 120i32);
        let (cx, cy, r, _) = octagon_geom(w, h);
        let count = |locked: bool| -> u32 {
            let mut buf = Scanbuf::new(w, h).expect("scratch");
            assert!(render_gadget_into(&mut buf, 1.0, "SDR", "#22d3ee", None, locked));
            let mut n = 0u32;
            unsafe {
                for y in ((cy - 0.82 * r).round() as i32)..=((cy - 0.53 * r).round() as i32) {
                    for x in ((cx - 0.2 * r).round() as i32)..=((cx + 0.2 * r).round() as i32) {
                        let i = (y as usize * w as usize + x as usize) * 4;
                        let b = *buf.bits.add(i) as i32;
                        let g = *buf.bits.add(i + 1) as i32;
                        let rr = *buf.bits.add(i + 2) as i32;
                        // Cuerpo del candado: blanco puro (el contorno es acento).
                        if b > 200 && g > 200 && rr > 200 {
                            n += 1;
                        }
                    }
                }
            }
            n
        };
        assert_eq!(count(false), 0, "candado pintado sin bloqueo");
        assert!(count(true) > 30, "candado invisible con bloqueo");
    }

    /// Chip + candado del icono de bandeja (bloqueado): esquina
    /// sup-derecha, color acento. La caja medida (92..118 × 8..34) toca el
    /// borde del chip (arriba/izq/abajo) pero no el borde exterior del
    /// rectángulo (que también es acento): sin bloqueo debe quedar limpia.
    /// Ojo: el umbral se midió con el SDF de rrect CORREGIDO (con el bug
    /// de min/min el borde del chip se hinchaba y la cuenta era mayor).
    #[test]
    fn tray_candado_bloqueo() {
        let count = |locked: bool| -> u32 {
            let mut dib = Scanbuf::new(128, 128).expect("scratch");
            render_tray_into(&mut dib, "SDR", "#22d3ee", locked);
            let mut n = 0u32;
            unsafe {
                for y in 8..34 {
                    for x in 92..118 {
                        let i = (y as usize * 128 + x as usize) * 4;
                        let b = *dib.bits.add(i) as i32;
                        let g = *dib.bits.add(i + 1) as i32;
                        let rr = *dib.bits.add(i + 2) as i32;
                        if b > 140 && g > 120 && rr < 80 {
                            n += 1;
                        }
                    }
                }
            }
            n
        };
        assert_eq!(count(false), 0, "chip pintado sin bloqueo");
        assert!(count(true) > 20, "chip/candado invisible con bloqueo");
    }

    /// apply_opacity escala el premultiplied: α y RGB se multiplican por
    /// pct/100; α=0 sigue 0; pct=100 no toca nada.
    #[test]
    fn apply_opacity_escalado() {
        let buf = Scanbuf::new(2, 1).expect("scratch");
        unsafe {
            // píxel 0: (B,G,R,A) = (0, 128, 200, 255) — premultiplied
            *buf.bits = 0;
            *buf.bits.add(1) = 128;
            *buf.bits.add(2) = 200;
            *buf.bits.add(3) = 255;
            // píxel 1: α=0 (transparencia: debe seguir 0)
            *buf.bits.add(4) = 0;
            *buf.bits.add(5) = 0;
            *buf.bits.add(6) = 0;
            *buf.bits.add(7) = 0;
            buf.apply_opacity(50);
            assert_eq!(*buf.bits.add(1), 64);
            assert_eq!(*buf.bits.add(2), 100);
            assert_eq!(*buf.bits.add(3), 127, "255*50/100 = 127 (truncado)");
            assert_eq!(*buf.bits.add(7), 0, "el α=0 no debe cambiar");
            buf.apply_opacity(100);
            assert_eq!(*buf.bits.add(1), 64, "pct=100 debe ser no-op");
        }
    }
}
