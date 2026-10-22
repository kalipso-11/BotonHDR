//! Tooltip propio de 2 líneas para el icono de la bandeja.
//!
//! - WS_POPUP con WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED |
//!   WS_EX_TRANSPARENT (click-through): no roba el foco ni los clicks.
//! - Sin WNDPROC propio: `DefWindowProcW` basta (no hay mensajes propios).
//! - Línea 1 "HDR On"/"HDR Off" + línea 2 localizada por `Cfg.language`
//!   (ya definida en `draw::render_tip` vía `lang::tip_line2`).
//! - Posición: centrado bajo el icono, clamp contra el monitor COMPLETO
//!   (la bandeja queda bajo el work area); si no cabe abajo, se voltea
//!   hacia arriba.
//! - El DPI real del monitor se consulta con una sonda oculta
//!   (`SetWindowPos` + `GetDpiForWindow`); si cambió la escala, se
//!   re-computa layout y se re-renderiza antes del show final.

use crate::draw;
use crate::win as w;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;

const CLASS: &str = "BotonHDRTip";
const TITLE: &str = "Botón HDR (tip)";

/// Tema oscuro de Windows: `AppsUseLightTheme == 0` (fallback: claro).
fn dark_mode() -> bool {
    unsafe {
        let mut h: i64 = 0;
        let path = w::utf16(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        if w::RegOpenKeyExW(w::HKEY_CURRENT_USER, path.as_ptr(), 0, w::KEY_READ, &mut h)
            != w::ERROR_SUCCESS
        {
            return false;
        }
        let name = w::utf16("AppsUseLightTheme");
        let mut val: u32 = 1;
        let mut size = 4u32;
        let r = w::RegQueryValueExW(
            h,
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut val as *mut u32 as *mut u8,
            &mut size,
        );
        let _ = w::RegCloseKey(h);
        r == w::ERROR_SUCCESS && size == 4 && val == 0
    }
}

pub struct TipState {
    hwnd: *mut c_void,
    state: String,
    dark: bool,
    /// Intención de visibilidad: el worker ejecuta ShowWindow.
    visible: bool,
    /// Rect del icono de bandeja para el próximo placement (copy).
    pending_icon: Option<w::RECT>,
    /// Worker de paint (placement + render + UpdateLayeredWindow).
    worker: Option<JoinHandle<()>>,
}

// Canal de paint (mismo patrón que el del gadget): el hilo de mensajes no
// hace ULW/ShowWindow/sonda de DPI: todo corre en un worker (AppHangB1).
// Vive FUERA de `crate::APP` por la misma razón que `GADGET_DIRTY`.
static TIP_DIRTY: Mutex<bool> = Mutex::new(false);
static TIP_CV: Condvar = Condvar::new();
static TIP_STOP: AtomicBool = AtomicBool::new(false);

fn mark_dirty() {
    *TIP_DIRTY.lock().unwrap() = true;
    TIP_CV.notify_one();
}

/// Worker de paint del tip: posiciona (con sonda de DPI en este hilo),
/// renderiza y hace UpdateLayeredWindow. Buffer propio: no cruza hilos.
fn tip_worker(hwnd: *mut c_void) {
    let mut buf: Option<draw::Scanbuf> = None;
    let mut shown = false;
    let mut scale: f32 = 1.0;
    loop {
        // 1) Espera un dirty (el guard se suelta durante la espera).
        {
            let mut d = TIP_DIRTY.lock().unwrap();
            while !*d && !TIP_STOP.load(Ordering::Relaxed) {
                d = TIP_CV.wait(d).unwrap();
            }
            *d = false;
        }
        if TIP_STOP.load(Ordering::Relaxed) {
            break;
        }
        // 2) Snapshot de parámetros (APP breve, sin GDI).
        let snap = {
            let app = crate::APP.lock().unwrap();
            app.as_ref().map(|a| {
                (
                    a.tip.state.clone(),
                    a.tip.dark,
                    a.tip.visible,
                    a.tip.pending_icon,
                    a.cfg.language.clone(),
                )
            })
        };
        let Some((state, dark, visible, icon, lang)) = snap else {
            break; // la app ya no existe (cleanup)
        };
        let Some(icon) = icon else {
            continue;
        };
        // 3) Layout + posición (clamp contra el monitor COMPLETO).
        let (mut bw, mut bh) = draw::tip_layout(scale, &state, &lang);
        if bw < 2 || bh < 2 {
            continue;
        }
        let (mut x, mut y) = (
            icon.left + ((icon.right - icon.left - bw) / 2).max(0),
            icon.bottom + 4,
        );
        let mon = unsafe {
            w::MonitorFromPoint(
                w::POINT {
                    x: (icon.left + icon.right) / 2,
                    y: (icon.top + icon.bottom) / 2,
                },
                w::MONITOR_DEFAULTTONEAREST,
            )
        };
        let mut mi: w::MONITORINFO = unsafe { std::mem::zeroed() };
        mi.cbSize = std::mem::size_of::<w::MONITORINFO>() as u32;
        unsafe {
            if w::GetMonitorInfoW(mon, &mut mi) != 0 {
                let g = mi.rcMonitor;
                if x + bw > g.right - 2 {
                    x = (g.left + 2).max(g.right - bw - 2);
                }
                if y + bh > g.bottom - 2 {
                    y = icon.top - bh - 4; // no cabe abajo: voltear arriba
                }
            }
        }
        // 4) Sonda oculta para el DPI real del monitor destino
        //    (SetWindowPos + GetDpiForWindow): corre en este hilo.
        unsafe {
            let _ = w::SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x,
                y,
                bw,
                bh,
                w::SWP_NOACTIVATE | w::SWP_NOZORDER,
            );
        }
        let dpi = unsafe { w::GetDpiForWindow(hwnd) };
        let s2 = if dpi > 0 { dpi as f32 / 96.0 } else { scale };
        if (s2 - scale).abs() > 0.01 {
            scale = s2;
            let (w2, h2) = draw::tip_layout(scale, &state, &lang);
            if w2 > 1 && h2 > 1 {
                (bw, bh) = (w2, h2);
                x = icon.left + ((icon.right - icon.left - bw) / 2).max(0);
                y = icon.bottom + 4;
                unsafe {
                    if w::GetMonitorInfoW(mon, &mut mi) != 0 {
                        let g = mi.rcMonitor;
                        if x + bw > g.right - 2 {
                            x = (g.left + 2).max(g.right - bw - 2);
                        }
                        if y + bh > g.bottom - 2 {
                            y = icon.top - bh - 4;
                        }
                    }
                }
            }
        }
        // 5) Buffer propio + render.
        if buf.as_ref().map(|b| (b.w, b.h)) != Some((bw, bh)) {
            buf = draw::Scanbuf::new(bw, bh);
        }
        let Some(b) = buf.as_mut() else {
            continue;
        };
        if !draw::render_tip(b, scale, &state, dark, &lang) {
            continue;
        }
        // 6) ULW con DC fuente (bitmap SELECCIONADO en DC compatible).
        let ok = unsafe {
            let hdc = w::GetWindowDC(std::ptr::null_mut());
            let hdc_src = w::CreateCompatibleDC(hdc);
            let old = if hdc_src.is_null() {
                std::ptr::null_mut()
            } else {
                w::SelectObject(hdc_src, b.hbm())
            };
            let ret = w::UpdateLayeredWindow(
                hwnd,
                hdc,
                &w::POINT { x, y },
                &w::SIZE { cx: bw, cy: bh },
                hdc_src,
                &w::POINT { x: 0, y: 0 },
                0,
                &w::BLENDFUNCTION {
                    blendOp: w::AC_SRC_OVER,
                    blendFlags: 0,
                    sourceConstantAlpha: 255,
                    alphaFormat: w::AC_SRC_ALPHA,
                },
                w::ULW_ALPHA,
            ) != 0;
            if !hdc_src.is_null() {
                if !old.is_null() {
                    w::SelectObject(hdc_src, old);
                }
                w::DeleteDC(hdc_src);
            }
            w::ReleaseDC(std::ptr::null_mut(), hdc);
            ret
        };
        // 6b) La forma (rectángulo redondeado) viene del alpha per-píxel
        //     real del buffer premultiplied: α=0 afuera del radio (probe6).
        //     Sin SetWindowRgn (roto en este entorno y no hace falta).
        // 7) Visibilidad.
        if visible && !shown {
            if ok {
                unsafe {
                    let _ = w::ShowWindow(hwnd, w::SW_SHOWNA);
                }
                shown = true;
            }
        } else if !visible && shown {
            unsafe {
                let _ = w::ShowWindow(hwnd, w::SW_HIDE);
            }
            shown = false;
        }
    }
}

