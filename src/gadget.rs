//! Gadget del escritorio: botón octagonal tipo skin de Rainmeter.
//!
//! - Ventana WS_POPUP sin bordes, layered, con WS_EX_NOACTIVATE (un click
//!   no la activa ni la eleva en el z-order).
//! - Vive AL FONDO del z-order: se ancla sobre Progman en cada show
//!   (HWND_BOTTOM no funciona en Windows 11: el shell protege la banda de
//!   ventanas de escritorio).
//! - Click corto: alterna HDR/SDR (con traba anti doble-toggle).
//! - Arrastrar: mueve la ventana y guarda la posición (con topes de pantalla).
//! - Cursor "manito" solo sobre el octágono: se setea en WM_MOUSEMOVE
//!   (la ventana al fondo no recibe WM_SETCURSOR; ese handler queda
//!   como respaldo).

use crate::draw;
use crate::win as w;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

const CLASS: &str = "BotonHDRGadget";
const TITLE: &str = "Botón HDR";

/// Umbral (px, Manhattan) de movimiento para considerar el press un arrastre.
const DRAG_THRESHOLD: i32 = 4;

/// Estado de la ventana del gadget. Los campos de arrastre son privados:
/// solo el WNDPROC los toca. El buffer de render NO vive aquí: vive en el
/// hilo de paint (un HBITMAP no cruza hilos).
pub struct GadgetState {
    pub hwnd: *mut c_void,
    pub state: String,
    pub accent: String,
    pub opacity: i32,
    pub size_name: String,
    /// dpi/96 del monitor donde vive la ventana (WM_DPICHANGED / paint).
    pub scale: f32,
    pub warning: Option<String>,
    pub visible: bool,
    /// Estado bloqueado ("Bloquear SDR/HDR" del menú): pinta el candado.
    pub locked: bool,
    /// Posición global (pantalla, px físicos) del presionado.
    press: Option<(i32, i32)>,
    /// Posición global de la ventana al presionar (fija durante el drag).
    win_pos: Option<(i32, i32)>,
    dragging: bool,
    /// Último toggle emitido (traba anti doble-click).
    pub last_toggle: Option<Instant>,
    /// ¿Cursor "manito" activo? (evita re-SetCursor en cada move).
    cursor_hand: bool,
    /// Worker de paint (render + UpdateLayeredWindow).
    worker: Option<JoinHandle<()>>,
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

/// Crea la ventana (clase registrada una vez) de w×h px físicos.
pub fn create(w: i32, h: i32, scale: f32, size_name: &str) -> Option<GadgetState> {
    unsafe {
        let inst = w::GetModuleHandleW(std::ptr::null());
        let cls = w::utf16(CLASS);
        let wcx = w::WNDCLASSEXW {
            cbSize: std::mem::size_of::<w::WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: gadget_wndproc,
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
        let _ = w::RegisterClassExW(&wcx); // si ya existe: no pasa nada
        let name = w::utf16(TITLE);
        // TOOLWINDOW: no aparece en taskbar ni Alt-Tab. (Verificado en
        // vivo 2026-10-20: el TOOLWINDOW NO afecta a la composición del
        // ULW — el bug del "fondo negro" era la constante ULW_ALPHA, ver
        // win.rs y notas.md.)
        let exstyles = w::WS_EX_LAYERED | w::WS_EX_TOOLWINDOW | w::WS_EX_NOACTIVATE;
        let hwnd = w::CreateWindowExW(
            exstyles,
            cls.as_ptr(),
            name.as_ptr(),
            w::WS_POPUP,
            0,
            0,
            w.max(1),
            h.max(1),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            inst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return None;
        }
        Some(GadgetState {
            hwnd,
            state: "SDR".into(),
            accent: draw::COLORS[0].1.to_string(),
            opacity: 100,
            size_name: size_name.to_string(),
            scale,
            warning: None,
            visible: false,
            locked: false,
            press: None,
            win_pos: None,
            dragging: false,
            last_toggle: None,
            cursor_hand: false,
            worker: None,
        })
    }
}

/// Ancla la ventana al fondo del z-order (sobre Progman). Idempotente.
pub fn anchor_to_bottom(hwnd: *mut c_void) {
    unsafe {
        let progman = w::FindWindowW(w::utf16("Progman").as_ptr(), std::ptr::null());
        let _ = w::SetWindowPos(
            hwnd,
            progman, // null (HWND_BOTTOM) si no existe: fallback
            0,
            0,
            0,
            0,
            w::SWP_NOMOVE | w::SWP_NOSIZE | w::SWP_NOACTIVATE,
        );
    }
}

// ---------------------------------------------------------------------------
// Canal de paint: el hilo de mensajes solo marca "sucio"; un worker hace el
// render + UpdateLayeredWindow. Motivo (AppHangB1 observado, WER 2026-10-20):
// ULW/ShowWindow sobre una ventana ULW pueden bloquearse si DWM está
// reconfigurando displays; el hilo de mensajes jamás debe esperarlos (se
// congelarían timers, bandeja y arrastre). El worker, si se bloquea, solo
// se fuga: la app sigue respondiendo.
//
// El canal vive FUERA de `crate::APP` (estáticos): esperar en el Condvar
// exige el guard del lock dirty, y ese no puede sostenerse junto con APP
// (los setters toman APP y después toman dirty, brevemente).
// ---------------------------------------------------------------------------

static GADGET_DIRTY: Mutex<bool> = Mutex::new(false);
static GADGET_CV: Condvar = Condvar::new();
static GADGET_STOP: AtomicBool = AtomicBool::new(false);
/// Chequeo de geometría off-screen one-shot (tras cambio de display):
/// el worker lo ejecuta en su próximo ciclo (SetWindowPos fuera del
/// hilo de mensajes; ver AppHangB1, WER 2026-10-21).
static GEO_CHECK: AtomicBool = AtomicBool::new(false);
/// Escala autoritativa opcional para el chequeo (GetDpiForMonitor del
/// primario en el momento del cambio): tras un hotplug, GetDpiForWindow
/// puede quedarse con la escala vieja hasta que Windows entrega
/// WM_DPICHANGED (segundos después); el hint corrige el paso 3.5 al
/// instante.
static GEO_HINT: Mutex<Option<f32>> = Mutex::new(None);

/// Pide al worker de paint un chequeo de geometría completo (tamaño
/// canónico + off-screen): se invoca tras WM_DISPLAYCHANGE. El worker
/// es el único que hace SetWindowPos sobre esta ventana: durante una
/// reconfiguración de displays el hilo de mensajes puede bloquearse en
/// el stack de ventanas (AppHangB1, WER 2026-10-21 10:56). `hint` es la
/// escala del primario nuevo leída en vivo (GetDpiForMonitor); si viene,
/// el worker la usa en lugar de GetDpiForWindow (que puede estar atrasada).
pub fn request_geometry_check(hint: Option<f32>) {
    if let Some(h) = hint {
        *GEO_HINT.lock().unwrap() = Some(h);
    }
    GEO_CHECK.store(true, Ordering::Relaxed);
    mark_dirty();
}

/// Reposición completa pedida por `on_display_change` (cambio de monitor
/// primario): el worker mueve la ventana a (x, y) con tamaño (w, h); `scale`
/// es la escala FINAL resuelta por `resolve_display_state` (muestra GDI
/// corregida por WM_DPICHANGED/DWM — v7: la escala que pasa al worker
/// manda sobre la que el worker lea solo de GDI/DWM en ese instante).
static GADGET_MOVE_TO: Mutex<Option<(i32, i32, i32, i32, f32)>> = Mutex::new(None);

/// Marca sucio: el worker de paint lo toma (render + ULW).
fn mark_dirty() {
    *GADGET_DIRTY.lock().unwrap() = true;
    GADGET_CV.notify_one();
}

/// Pide al worker de paint una reposición completa (nunca en el hilo de
/// mensajes en plena reconfiguración; ver AppHangB1, WER 2026-10-21):
/// se invoca cuando el monitor primario cambia y el gadget vive en otro.
pub fn request_reposition(x: i32, y: i32, w: i32, h: i32, scale: f32) {
    *GADGET_MOVE_TO.lock().unwrap() = Some((x, y, w, h, scale));
    mark_dirty();
}

/// Descarta una reposición en cola (v6): con un nuevo WM_DISPLAYCHANGE la
/// petición anterior puede estar calculada contra el mundo VIEJO (se
/// observó en el log que un move queueado antes del swap se ejecutaba
/// después, sobre el monitor nuevo). El ajuste nuevo lo re-encola si hace
/// falta.
pub fn take_pending_move() -> Option<(i32, i32, i32, i32, f32)> {
    GADGET_MOVE_TO.lock().unwrap().take()
}

/// Worker de paint: renderiza el gadget + UpdateLayeredWindow.
///
/// - Espera `GADGET_DIRTY` (Condvar: sin polling, sin CPU en reposo).
/// - Toma `crate::APP` solo para un snapshot de parámetros (nunca GDI
///   bajo el lock; nunca toca tray/tip).
/// - El buffer es PROPIO del worker (no cruza hilos).
/// - Ejecuta `ShowWindow` (show/hide) cuando cambia la intención: este es
///   el único hilo que llama ShowWindow sobre esta ventana.
/// - La forma (octágono + pill) viene del alpha per-píxel REAL del buffer
///   premultiplied (probe6): sin SetWindowRgn (CreatePolygonRgn roto en
///   este entorno y ya no necesario).
/// - La opacidad real (20–100 %) se hornea en el buffer con
///   `Scanbuf::apply_opacity` (escala el premultiplied; α=0 sigue 0 →
///   transparencia real, sin oscurecer). NO se usa SLWA: en esta máquina
///   aplana la superficie contra negro antes de aplicar bAlpha (las
///   zonas α=0 salen oscuras, no transparentes).
/// - Sin SLWA, ULW es repetible sin error 87; si aun así un ULW falla,
///   se hace un toggle de WS_EX_LAYERED y se reintenta una vez (probe5).
fn paint_worker(hwnd: *mut c_void) {
    let mut buf: Option<draw::Scanbuf> = None;
    let mut shown = false;
    // Clave del último contenido ULW (incluye la opacidad, que se hornea
    // en el buffer): evita re-ULW innecesarios (p. ej. arrastre: el
    // contenido es relativo a la ventana).
    let mut last_key: Option<(
        String,
        String,
        i32,
        i32,
        i32,
        Option<String>,
        u8,
        bool,
    )> = None;
    let mut first_ok = true;
    loop {
        // 1) Espera un dirty. El guard se suelta durante la espera.
        {
            let mut d = GADGET_DIRTY.lock().unwrap();
            while !*d && !GADGET_STOP.load(Ordering::Relaxed) {
                d = GADGET_CV.wait(d).unwrap();
            }
            *d = false;
        }
        if GADGET_STOP.load(Ordering::Relaxed) {
            break;
        }
        // 2) Geometría + DPI del monitor donde vive (sin APP).
        let (mut wr, mut scale) = unsafe {
            let mut wr: w::RECT = std::mem::zeroed();
            if w::GetWindowRect(hwnd, &mut wr) == 0 {
                continue;
            }
            let dpi = w::GetDpiForWindow(hwnd);
            (wr, if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 })
        };
        // Escala autoritativa del último cambio de display (GetDpiForMonitor
        // del primario): tras un hotplug, GetDpiForWindow puede quedarse
        // con la escala vieja hasta que llegue WM_DPICHANGED.
        if let Some(h) = GEO_HINT.lock().unwrap().take() {
            scale = h.max(0.1);
        }
        let (mut cw, mut ch) = (wr.right - wr.left, wr.bottom - wr.top);
        if cw < 2 || ch < 2 {
            continue;
        }
        // 2.5) Reposición completa pendiente (cambio de monitor primario):
        //     SetWindowPos a la posición/tamaño ya calculados por
        //     on_display_change, con la escala FINAL resuelta PASADA como
        //     argumento (v7: muestra GDI corregida por WM/DWM; con la de
        //     GDI cruda el worker achicaba el gadget, log 2026-10-27). No
        //     se re-lee GetDpiForWindow aquí: tras un hotplug puede
        //     devolver la escala vieja hasta el WM_DPICHANGED y el paso
        //     3.5 revertiría el tamaño recién puesto.
        //     v6.1: el guard se suelta ANTES del GDI. Con `if let` sobre
        //     `lock().take()`, Rust extiende la vida del guard a TODO el
        //     bloque (SetWindowPos + anchor_to_bottom + slog): en plena
        //     reconfiguración de displays esas llamadas pueden bloquearse
        //     y el WM_DISPLAYCHANGE del hilo de UI (take_pending_move)
        //     se deadlockeaba → message loop muerto, bandeja/menú
        //     colgados y sin más applies (log 2026-10-23: step=43 a
        //     t=…982555 y silencio; solo hb de otro hilo seguía).
        let move_req = GADGET_MOVE_TO.lock().unwrap().take();
        if let Some((mx, my, mw, mh, msc)) = move_req {
            unsafe {
                w::SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    mx,
                    my,
                    mw,
                    mh,
                    w::SWP_NOZORDER | w::SWP_NOACTIVATE,
                );
            }
            wr = w::RECT { left: mx, top: my, right: mx + mw, bottom: my + mh };
            (cw, ch) = (mw, mh);
            scale = msc.max(0.1);
            anchor_to_bottom(hwnd);
            crate::slog(
                48,
                &format!("gadget_repositioned ({},{}) {}x{} (esc={})", mx, my, mw, mh, scale),
            );
        }
        // 3) Snapshot de parámetros + sync de escala (APP breve, sin GDI).
        let snap = {
            let mut app = crate::APP.lock().unwrap();
            app.as_mut().map(|a| {
                a.gadget.scale = scale;
                (
                    a.gadget.state.clone(),
                    a.gadget.accent.clone(),
                    a.gadget.opacity,
                    a.gadget.warning.clone(),
                    a.gadget.visible,
                    a.gadget.locked,
                    a.gadget.size_name.clone(),
                )
            })
        };
        let Some((state, accent, opacity, warning, visible, locked, size_name)) = snap else {
            break; // la app ya no existe (cleanup): el worker se muere
        };
        // 3.5) Tamaño canónico (tamaño lógico × escala del monitor):
        //     tras una reconfiguración de displays (hotplug, cambio de
        //     resolución) o un WM_DPICHANGED, Windows recorta/redimensiona
        //     las ventanas de forma NO uniforme; se fuerza el tamaño
        //     de vuelta manteniendo el centro global del octágono. Corre
        //     en ESTE worker: SetWindowPos en el hilo de mensajes en plena
        //     reconfiguración puede bloquearse en el stack de ventanas
        //     (AppHangB1, WER 2026-10-21 10:56: display_change → silencio
        //     → click en el gadget sin procesar).
        let (lw, lh) = draw::size_of(&size_name);
        let (ew, eh) = (
            (lw as f32 * scale).round() as i32,
            (lh as f32 * scale).round() as i32,
        );
        if cw.abs_diff(ew) > 2 || ch.abs_diff(eh) > 2 {
            let (old_w, old_h) = (cw, ch);
            let (cx, cy, _, _) = draw::octagon_geom(cw, ch);
            let center = (wr.left + cx.round() as i32, wr.top + cy.round() as i32);
            let (ncx, ncy, _, _) = draw::octagon_geom(ew, eh);
            let (nx, ny) = (
                center.0 - ncx.round() as i32,
                center.1 - ncy.round() as i32,
            );
            unsafe {
                w::SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    nx,
                    ny,
                    ew,
                    eh,
                    w::SWP_NOZORDER | w::SWP_NOACTIVATE,
                );
            }
            wr = w::RECT { left: nx, top: ny, right: nx + ew, bottom: ny + eh };
            (cw, ch) = (ew, eh);
            crate::slog(
                45,
                &format!(
                    "gadget_ressized {}x{} -> {}x{} (esc={})",
                    old_w, old_h, ew, eh, scale
                ),
            );
        }
        // 3.6) Chequeo off-screen one-shot tras cambio de display (lo pide
        //     request_geometry_check desde WM_DISPLAYCHANGE): si el gadget
        //     quedó fuera de toda la pantalla virtual → esquina inf-derecha
        //     del work area primario + re-ancle al fondo.
        if GEO_CHECK.swap(false, Ordering::Relaxed) {
            let (vx0, vy0) = (
                unsafe { w::GetSystemMetrics(w::SM_XVIRTUALSCREEN) },
                unsafe { w::GetSystemMetrics(w::SM_YVIRTUALSCREEN) },
            );
            let (vw, vh) = (
                unsafe { w::GetSystemMetrics(w::SM_CXVIRTUALSCREEN) },
                unsafe { w::GetSystemMetrics(w::SM_CYVIRTUALSCREEN) },
            );
            let off = wr.right <= vx0
                || wr.left >= vx0 + vw
                || wr.bottom <= vy0
                || wr.top >= vy0 + vh;
            if off {
                let (gw, gh) = (wr.right - wr.left, wr.bottom - wr.top);
                let (mut nx, mut ny) = if let Some(av) = crate::primary_work_area() {
                    (av.right - gw - 24, av.bottom - gh - 24)
                } else {
                    (0, 0)
                };
                // Clamp a la pantalla VIRTUAL: en plena reconfiguración el
                // work area primario puede estar stale y dejarlo afuera.
                if nx < vx0 {
                    nx = vx0;
                }
                if ny < vy0 {
                    ny = vy0;
                }
                if nx + gw > vx0 + vw {
                    nx = (vx0 + vw - gw - 8).max(vx0);
                }
                if ny + gh > vy0 + vh {
                    ny = (vy0 + vh - gh - 8).max(vy0);
                }
                let old = (wr.left, wr.top);
                unsafe {
                    w::SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        nx,
                        ny,
                        0,
                        0,
                        w::SWP_NOSIZE | w::SWP_NOACTIVATE,
                    );
                }
                wr = w::RECT { left: nx, top: ny, right: nx + gw, bottom: ny + gh };
                anchor_to_bottom(hwnd);
                crate::slog(
                    44,
                    &format!("gadget_replaced ({},{}) -> ({},{})", old.0, old.1, nx, ny),
                );
                // El gadget quedó en el primario: actualizar memoria por
                // monitor (posición relativa a la sup-izq de ese monitor).
                // En plena reconfiguración EnumDisplayDevices/GetMonitorInfo
                // pueden fallar un instante (GDI saturado): retry corto.
                let mut prim = crate::primary_monitor();
                if prim.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    prim = crate::primary_monitor();
                }
                // Puerta de escala: no corromper la memoria por monitor con
                // una muestra de GDI STALE (id/escala del monitor viejo que
                // contradice un WM_DPICHANGED fresco ≤20 s). v6: además se
                // SKIPEA el propio reemplazo off-screen cuando GDI está
                // stale: GetSystemMetrics puede seguir reportando la
                // pantalla virtual vieja unos segundos tras el swap y
                // marcar "off" a un gadget que en el monitor nuevo SÍ está
                // en pantalla (el ajuste de display ya lo colocó); moverlo
                // a la esquina con métricas stale lo dejaba mal ubicado.
                let wm = wm_dpi_fresh(20_000);
                let stale_gdi = match (&wm, &prim) {
                    (Some(w), Some(p)) => (p.scale - w).abs() > 0.01,
                    _ => false,
                };
                if stale_gdi {
                    crate::slog(47, "gadget_replaced_skip_stale_gdi");
                } else if let Some(prim) = prim {
                    if let Ok(mut app) = crate::APP.lock() {
                        if let Some(a) = app.as_mut() {
                            a.monitor_id = prim.id.clone();
                            // Escala fresca primero (DWM); la de GDI solo
                            // si no hay WM fresco (aunque coincida con la de
                            // GDI, la de DWM es la autoritativa).
                            let s = wm.unwrap_or(prim.scale).max(0.1);
                            a.cfg.monitor_mem.insert(
                                crate::mem_key(&prim.id, s),
                                [
                                    ((nx - prim.mon.left) as f32 / s).round() as i64,
                                    ((ny - prim.mon.top) as f32 / s).round() as i64,
                                ],
                            );
                            crate::config::save(&a.cfg);
                        }
                    }
                }
            }
        }
        // 4) Clave de contenido (incluye la opacidad): ¿cambió algo
        //    desde el último ULW?
        let alpha = ((opacity as u32).clamp(0, 100) * 255 / 100) as u8;
        let key = (
            state.clone(),
            accent.clone(),
            cw,
            ch,
            (scale * 1000.0).round() as i32,
            warning.clone(),
            alpha,
            locked,
        );
        // 5) Visibilidad: el único hilo con ShowWindow sobre esta ventana.
        let just_shown = if visible && !shown {
            unsafe {
                let _ = w::ShowWindow(hwnd, w::SW_SHOWNOACTIVATE);
            }
            // Tras hide/show volvería arriba: re-ancelar al fondo.
            anchor_to_bottom(hwnd);
            shown = true;
            crate::slog(16, "gadget_shown");
            true
        } else if !visible && shown {
            unsafe {
                let _ = w::ShowWindow(hwnd, w::SW_HIDE);
            }
            shown = false;
            false
        } else {
            false
        };
        // 6) Contenido + opacidad real en la capa DWM. La opacidad se
        //    hornea en el buffer (apply_opacity): sin SLWA, ULW es
        //    repetible sin error 87.
        if last_key.as_ref() == Some(&key) && !just_shown {
            continue; // nada cambió: el contenido ya está en DWM
        }
        // Buffer propio del worker; se recrea si cambió el tamaño.
        if buf.as_ref().map(|b| (b.w, b.h)) != Some((cw, ch)) {
            buf = draw::Scanbuf::new(cw, ch);
        }
        let Some(b) = buf.as_mut() else {
            crate::slog(34, "paint_buf_none");
            continue;
        };
        // Render premultiplied con alpha REAL por píxel: la forma con
        // AA (octágono + pill + texto) vive en el buffer mismo.
        if !draw::render_gadget_into(b, scale, &state, &accent, warning.as_deref(), locked) {
            crate::slog(36, "paint_render_false");
            continue;
        }
        // Opacidad global (20–100 %): escala el buffer premultiplied.
        b.apply_opacity(opacity);
        let ulw_ok = unsafe {
            if do_ulw(hwnd, wr.left, wr.top, cw, ch, b) != 0 {
                true
            } else {
                // Respaldo (quirk documentado por MS; verificado probe5):
                // toggle de WS_EX_LAYERED y reintentar una vez.
                let gle1 = w::GetLastError();
                let ex = w::GetWindowLongPtrW(hwnd, w::GWL_EXSTYLE);
                w::SetWindowLongPtrW(
                    hwnd,
                    w::GWL_EXSTYLE,
                    ex & !(w::WS_EX_LAYERED as isize),
                );
                w::SetWindowLongPtrW(
                    hwnd,
                    w::GWL_EXSTYLE,
                    w::GetWindowLongPtrW(hwnd, w::GWL_EXSTYLE) | (w::WS_EX_LAYERED as isize),
                );
                let ret2 = do_ulw(hwnd, wr.left, wr.top, cw, ch, b);
                if ret2 == 0 {
                    crate::slog(
                        35,
                        &format!("ulw_fail gle1={} gle2={}", gle1, w::GetLastError()),
                    );
                }
                ret2 != 0
            }
        };
        if ulw_ok {
            last_key = Some(key);
            if first_ok {
                crate::slog(80, "paint_ulw_opacity_ok");
                first_ok = false;
            }
        }
    }
}