impl TipState {
    /// Spawnea el worker de paint (después de instalar la `App` en
    /// `crate::APP`; el worker muere cuando esa `App` deja de existir).
    pub fn start_worker(&mut self) {
        if self.worker.is_some() || self.hwnd.is_null() {
            return;
        }
        let hwnd = w::Shared::new(self.hwnd); // Shared: cruce de threads seguro
        self.worker = std::thread::Builder::new()
            .name("botonhdr-tip-paint".into())
            .spawn(move || tip_worker(hwnd.ptr()))
            .ok();
    }

    pub fn create() -> Option<TipState> {
        unsafe {
            let inst = w::GetModuleHandleW(std::ptr::null());
            let cls = w::utf16(CLASS);
            let wcx = w::WNDCLASSEXW {
                cbSize: std::mem::size_of::<w::WNDCLASSEXW>() as u32,
                style: 0,
                lpfnWndProc: w::DefWindowProcW,
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: inst,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: cls.as_ptr(),
                hIconSm: std::ptr::null_mut(),
            };
            let _ = w::RegisterClassExW(&wcx);
            let name = w::utf16(TITLE);
            let hwnd = w::CreateWindowExW(
                w::WS_EX_TOOLWINDOW
                    | w::WS_EX_NOACTIVATE
                    | w::WS_EX_LAYERED
                    | w::WS_EX_TRANSPARENT,
                cls.as_ptr(),
                name.as_ptr(),
                w::WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                inst,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return None;
            }
            Some(TipState {
                hwnd,
                state: "SDR".into(),
                dark: dark_mode(),
                visible: false,
                pending_icon: None,
                worker: None,
            })
        }
    }

    /// Cambia el estado; el worker re-renderiza al próximo dirty.
    pub fn set_state(&mut self, s: &str) {
        if s != self.state {
            self.state = s.to_string();
            mark_dirty();
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Oculta el tip (intención): el worker ejecuta ShowWindow(SW_HIDE).
    pub fn hide(&mut self) {
        if !self.visible {
            return;
        }
        self.visible = false;
        mark_dirty();
    }

    /// Posiciona sobre `icon` y muestra. El placement (sonda de DPI,
    /// clamp, render y ULW) lo ejecuta el worker: el hilo de mensajes no
    /// espera a DWM (AppHangB1).
    pub fn place_and_show(&mut self, icon: &w::RECT) {
        self.pending_icon = Some(*icon);
        self.visible = true;
        mark_dirty();
    }

    pub fn destroy(&mut self) {
        TIP_STOP.store(true, Ordering::Relaxed);
        TIP_CV.notify_one();
        unsafe {
            if !self.hwnd.is_null() {
                w::DestroyWindow(self.hwnd);
                self.hwnd = std::ptr::null_mut();
            }
        }
        self.worker = None;
        self.visible = false;
    }
}