/// UpdateLayeredWindow con DC fuente (el bitmap debe ir SELECCIONADO en
/// un DC compatible; pasar el HBITMAP directamente hace que falle).
unsafe fn do_ulw(hwnd: *mut c_void, x: i32, y: i32, cw: i32, ch: i32, b: &draw::Scanbuf) -> i32 {
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
        &w::SIZE { cx: cw, cy: ch },
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
    );
    if !hdc_src.is_null() {
        if !old.is_null() {
            w::SelectObject(hdc_src, old);
        }
        w::DeleteDC(hdc_src);
    }
    w::ReleaseDC(std::ptr::null_mut(), hdc);
    ret
}

impl GadgetState {
    /// Spawnea el worker de paint. Debe llamarse DESPUÉS de instalar la
    /// `App` en `crate::APP` (el worker toma snapshots ahí; cuando esa
    /// `App` deja de existir, el worker muere).
    pub fn start_worker(&mut self) {
        if self.worker.is_some() || self.hwnd.is_null() {
            return;
        }
        let hwnd = w::Shared::new(self.hwnd); // Shared: cruce de threads seguro
        self.worker = std::thread::Builder::new()
            .name("botonhdr-gadget-paint".into())
            .spawn(move || paint_worker(hwnd.ptr()))
            .ok();
    }

    pub fn set_state(&mut self, s: &str) {
        if s != self.state {
            self.state = s.to_string();
            mark_dirty();
        }
    }

    pub fn set_accent(&mut self, c: &str) {
        if c != self.accent {
            self.accent = c.to_string();
            mark_dirty();
        }
    }

    pub fn set_opacity_pct(&mut self, pct: i32) {
        if pct != self.opacity {
            self.opacity = pct.clamp(0, 100);
            mark_dirty();
        }
    }

    pub fn set_locked(&mut self, l: bool) {
        if l != self.locked {
            self.locked = l;
            mark_dirty();
        }
    }

    pub fn show_warning(&mut self, text: &str) {
        self.warning = Some(text.to_string());
        mark_dirty();
    }

    pub fn clear_warning(&mut self) {
        if self.warning.is_some() {
            self.warning = None;
            mark_dirty();
        }
    }

    /// Repinta sin cambiar de estado (p. ej. tras re-posicionarse).
    pub fn refresh(&mut self) {
        mark_dirty();
    }

    /// Muestra el gadget (intención). El worker ejecuta ShowWindow +
    /// re-ancle al fondo + ULW; el hilo de mensajes no espera a DWM.
    pub fn show(&mut self) {
        if self.visible {
            return;
        }
        self.visible = true;
        mark_dirty();
    }

    pub fn hide(&mut self) {
        if !self.visible {
            return;
        }
        self.visible = false;
        mark_dirty();
    }

    pub fn set_position(&mut self, x: i32, y: i32) {
        unsafe {
            let _ = w::SetWindowPos(
                self.hwnd,
                std::ptr::null_mut(),
                x,
                y,
                0,
                0,
                w::SWP_NOSIZE | w::SWP_NOACTIVATE,
            );
        }
    }

    /// Cambia el tamaño manteniendo el centro del octágono en el mismo
    /// punto de la pantalla: el worker de paint fuerza el tamaño canónico
    /// (tamaño lógico × escala) en su próximo ciclo (paso 3.5) — el
    /// SetWindowPos corre fuera del hilo de mensajes (AppHangB1, WER
    /// 2026-10-21 10:56). El worker recrea el buffer al ver el tamaño.
    pub fn set_size(&mut self, name: &str) {
        if !draw::SIZES.iter().any(|(n, _)| *n == name) || name == self.size_name {
            return;
        }
        self.size_name = name.to_string();
        crate::slog(84, &format!("gadget_set_size {}", name));
        mark_dirty();
    }

    pub fn destroy(&mut self) {
        // Detiene al worker (notify: puede dormir en el Condvar) y descarta
        // el handle SIN join: se muere con el proceso. Un ULW sobre un hwnd
        // destruido solo falla con error, no crashea.
        GADGET_STOP.store(true, Ordering::Relaxed);
        GADGET_CV.notify_one();
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

// ---------------------------------------------------------------------------
// WNDPROC (los handlers toman el lock de `crate::APP` y lo sueltan al salir)
// ---------------------------------------------------------------------------

/// ¿El punto global (pantalla, px físicos) está dentro del octágono?
fn inside_octagon(hwnd: *mut c_void, gx: i32, gy: i32) -> bool {
    unsafe {
        let mut wr: w::RECT = std::mem::zeroed();
        if w::GetWindowRect(hwnd, &mut wr) == 0 {
            return false;
        }
        let (cw, ch) = (wr.right - wr.left, wr.bottom - wr.top);
        let (cx, cy, r, _) = draw::octagon_geom(cw, ch);
        let dx = (gx - (wr.left + cx.round() as i32)) as f32;
        let dy = (gy - (wr.top + cy.round() as i32)) as f32;
        dx * dx + dy * dy <= r * r
    }
}

/// lParam de WM_MOUSE* (coords de cliente, i16 empaquetados) → pantalla.
fn client_to_global(hwnd: *mut c_void, lparam: isize) -> Option<(i32, i32)> {
    let cx = (lparam & 0xFFFF) as i16 as i32;
    let cy = ((lparam >> 16) & 0xFFFF) as i16 as i32;
    let mut pt = w::POINT { x: cx, y: cy };
    unsafe {
        if w::ClientToScreen(hwnd, &mut pt) == 0 {
            return None;
        }
    }
    Some((pt.x, pt.y))
}

/// WM_DPICHANGED: wparam = MAKELPARAM(dpi_nuevo, dpi_viejo). Es la señal
/// de que Windows YA aplicó la nueva escala — a menudo SEGUNDOS después
/// del hotplug, cuando GDI (EnumDisplayDevices/GetMonitorInfoW) todavía
/// reporta la topología VIEJA. Por eso:
/// 1) pasa la escala nueva como hint al worker de paint (resize canónico +
///    chequeo off-screen inmediatos; el worker es el único que hace
///    SetWindowPos — AppHangB1, WER 2026-10-21 10:56);
/// 2) si la escala cambió respecto del último DPICHANGED procesado,
///    re-dispara el ajuste completo de display (hilo efímero, serializado
///    con DISP_BUSY): si GDI ya es fresco, mueve el gadget a la posición
///    recordada del monitor nuevo;
/// 3) registra el instante del evento (LAST_WM_DPI_MS): `wm_dpi_fresh` lo
///    usa como puerta de escala para no actuar con GDI stale.
static LAST_DPICHANGED_DPI: AtomicU32 = AtomicU32::new(0);
/// Instante (ms desde la época) del último WM_DPICHANGED procesado.
static LAST_WM_DPI_MS: AtomicU64 = AtomicU64::new(0);

/// Escala reportada por el último WM_DPICHANGED, si tiene menos de
/// `max_age_ms` de antigüedad. Es la señal más fresca y fiable tras un
/// hotplug: GDI (EnumDisplayDevices/GetMonitorInfoW/GetDpiForMonitor) puede
/// tardar hasta ~15–20 s en reflejar el monitor nuevo. Se usa como PUERTA
/// DE ESCALA: una muestra de GDI cuya escala contradice este valor es STALE
/// y no debe usarse para mover el gadget ni para escribir la memoria por
/// monitor.
pub fn wm_dpi_fresh(max_age_ms: u64) -> Option<f32> {
    let dpi = LAST_DPICHANGED_DPI.load(Ordering::Relaxed);
    if dpi == 0 {
        return None;
    }
    let age = crate::now_ms().saturating_sub(LAST_WM_DPI_MS.load(Ordering::Relaxed));
    (age <= max_age_ms).then_some(dpi as f32 / 96.0)
}

/// Invalida el hint de WM_DPICHANGED (v6): un DPICHANGED ANTERIOR al último
/// WM_DISPLAYCHANGE NO puede validar datos de GDI del swap nuevo — se
/// observó en el log que uno de hace 7–15 s abría la puerta de escala y se
/// actuaba con una muestra stale del mundo viejo (el gadget quedaba con el
/// tamaño del monitor anterior). Solo un DPICHANGED POSTERIOR al último
/// cambio de display cuenta como fresco; por eso se llama desde el brazo
/// WM_DISPLAYCHANGE, antes de que cualquier DPICHANGED posterior se
/// procese.
pub fn wm_hint_invalidate() {
    LAST_WM_DPI_MS.store(0, Ordering::Relaxed);
}

fn on_dpi_changed(hwnd: *mut c_void, wparam: usize) {
    let new_dpi = (wparam & 0xFFFF) as u32;
    if new_dpi == 0 {
        return;
    }
    crate::slog(49, &format!("gadget_dpichanged dpi={}", new_dpi));
    LAST_WM_DPI_MS.store(crate::now_ms(), Ordering::Relaxed);
    let _ = hwnd;
    // (1) Escala nueva autoritativa para el worker de paint.
    request_geometry_check(Some(new_dpi as f32 / 96.0));
    // (2) ¿Escala distinta a la última procesada? Re-evaluar todo: el
    //     estado con el que se actuó en el último WM_DISPLAYCHANGE puede
    //     ser GDI stale. Los duplicados consecutivos no re-disparan nada.
    let prev = LAST_DPICHANGED_DPI.swap(new_dpi, Ordering::Relaxed);
    if prev != new_dpi {
        crate::on_display_change();
    }
}

unsafe extern "system" fn gadget_wndproc(
    hwnd: *mut c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    match msg {
        w::WM_PAINT => {
            // El contenido vive en la capa DWM (UpdateLayeredWindow desde el
            // worker de paint): aquí solo se limpia la cola de paint. NO se
            // marca dirty: no hay nada que repintar con GDI, y los paints
            // generados por el toggle de WS_EX_LAYERED no requieren ULW.
            unsafe {
                let mut ps: w::PAINTSTRUCT = std::mem::zeroed();
                w::BeginPaint(hwnd, &mut ps);
                w::EndPaint(hwnd, &ps);
            }
            1
        }
        w::WM_ERASEBKGND => 1,
        // PrintWindow / capturas externas: el contenido vive en la capa DWM
        // (UpdateLayeredWindow). Si se re-entra a paint() mientras DWM
        // renderiza la ventana para la captura, deadlock; con flag 2
        // (PW_RENDERFULLCONTENT) DWM compone la capa directamente.
        w::WM_PRINT | w::WM_PRINTCLIENT => 1,
        w::WM_SETCURSOR => {
            // Manito solo sobre el octágono (área clicable). Sin lock:
            // depende solo de la geometría de la ventana.
            let mut pt: w::POINT = std::mem::zeroed();
            if w::GetCursorPos(&mut pt) == 0 {
                return 0; // no forzar el cursor
            }
            let inside = inside_octagon(hwnd, pt.x, pt.y);
            let cur = w::LoadCursorW(
                std::ptr::null_mut(),
                if inside { w::IDC_HAND } else { w::IDC_ARROW },
            );
            w::SetCursor(cur);
            1
        }
        w::WM_LBUTTONDOWN => match client_to_global(hwnd, lparam) {
            Some((gx, gy)) => {
                if !inside_octagon(hwnd, gx, gy) {
                    crate::slog(55, "gadget_down_outside");
                    return 1; // margen transparente: sin acción
                }
                let mut app = crate::APP.lock().unwrap();
                if let Some(a) = app.as_mut() {
                    let mut wr: w::RECT = std::mem::zeroed();
                    w::GetWindowRect(hwnd, &mut wr);
                    a.gadget.press = Some((gx, gy));
                    a.gadget.win_pos = Some((wr.left, wr.top));
                    a.gadget.dragging = false;
                }
                w::SetCapture(hwnd);
                1
            }
            None => 1,
        },
        w::WM_MOUSEMOVE => {
            // SetWindowPos FUERA del lock de APP (se ejecuta después de
            // soltarlo): durante una reconfiguración de displays user32
            // puede bloquearse (AppHangB1, WER 2026-10-21 10:56).
            let mut move_to: Option<(i32, i32)> = None;
            {
                let mut app = crate::APP.lock().unwrap();
                if let Some(a) = app.as_mut() {
                    if let Some((px, py)) = a.gadget.press {
                        if let Some((gx, gy)) = client_to_global(hwnd, lparam) {
                            let (dx, dy) = (gx - px, gy - py);
                            if !a.gadget.dragging && dx.abs() + dy.abs() > DRAG_THRESHOLD {
                                a.gadget.dragging = true;
                            }
                            if a.gadget.dragging {
                                if let Some((wx, wy)) = a.gadget.win_pos {
                                    let (mut nx, mut ny) = (wx + dx, wy + dy);
                                    // Topes: work area del monitor actual (el
                                    // margen del cartelito puede quedar fuera).
                                    let mon =
                                        w::MonitorFromWindow(hwnd, w::MONITOR_DEFAULTTONEAREST);
                                    let mut mi: w::MONITORINFO = std::mem::zeroed();
                                    mi.cbSize = std::mem::size_of::<w::MONITORINFO>() as u32;
                                    if w::GetMonitorInfoW(mon, &mut mi) != 0 {
                                        let av = mi.rcWork;
                                        let mut wr: w::RECT = std::mem::zeroed();
                                        if w::GetWindowRect(hwnd, &mut wr) != 0 {
                                            let (cw, ch) =
                                                (wr.right - wr.left, wr.bottom - wr.top);
                                            let (_, cyg, r, _) = draw::octagon_geom(cw, ch);
                                            // Tope superior: monitor COMPLETO (no
                                            // rcWork) para que el octágono llegue
                                            // al borde de la pantalla.
                                            let top_floor = mi.rcMonitor.top
                                                - (cyg.round() as i32 - r.round() as i32);
                                            nx = nx.max(av.left).min(av.right - cw);
                                            ny = ny.max(top_floor).min(av.bottom - ch);
                                        }
                                    }
                                    move_to = Some((nx, ny));
                                }
                            }
                        }
                    } else {
                        // Cursor "manito" sobre el octágono: la ventana vive al
                        // fondo del z-order (no es la ventana activa) y Windows
                        // no le envía WM_SETCURSOR, así que se setea en el propio
                        // move. (El handler WM_SETCURSOR queda como respaldo.)
                        if let Some((gx, gy)) = client_to_global(hwnd, lparam) {
                            let hand = inside_octagon(hwnd, gx, gy);
                            if hand != a.gadget.cursor_hand {
                                a.gadget.cursor_hand = hand;
                                let cur = w::LoadCursorW(
                                    std::ptr::null_mut(),
                                    if hand { w::IDC_HAND } else { w::IDC_ARROW },
                                );
                                w::SetCursor(cur);
                            }
                        }
                    }
                }
            }
            if let Some((nx, ny)) = move_to {
                unsafe {
                    let _ = w::SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        nx,
                        ny,
                        0,
                        0,
                        w::SWP_NOSIZE | w::SWP_NOZORDER | w::SWP_NOACTIVATE,
                    );
                }
            }
            1
        }
        w::WM_LBUTTONUP => {
            let toggle = {
                let mut app = crate::APP.lock().unwrap();
                let Some(a) = app.as_mut() else {
                    return 1;
                };
                if a.gadget.press.is_none() {
                    crate::slog(56, "gadget_up_no_press");
                    false
                } else {
                    w::ReleaseCapture();
                    let was_drag = a.gadget.dragging;
                    a.gadget.press = None;
                    a.gadget.dragging = false;
                    if was_drag {
                        // Arrastre: guardar la posición final en coordenadas
                        // lógicas (÷ escala principal).
                        let mut wr: w::RECT = std::mem::zeroed();
                        if w::GetWindowRect(a.gadget.hwnd, &mut wr) != 0 {
                            let s = a.gadget.scale.max(0.1);
                            a.cfg.position = Some([
                                ((wr.left as f32) / s).round() as i64,
                                ((wr.top as f32) / s).round() as i64,
                            ]);
                            // Memoria por monitor: posición relativa al
                            // monitor donde quedó (÷ escala de ese monitor).
                            let mon =
                                w::MonitorFromWindow(a.gadget.hwnd, w::MONITOR_DEFAULTTONEAREST);
                            let mut mi: w::MONITORINFO = std::mem::zeroed();
                            mi.cbSize = std::mem::size_of::<w::MONITORINFO>() as u32;
                            if w::GetMonitorInfoW(mon, &mut mi) != 0 {
                                let device = w::from_wchar(&mi.szDevice);
                                let id = crate::hdr::monitor_id_for_device(&device);
                                // v6: clave compuesta "id|escala%": los
                                // monitores del usuario comparten el mismo
                                // ID PnP pero escalas distintas (100% vs
                                // 200%), así que el id solo no distingue a
                                // cuál corresponde la posición.
                                a.cfg.monitor_mem.insert(
                                    crate::mem_key(&id, s),
                                    [
                                        (((wr.left - mi.rcMonitor.left) as f32) / s).round() as i64,
                                        (((wr.top - mi.rcMonitor.top) as f32) / s).round() as i64,
                                    ],
                                );
                                a.monitor_id = id;
                            }
                            crate::config::save(&a.cfg);
                        }
                        false
                    } else {
                        // Traba anti doble-toggle: la 2ª release dentro de
                        // SM_DBLCLKTIME se traga (doble click = 1 acción).
                        let dbl = (w::GetSystemMetrics(w::SM_DBLCLKTIME) as u64).max(1);
                        let now = Instant::now();
                        let ok = a
                            .gadget
                            .last_toggle
                            .map(|t| now.duration_since(t).as_millis() >= dbl as u128)
                            .unwrap_or(true);
                        if ok {
                            a.gadget.last_toggle = Some(now);
                            true
                        } else {
                            crate::slog(57, "gadget_toggle_suppressed");
                            false
                        }
                    }
                }
            };
            if toggle {
                crate::slog(58, "gadget_toggle_emit");
                crate::request_toggle();
            }
            1
        }
        w::WM_LBUTTONDBLCLK => 1, // nunca se genera; por si acaso
        w::WM_DPICHANGED => {
            on_dpi_changed(hwnd, wparam);
            0
        }
        _ => w::DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
