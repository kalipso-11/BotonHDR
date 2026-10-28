// App sin consola (subsystem GUI): es un tray, no debe abrir una ventana
// negra. (Los eprintln! de arranque solo se ven si se corre desde una
// terminal con -Zunstable-options o re-dirigiendo; no afectan nada.)
#![windows_subsystem = "windows"]

// Es un bin único: se declara una superficie de API (constantes GDI,
// helpers, funciones aún no cableadas). Se silencia el dead_code de esa
// superficie; el resto de warnings se dejan vivos.
#![allow(dead_code)]

//! Botón HDR/SDR: icono en la bandeja + gadget octagonal en el escritorio.
//!
//! Win32/GDI por FFI cruda (sin crates); el render es 100% por software
//! (ver `draw.rs`):
//!
//! - Una ventana de control OCULTA bombea todos los mensajes (timers,
//!   callback de bandeja, toggle terminado, salida).
//! - Estado global único tras `Mutex<Option<App>>`. Regla de oro:
//!   NUNCA mantener el lock a través de `TrackPopupMenu` (bombea
//!   mensajes → deadlock).
//! - El toggle de HDR corre en un hilo aparte (puede tardar ~8 s) y
//!   avisa con `PostMessageW`; jamás toca el lock.

mod autostart;
mod config;
mod draw;
mod gadget;
mod gamebar;
mod hdr;
mod lang;
mod singleinstance;
mod tray;
mod traytip;
mod win;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Estado global (toda la app vive aquí).
pub static APP: Mutex<Option<App>> = Mutex::new(None);

/// ¿Un worker de consulta de estado está en curso? (fuera del lock: el
/// worker jamás toca `APP`).
static STATE_CHECKING: AtomicBool = AtomicBool::new(false);
/// Milisegundos (epoch) en que empezó la consulta en curso (timeout 10 s).
static STATE_CHECK_SINCE: AtomicU64 = AtomicU64::new(0);

/// ¿Un ajuste de cambio de display está corriendo? (fuera de APP):
/// serializa los ajustes para que una ráfaga de WM_DISPLAYCHANGE no
/// entrecruze dos workers de display.
static DISP_BUSY: AtomicBool = AtomicBool::new(false);

/// Firma topológica del último cambio de display PROCESADO: (id del
/// monitor, escala, rectángulo del monitor). Seme al arrancar. Sirve para
/// detectar GDI STALE tras un hot-swap: si lo que reporta GDI justo después
/// del WM_DISPLAYCHANGE es IDÉNTICO a esta firma, probablemente sigue
/// reportando el monitor VIEJO (hasta ~7 s observado) y hay que seguir
/// observando hasta que aparezca el estado real.
static LAST_DISP: Mutex<Option<(String, f32, i32, i32, i32, i32)>> = Mutex::new(None);

/// Firma topológica de un `MonInfo` (comparar estabilidad de GDI).
fn disp_sig(p: &MonInfo) -> (String, f32, i32, i32, i32, i32) {
    (p.id.clone(), p.scale, p.mon.left, p.mon.top, p.mon.right, p.mon.bottom)
}

/// ¿Difieren dos topologías (monitor, escala o rectángulo)?
fn sig_differs(
    a: &(String, f32, i32, i32, i32, i32),
    b: &(String, f32, i32, i32, i32, i32),
) -> bool {
    a.0 != b.0 || (a.1 - b.1).abs() > 0.01 || a.2 != b.2 || a.3 != b.3 || a.4 != b.4 || a.5 != b.5
}

/// Escala DWM (GetDpiForWindow) de una ventana: la escala del monitor
/// donde vive, leída del lado de DWM (fresca en segundos). El cache de
/// GDI de monitores (EnumDisplayDevices/GetMonitorInfoW/GetDpiForMonitor)
/// puede quedarse STALE HORAS tras un hot-swap (log 2026-10-27: una
/// muestra chimérica "estable" duró ~1.8 h) — DWM no.
fn dwm_probe(hwnd: *mut c_void) -> Option<f32> {
    if hwnd.is_null() {
        return None;
    }
    let dpi = unsafe { win::GetDpiForWindow(hwnd) };
    (dpi > 0).then_some(dpi as f32 / 96.0)
}

/// ID del monitor donde vive una ventana (MonitorFromWindow +
/// GetMonitorInfoW + monitor_id_for_device). El id sale del MISMO cache
/// GDI que el de la muestra: la comparación muestra-vs-ventana es
/// consistente aunque ambos estén stale (el mismo monitor da el mismo
/// szDevice en ambos mundos).
fn window_monitor_id(hwnd: *mut c_void) -> Option<String> {
    if hwnd.is_null() {
        return None;
    }
    unsafe {
        let hm = win::MonitorFromWindow(hwnd, win::MONITOR_DEFAULTTONEAREST);
        if hm.is_null() {
            return None;
        }
        let mut mi: win::MONITORINFO = std::mem::zeroed();
        mi.cbSize = core::mem::size_of::<win::MONITORINFO>() as u32;
        if win::GetMonitorInfoW(hm, &mut mi) == 0 {
            return None;
        }
        Some(hdr::monitor_id_for_device(&win::from_wchar(&mi.szDevice)))
    }
}

/// v7 — Verificación DWM de una muestra de GDI que declara un FLIP
/// (escala/id distinta de la firma base) SIN WM_DPICHANGED fresco.
/// Devuelve la ESCALA a usar si la muestra es accionable; None = no
/// actuar con esa muestra (chimérica).
///
/// Motivo: tras el hot-swap, una muestra chimérica (id viejo + escala
/// nueva, o al revés) puede mantenerse "estable" HORAS — la regla v6
/// "estable 12 s ⇒ GDI despierto, actuar" actuaba justo con esa muestra
/// (log 2026-10-27: swap de regreso actuó con ACR0490|1.0, imposible en
/// hardware → move a memoria corrupta + gadget a medio tamaño 1.8 h).
/// La verdad de escala la da DWM vía GetDpiForWindow, con dos sondas:
/// - ventana de CONTROL: vive en (0,0) ⇒ SIEMPRE sobre el primario real;
///   su DPI es la escala del primario real;
/// - ventana del GADGET: su DPI es la escala del monitor donde vive.
///
/// Reglas:
/// - El gadget está sobre el monitor que la muestra declara primario
///   (y EN PANTALLA, v7.1): el DPI del gadget es la verdad de ESE
///   monitor → actuar con él. Si el id de la muestra era stale (swap de
///   regreso), la memoria del (monitor|escala) real round-tripkea la
///   posición actual del gadget ⇒ el "move" queda en no-op y el gadget
///   no se toca. v7.1: si el gadget está FUERA de la pantalla virtual
///   (su monitor se desconectó), MonitorFromWindow TONEAREST da un
///   monitor ARBITRARIO (el id stale puede coincidir por casualidad con
///   el de la muestra) y el DPI del gadget puede estar STALE (una
///   ventana cuyo monitor se desconectó mantiene el DPI viejo unos
///   segundos — log 2026-10-27: dw_g=2.0 stale 5 ms antes del
///   WM_DPICHANGED dpi=96 → move con escala equivocada + gadget a
///   medio tamaño). En ese caso la regla 1 queda deshabilitada.
/// - El gadget está en otro monitor (o fuera de pantalla, v7.1): la
///   escala del primario declarado solo la valida la ventana de
///   control; coincide → actuar con la escala de la muestra (id stale
///   degrada a no-op, misma razón); no coincide → muestra chimérica →
///   no actuar (esperar; el timeout de 60 s resuelve de forma
///   conservadora, sin mover).
fn verify_flip_scale(
    sample: &MonInfo,
    hwnd_gadget: *mut c_void,
    hwnd_ctrl: *mut c_void,
) -> Option<f32> {
    let dw_gadget = dwm_probe(hwnd_gadget)?;
    // v7.1: la regla 1 solo vale con el gadget EN PANTALLA (ver doc).
    if gadget_on_screen(hwnd_gadget)
        && window_monitor_id(hwnd_gadget).as_deref() == Some(sample.id.as_str())
    {
        return Some(dw_gadget);
    }
    match dwm_probe(hwnd_ctrl) {
        Some(c) if (c - sample.scale).abs() <= 0.01 => Some(sample.scale),
        _ => None,
    }
}

/// ¿Está la ventana dentro de la pantalla virtual? (GetWindowRect +
/// SM_*VIRTUALSCREEN; misma fórmula que `disp_apply`/
/// `ensure_gadget_ok`). `false` si no se pudo leer (conservador: quien
/// decide lo trata como off-screen).
fn gadget_on_screen(hwnd: *mut c_void) -> bool {
    if hwnd.is_null() {
        return false;
    }
    let mut wr = unsafe { std::mem::zeroed::<win::RECT>() };
    if unsafe { win::GetWindowRect(hwnd, &mut wr) } == 0 {
        return false;
    }
    let (vx0, vy0) = unsafe {
        (
            win::GetSystemMetrics(win::SM_XVIRTUALSCREEN),
            win::GetSystemMetrics(win::SM_YVIRTUALSCREEN),
        )
    };
    let (vw, vh) = unsafe {
        (
            win::GetSystemMetrics(win::SM_CXVIRTUALSCREEN),
            win::GetSystemMetrics(win::SM_CYVIRTUALSCREEN),
        )
    };
    !(wr.right <= vx0 || wr.left >= vx0 + vw || wr.bottom <= vy0 || wr.top >= vy0 + vh)
}

/// Milisegundos desde la época (para el timeout, sin cronómetro propio).
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Clave de la memoria por monitor (v6): "id|escala%". Los monitores del
/// usuario comparten el mismo ID PnP (ACR0490) pero escalas distintas
/// (100% vs 200%), así que el id solo no distingue a cuál corresponde la
/// posición recordada: la clave compuesta sí.
pub(crate) fn mem_key(id: &str, scale: f32) -> String {
    format!("{}|{}", id, (scale.max(0.1) * 100.0).round() as i32)
}

/// v7 — Parsea una clave de memoria compuesta "id|escala%" → (id,
/// escala). `None` si la clave es legacy (solo id) o la escala no se
/// puede parsear (id vacío o porcentaje no numérico).
pub(crate) fn parse_mem_key(k: &str) -> Option<(String, f32)> {
    let (id, pct) = k.rsplit_once('|')?;
    if id.is_empty() {
        return None;
    }
    let pct: i32 = pct.parse().ok()?;
    Some((id.to_string(), (pct as f32 / 100.0).max(0.1)))
}

// v7 — Escalas efectivas (id → escala) de los monitores conectados,
// leídas del cache GDI de monitores (EnumDisplayMonitors). La
// enumeración es SÍNCRONA (el callback corre en el thread actual) y un
// `MonEnumProc` no puede capturar: se acumula en thread_local y se
// drena al volver.
thread_local! {
    static MON_SCALES: std::cell::RefCell<std::collections::BTreeMap<String, f32>> =
        std::cell::RefCell::new(std::collections::BTreeMap::new());
}

unsafe extern "system" fn mon_scale_cb(
    hmon: *mut c_void,
    _hdc: *mut c_void,
    _lprc: *mut win::RECT,
    _lparam: isize,
) -> i32 {
    let mut mi: win::MONITORINFO = std::mem::zeroed();
    mi.cbSize = core::mem::size_of::<win::MONITORINFO>() as u32;
    if win::GetMonitorInfoW(hmon, &mut mi) == 0 {
        return 1;
    }
    let id = hdr::monitor_id_for_device(&win::from_wchar(&mi.szDevice));
    let mut dpi: u32 = 0;
    let _ =
        win::GetDpiForMonitor(hmon, win::MDT_EFFECTIVE_DPI, &mut dpi, std::ptr::null_mut());
    let scale = if dpi > 0 {
        dpi as f32 / 96.0
    } else {
        1.0
    };
    MON_SCALES.with(|m| m.borrow_mut().insert(id, scale));
    1
}

fn current_scales_by_id() -> std::collections::BTreeMap<String, f32> {
    MON_SCALES.with(|m| m.borrow_mut().clear());
    unsafe {
        win::EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null_mut(), mon_scale_cb, 0);
    }
    MON_SCALES.with(|m| m.borrow().clone())
}

/// v7 — Migración one-shot al arrancar: purga claves de memoria
/// IMPOSIBLES.
///
/// El bug pre-v7 escribía claves chiméricas (id|escala que el hardware
/// no soporta — log 2026-10-27: `ACR0490|100` mientras ese panel está a
/// 200%), y el gadget luego fue movido ahí y se quedó roto horas
/// ("no respeta la posición"). Una clave `id|escala` es imposible si,
/// AHORA, ese id tiene otra escala.
///
/// La verdad es la escala EN VIVO (EnumDisplayMonitors) + la escala
/// primario corregida por DWM (paso 6.5): si el cache GDI de monitores
/// está stale al arrancar (observado: arranque en 4K@200% reportando
/// 100%), el override del primario evita purgar por error. Trade-off
/// aceptado: si el usuario cambia la escala de un monitor en Windows y
/// vuelve, pierde esa posición (se recupera con un arrastre).
///
/// Re-clavea también entradas legacy (solo id) a `id|escala_actual`
/// (misma intención que el paso 7.5, pero por id y no solo el primario).
/// Si la enumeración falla (map vacío), NO se fija `migrated_v7`:
/// reintento en el próximo arranque.
fn migrate_monitor_mem_v7(cfg: &mut config::Cfg, prim_id: &str, prim_scale: f32) {
    if cfg.migrated_v7.is_some() {
        return;
    }
    let mut scales = current_scales_by_id();
    if scales.is_empty() {
        return;
    }
    if !prim_id.is_empty() {
        scales.insert(prim_id.to_string(), prim_scale);
    }
    let mut purged = 0usize;
    let mut rekeyed = 0usize;
    let keys: Vec<String> = cfg.monitor_mem.keys().cloned().collect();
    for k in keys {
        match parse_mem_key(&k) {
            Some((id, s)) => {
                // Clave compuesta: se purga si la escala no es la actual
                // de ese id (la escribió el bug chimérico).
                if let Some(cur) = scales.get(&id) {
                    if (cur - s).abs() > 0.01 {
                        cfg.monitor_mem.remove(&k);
                        purged += 1;
                    }
                }
            }
            None => {
                if !k.contains('|') {
                    // Legacy (solo id): re-clavear con la escala actual
                    // de ese id. Si el id no se identifica (monitor
                    // desconectado), se deja para un re-claveo futuro
                    // (paso 7.5 si es el primario, o el próximo arranque).
                    if let Some(s) = scales.get(&k) {
                        let nk = mem_key(&k, *s);
                        if cfg.monitor_mem.contains_key(&nk) {
                            cfg.monitor_mem.remove(&k);
                        } else {
                            let v = cfg.monitor_mem.remove(&k).unwrap_or_default();
                            cfg.monitor_mem.insert(nk, v);
                            rekeyed += 1;
                        }
                    }
                } else {
                    // Corrupta ("id|zzz"): no puede ser una clave válida.
                    cfg.monitor_mem.remove(&k);
                    purged += 1;
                }
            }
        }
    }
    cfg.migrated_v7 = Some(1);
    if purged > 0 || rekeyed > 0 {
        config::save(cfg);
        slog(5, &format!("mem_v7_purged={} rekeyed={}", purged, rekeyed));
    }
}

/// Log de arranque por pasos → %TEMP%\botonhdr_start.log (diagnóstico:
/// si la app "no responde", la última línea dice en qué paso se quedó).
/// Mutex global: varios hilos (workers) escriben a la vez; sin él las
/// líneas se intercalan a media línea y se vuelven ilegibles.
static SLOG_LOCK: Mutex<()> = Mutex::new(());

fn slog(step: u8, label: &str) {
    use std::io::Write;
    let _g = SLOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = crate::config::log_path();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "t={} step={} {}", now_ms(), step, label);
    }
}

pub struct App {
    /// La mutex debe durar lo que el proceso (no dejar de vivir).
    pub single: singleinstance::SingleInstance,
    pub cfg: config::Cfg,
    /// "HDR" | "SDR" (constantes de `hdr`).
    pub state: &'static str,
    /// Toggle en curso en el worker (traba todos los demás).
    pub busy: bool,
    /// Cuándo empezó el toggle en curso (watchdog: >20 s = worker colgado).
    pub busy_since: Option<Instant>,
    pub hwnd_ctrl: *mut c_void,
    pub gadget: gadget::GadgetState,
    pub tray: tray::Tray,
    pub tip: traytip::TipState,
    /// Último toggle desde la bandeja (guarda anti doble-click).
    pub last_tray_toggle: Option<Instant>,
    /// dpi/96 del monitor principal (para coords lógicas de config).
    pub primary_scale: f32,
    /// ID del monitor donde vive el gadget (clave de `cfg.monitor_mem`).
    pub monitor_id: String,
    // --- tip de la bandeja (ventana propia de 2 líneas) ---
    pub tip_suppressed: bool,
    pub tip_armed: bool,
    pub tip_overflow: bool,
    // --- bloqueo de estado (menú "Bloquear SDR/HDR") ---
    /// ¿Lock activo? (el estado no se puede cambiar de ningún lado).
    pub locked: bool,
    /// Estado bloqueado ("HDR"/"SDR"); mientras hay lock, state == lock_state.
    pub lock_state: &'static str,
}

// Safe: App vive solo en el hilo de mensajes (el worker de toggle jamás
// toca el lock; PostMessageW es thread-safe). Los campos con punteros
// crudos (hwnd, buffers GDI) se crean/destruyen en ese mismo hilo.
unsafe impl Send for App {}

// Mensajes propios de la ventana de control.
const WM_TOGGLE_DONE: u32 = win::WM_APP + 2; // wParam = ok (0/1)
const WM_STATE_DONE: u32 = win::WM_APP + 3; // wParam = 1 si HDR, 0 si SDR

// IDs de timers.
const T_STATE: usize = 1; // 2000 ms: re-lee el estado HDR/SDR
const T_SHOW: usize = 2; // 200 ms: señal de 2ª instancia
const T_TIP: usize = 3; // 120 ms: hover del icono de bandeja
const T_WARN: usize = 4; // 3500 ms one-shot: aviso en el gadget
const T_TIP_SHOW: usize = 5; // 350 ms one-shot: mostrar el tip
const T_DISP: usize = 6; // 300 ms one-shot: reajuste tras cambio de display

/// Hook de panic permanente: con `panic = "abort"` el hook corre ANTES
/// del abort, así que queda registrado en el log de arranque dónde y por
/// qué cayó la app (sin depender del dump de WER, que solo da el código
/// de excepción).
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            String::from("payload desconocido")
        };
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        let mut s = format!("\nPANIC t={} @ {loc}: {msg}\n", now_ms());
        let bt = std::backtrace::Backtrace::force_capture();
        s.push_str(&bt.to_string());
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(crate::config::log_path())
        {
            let _ = f.write_all(s.as_bytes());
        }
    }));
}

// ---------------------------------------------------------------------------
// API pública (la usan gadget.rs / tray.rs)
// ---------------------------------------------------------------------------

/// Alterna HDR/SDR en un worker y notifica por `WM_TOGGLE_DONE`.
///
/// Sin parámetro de hwnd a propósito: el resultado se notifica a la
/// ventana de CONTROL (la única que maneja `WM_TOGGLE_DONE`). Si se
/// pasaba el hwnd del que origina el click, un click en el gadget
/// postaba al gadget, `on_toggle_done` nunca corría y `busy` quedaba
/// trabado para siempre (bug de la versión inicial corregido).
pub fn request_toggle() {
    let (target, hwnd_ctrl) = {
        let mut app = APP.lock().unwrap();
        let Some(a) = app.as_mut() else {
            return;
        };
        if a.locked {
            // Estado BLOQUEADO: no se alterna (ni por click ni por
            // bandeja). Aviso en el gadget y balloon si está oculto.
            slog(74, &format!("toggle_blocked_{}", a.lock_state));
            let msg = format!("{} bloqueado", a.lock_state);
            a.gadget.show_warning(&msg);
            unsafe {
                win::SetTimer(a.hwnd_ctrl, T_WARN, 3500, std::ptr::null());
            }
            if !a.gadget.visible {
                a.tray.balloon(lang::t(&a.cfg.language, "Botón HDR", "HDR Button"), &msg);
            }
            return;
        }
        if a.busy {
            // Un toggle previo sigue en curso (o su worker se quedó
            // colgado): el click no haría nada, se deja constancia.
            slog(50, "toggle_skip_busy");
            return;
        }
        a.busy = true;
        a.busy_since = Some(Instant::now());
        (
            if a.state == hdr::HDR {
                hdr::SDR
            } else {
                hdr::HDR
            },
            a.hwnd_ctrl,
        )
    };
    if hwnd_ctrl.is_null() {
        // Cerrando: la ventana de control ya no existe.
        let mut app = APP.lock().unwrap();
        if let Some(a) = app.as_mut() {
            a.busy = false;
            a.busy_since = None;
        }
        return;
    }
    slog(51, "toggle_start");
    let hwnd_s = win::Shared::new(hwnd_ctrl);
    let t0 = Instant::now();
    std::thread::spawn(move || {
        let ok = hdr::set_state(target);
        slog(52, &format!("toggle_done ok={} ms={}", ok, t0.elapsed().as_millis()));
        unsafe {
            win::PostMessageW(
                hwnd_s.ptr(),
                WM_TOGGLE_DONE,
                ok as usize,
                usize::from(target == hdr::HDR) as isize,
            );
        }
    });
}

/// ID del hotkey global (Win+Alt+B, capturado mientras un estado está
/// bloqueado: mientras lo retiene la app, el sistema no puede cambiar
/// el HDR con el atajo; solo un proceso puede retener un hotkey).
const HOTKEY_ID: i32 = 1;

fn set_hotkey(hwnd: *mut c_void, on: bool) {
    unsafe {
        if on {
            let ret = win::RegisterHotKey(
                hwnd,
                HOTKEY_ID,
                win::MOD_WIN | win::MOD_ALT,
                win::VK_B,
            );
            if ret == 0 {
                // Otro proceso retiene el hotkey: la sentinela cubre igual
                // (revierte en ~120 ms cualquier cambio ajeno).
                slog(76, &format!("hotkey_reg_fail gle={}", win::GetLastError()));
            }
        } else {
            win::UnregisterHotKey(hwnd, HOTKEY_ID);
        }
    }
}

/// Win+Alt+B capturado (lock activo): el atajo del sistema ya no puede
/// cambiar el estado; solo se avisa.
fn on_hotkey() {
    let mut app = APP.lock().unwrap();
    let Some(a) = app.as_mut() else {
        return;
    };
    if !a.locked {
        return;
    }
    slog(75, &format!("hotkey_blocked_{}", a.lock_state));
    let msg = format!("{} bloqueado", a.lock_state);
    a.gadget.show_warning(&msg);
    unsafe {
        win::SetTimer(a.hwnd_ctrl, T_WARN, 3500, std::ptr::null());
    }
    if !a.gadget.visible {
        a.tray.balloon(lang::t(&a.cfg.language, "Botón HDR", "HDR Button"), &msg);
    }
}

/// Alterna el bloqueo de estado (menú "Bloquear SDR/HDR").
///
/// Bloquear congela el estado ACTUAL: ni el usuario (clicks), ni el
/// sistema (Win+Alt+B, capturado), ni otra app pueden cambiarlo (la
/// sentinela lo revierte en ~120 ms con la API directa, sin atajo →
/// sin toast de Game Bar). Persiste en `cfg.hdr_lock` y se re-aplica en
/// el próximo arranque.
pub fn toggle_lock() {
    let mut app = APP.lock().unwrap();
    let Some(a) = app.as_mut() else {
        return;
    };
    if a.busy {
        // Toggle en curso: no capturar el lock sobre un estado intermedio
        // (el toggle termina y la UI re-valida en 2 s; el usuario puede
        // reintentar).
        slog(81, "lock_skip_busy");
        return;
    }
    if a.locked {
        a.locked = false;
        a.lock_state = hdr::SDR;
        a.cfg.hdr_lock = None;
        config::save(&a.cfg);
        a.gadget.set_locked(false);
        refresh_icon(a);
        set_hotkey(a.hwnd_ctrl, false);
        slog(77, "lock_off");
        a.gadget.clear_warning();
    } else {
        a.locked = true;
        a.lock_state = a.state;
        a.cfg.hdr_lock = Some(a.state.to_string());
        config::save(&a.cfg);
        a.gadget.set_locked(true);
        refresh_icon(a);
        set_hotkey(a.hwnd_ctrl, true);
        slog(78, &format!("lock_on_{}", a.lock_state));
        let msg = format!("{} bloqueado", a.lock_state);
        a.gadget.show_warning(&msg);
        unsafe {
            win::SetTimer(a.hwnd_ctrl, T_WARN, 3500, std::ptr::null());
        }
    }
}

/// Sentinela del lock (hilo dedicado): mientras hay estado bloqueado,
/// re-lee el estado real cada ~120 ms y lo revierte con
/// `hdr::set_state_fast` (API DisplayConfig directa, sin atajo → sin
/// toast de Game Bar). Nunca toca APP para reverter (solo un snapshot
/// breve); si una app ajena cambia el estado, el parpadeo dura ~120 ms
/// (aceptado; la captura del hotkey cubre el camino más común).
fn lock_sentinel() {
    loop {
        let snap = {
            let app = APP.lock().unwrap();
            app.as_ref().map(|a| (a.locked, a.lock_state))
        };
        let Some((locked, target)) = snap else {
            break; // app destruida (cleanup)
        };
        if locked {
            let (st, _) = hdr::get_state();
            if st != Some(target) {
                slog(79, &format!("lock_revert_to_{}", target));
                let _ = hdr::set_state_fast(target);
            }
            std::thread::sleep(std::time::Duration::from_millis(120));
        } else {
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    }
}

/// Watchdog de diagnóstico (cuelgue del 2026-10-21): heartbeat cada 10 s
/// + chequeo del lock de APP con timeout. Si APP no se libera en 500 ms
/// se loguea 91 (alguien lo retiene; junto a las líneas 45-49 identifica
/// al hilo trabado: 46 sin 47 = mensaje en on_display_change; 91 sin
/// 46/47 = un worker con APP). Si el hilo de mensajes muere sin soltar
/// el lock, el 91 se repite hasta el fin (AppHangB1).
fn watchdog() {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(10));
        // try_lock_for no existe en std (es de parking_lot): 500 ms de
        // gracia con reintentos (evita falsos positivos con holds normales).
        let mut g = APP.try_lock().ok();
        for _ in 0..10 {
            if g.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            g = APP.try_lock().ok();
        }
        match g {
            Some(guard) => {
                if guard.is_some() {
                    slog(99, "hb");
                }
            }
            None => {
                slog(91, "app_lock_busy");
            }
        }
    }
}

/// Refresca icono de bandeja + estado del tip (y tip nativo si overflow).
pub(crate) fn refresh_icon(a: &mut App) {
    a.tray.refresh(a.state, &a.cfg.color, a.locked);
    a.tip.set_state(a.state);
    if a.tip_overflow {
        a.tray.set_native_tip(Some(tray::native_tip_text(a.state)));
    }
}

// ---------------------------------------------------------------------------
// WNDPROC de la ventana de control
// ---------------------------------------------------------------------------

const CLASS_CTRL: &str = "BotonHDRCtrl";

unsafe extern "system" fn ctrl_wndproc(
    hwnd: *mut c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    match msg {
        win::WM_TIMER => {
            handle_timer(hwnd, wparam);
            0
        }
        tray::WM_TRAY => {
            // WM_MOUSEMOVE (0x200) NO se loguea: satura el archivo
            // (cada hover del cursor sobre la bandeja).
            let lp = lparam as u32;
            if lp != win::WM_MOUSEMOVE {
                slog(30, &format!("tray_msg lp={:#x}", lp));
            }
            match lp {
                win::WM_LBUTTONUP => {
                    let _ = tray_lbuttonup();
                }
                win::WM_LBUTTONDBLCLK => tray_lbuttondblclick(),
                win::WM_RBUTTONUP | win::WM_CONTEXTMENU => tray::open_menu(hwnd),
                _ => {}
            }
            0
        }
        WM_TOGGLE_DONE => {
            on_toggle_done(wparam != 0, lparam != 0);
            0
        }
        WM_STATE_DONE => {
            let st = if wparam != 0 { hdr::HDR } else { hdr::SDR };
            let mut app = APP.lock().unwrap();
            if let Some(a) = app.as_mut() {
                // Con lock activo se ignoran las actualizaciones que lo
                // contradicen (la sentinela es la que revierte).
                if !a.busy && st != a.state && !(a.locked && st != a.lock_state) {
                    a.state = st;
                    a.gadget.set_state(st);
                    refresh_icon(a);
                }
            }
            0
        }
        win::WM_HOTKEY => {
            on_hotkey();
            0
        }
        win::WM_CLOSE => {
            win::PostQuitMessage(0);
            0
        }
        win::WM_DISPLAYCHANGE => {
            // Cambió resolución/monitor. Se reajusta con 300 ms de
            // retraso (one-shot): el broadcast llega en plena
            // reconfiguración y a veces en ráfaga (hotplug).
            slog(43, "display_change");
            // v6: (a) invalidar el hint WM_DPICHANGED: solo un
            // DPICHANGED que llegue DESPUÉS de este evento valida datos
            // frescos; uno del swap ANTERIOR (aún con <20 s de edad) abría
            // la puerta de escala contra muestras stale del swap nuevo.
            // (b) descartar reposiciones en cola calculadas contra el
            // mundo viejo (el log mostró un move ejecutándose después del
            // swap sobre el monitor nuevo).
            gadget::wm_hint_invalidate();
            if gadget::take_pending_move().is_some() {
                slog(47, "stale_move_cleared");
            }
            unsafe {
                win::KillTimer(hwnd, T_DISP);
                win::SetTimer(hwnd, T_DISP, 300, std::ptr::null());
            }
            0
        }
        _ => win::DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn create_ctrl_window() -> *mut c_void {
    unsafe {
        let inst = win::GetModuleHandleW(std::ptr::null());
        let cls = win::utf16(CLASS_CTRL);
        let wcx = win::WNDCLASSEXW {
            cbSize: std::mem::size_of::<win::WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: ctrl_wndproc,
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
        let _ = win::RegisterClassExW(&wcx);
        let name = win::utf16("Botón HDR (control)");
        win::CreateWindowExW(
            win::WS_EX_TOOLWINDOW,
            cls.as_ptr(),
            name.as_ptr(),
            win::WS_POPUP,
            0,
            0,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            inst,
            std::ptr::null(),
        )
    }
}

// ---------------------------------------------------------------------------
// Timers (todos en el hilo de mensajes; toman y sueltan el lock al return)
// ---------------------------------------------------------------------------

fn handle_timer(hwnd: *mut c_void, id: usize) {
    match id {
        T_STATE => refresh_state(),
        T_SHOW => handle_show_signal(),
        T_TIP => tip_tick(hwnd),
        T_WARN => {
            if let Ok(mut app) = APP.lock() {
                if let Some(a) = app.as_mut() {
                    a.gadget.clear_warning();
                }
            }
            unsafe {
                win::KillTimer(hwnd, T_WARN);
            }
        }
        T_TIP_SHOW => {
            // One-shot: se desarma al dispararse.
            unsafe {
                win::KillTimer(hwnd, T_TIP_SHOW);
            }
            tip_show();
        }
        T_DISP => {
            unsafe {
                win::KillTimer(hwnd, T_DISP);
            }
            on_display_change();
        }
        _ => {}
    }
}

/// Tras un cambio de resolución/monitor (WM_DISPLAYCHANGE + 300 ms).
///
/// El trabajo corre en un HILO EFÍMERO (no en el de mensajes): durante la
/// reconfiguración el stack de video satura GDI (se observó GetWindowRect
/// de 1.4 s) y `primary_monitor()` puede fallar unos segundos tras el
/// hotplug; en el hilo de mensajes eso congelaría timers, bandeja y
/// arrastre (AppHangB1, WER 2026-10-21 10:56).
///
/// El worker (`disp_apply`):
/// 1) reintenta `primary_monitor()` hasta ~3 s; resuelve el estado con
///    PUERTA DE ESCALA (`resolve_display_state`): una muestra GDI que
///    contradice un WM_DPICHANGED fresco (≤20 s) jamás se actúa; un flip
///    de escala/id SIN WM fresco se verifica contra DWM
///    (`verify_flip_scale`, sondas GetDpiForWindow de gadget y ventana de
///    control) — v7: una muestra chimérica "estable" puede durar HORAS
///    (log 2026-10-27), así que la estabilidad sola ya no basta; si a los
///    60 s nada se verificó → actuar de forma conservadora, sin mover;
///    v7.1: la regla "gadget sobre el monitor declarado" de la
///    verificación solo vale con el gadget EN PANTALLA (fuera de
///    pantalla, MonitorFromWindow TONEAREST es arbitrario y el DPI del
///    gadget puede estar stale), y la rama rect-only valida la escala
///    contra DWM del primario real al cumplirse la grace de 8 s (la
///    escala GDI de esa rama puede estar stale: log 2026-10-27, 4K
///    reconectado con escala del mundo 2K-solo → gadget a mitad de
///    tamaño);
/// 2) lee el rect actual del gadget y en qué monitor vive;
/// 3) decide:
///    - gadget off-screen, en OTRO monitor o que vivía en uno distinto →
///      lo mueve al primario en la posición recordada para ese monitor
///      (`cfg.monitor_mem`), con el tamaño de la escala nueva (canal
///      GADGET_MOVE_TO; el SetWindowPos lo ejecuta el worker de paint);
///      v7.1: si la posición absoluta de la memoria no cabe en el rango
///      legítimo de la muestra (work area + margen del octágono), la
///      muestra es mixta/stale y se degrada a esquina inf-derecha
///      (`skip_mem_stale_rect`);
///    - mismo monitor y en pantalla → solo chequeo de geometría con la
///      escala nueva como hint (resize canónico inmediato, sin mover).
///
/// Además, cuando llega WM_DPICHANGED al gadget (señal tardía de que la
/// nueva escala ya está aplicada), se re-dispara este mismo proceso: si
/// GDI ya es fresco, corrige posición/tamaño al estado real.
pub fn on_display_change() {
    slog(46, "display_change_apply");
    // Serializa los ajustes: si llega otro cambio mientras uno corre, se
    // re-arma el timer y se repite tras él (el estado intermedio no
    // importa; el final sí).
    if DISP_BUSY.swap(true, Ordering::Relaxed) {
        // Ya hay un ajuste en curso: se repite al terminar (el estado
        // intermedio no importa; el final sí).
        if let Some(a) = APP.lock().unwrap().as_ref() {
            unsafe {
                win::SetTimer(a.hwnd_ctrl, T_DISP, 500, std::ptr::null());
            }
        }
        return;
    }
    let _ = std::thread::Builder::new()
        .name("disp".into())
        .spawn(|| {
            // catch_unwind: si algo paniquea, la bandera se limpia igual
            // (si no, ningún cambio de display futuro se procesaría).
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(disp_apply));
            DISP_BUSY.store(false, Ordering::Relaxed);
        });
}

/// Trabajo de `on_display_change` en el hilo efímero (GDI con retries,
/// fuera del hilo de mensajes).
fn disp_apply() {
    // 1) Muestra inicial con retries: a los segundos del hotplug GDI está
    //    saturado y primary_monitor() puede fallar.
    let mut s0: Option<MonInfo> = None;
    for _ in 0..8 {
        if let Some(p) = primary_monitor() {
            s0 = Some(p);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    // 1.5) Detección de GDI STALE + resolución orientada a WM_DPICHANGED
    //      (ver resolve_display_state): nunca actuar con una escala que un
    //      WM_DPICHANGED fresco desmiente. Devuelve el MonInfo y la escala
    //      FINAL a usar (el hint WM puede ser más fresco que el
    //      GetDpiForMonitor de la muestra). `last` se conserva para el
    //      paso 4 (estado consumido + detección de cambio de escala).
    let last = LAST_DISP.lock().unwrap().clone();
    // 1.6) Estado del gadget (handles + último monitor; APP breve, sin
    //      GDI). Se lee ANTES de resolver (v7): la verificación DWM de
    //      los flips sin WM necesita los handles para sondear
    //      GetDpiForWindow.
    let (hwnd_g, ctrl_hwnd, size_name, last_mon_id) = {
        let app = APP.lock().unwrap();
        match app.as_ref() {
            Some(a) if !a.gadget.hwnd.is_null() => (
                a.gadget.hwnd,
                a.hwnd_ctrl,
                a.gadget.size_name.clone(),
                a.monitor_id.clone(),
            ),
            _ => {
                slog(47, "display_change_apply_skip_no_gadget");
                return;
            }
        }
    };
    // 1.7) Resolución de estado con verificación DWM (v7).
    let (prim, scale_final) = match resolve_display_state(s0, &last, hwnd_g, ctrl_hwnd) {
        Some((p, s)) => (Some(p), s),
        // Sin estado fresco resoluble: ensure_gadget_ok (dentro de
        // resolve_display_state) ya dejó el gadget visible y con
        // tamaño; salir sin actuar sobre datos stale.
        None => return,
    };
    // 3) Rect de la ventana + monitor donde vive (GDI, sin locks).
    let win_rect: Option<win::RECT> = unsafe {
        let mut r: win::RECT = std::mem::zeroed();
        if win::GetWindowRect(hwnd_g, &mut r) != 0 {
            Some(r)
        } else {
            None
        }
    };
    let cur_id: Option<String> = unsafe {
        let hm = win::MonitorFromWindow(hwnd_g, win::MONITOR_DEFAULTTONEAREST);
        if hm.is_null() {
            None
        } else {
            let mut mi: win::MONITORINFO = std::mem::zeroed();
            mi.cbSize = core::mem::size_of::<win::MONITORINFO>() as u32;
            if win::GetMonitorInfoW(hm, &mut mi) == 0 {
                None
            } else {
                Some(hdr::monitor_id_for_device(&win::from_wchar(&mi.szDevice)))
            }
        }
    };

    // 4) Decisión.
    match prim {
        None => {
            // Sin primario resoluble tras ~3 s: fallback conservador. La
            // escala más fiable disponible primero (WM_DPICHANGED fresco),
            // después la de la ventana de control; solo None si no hay nada.
            let hint = gadget::wm_dpi_fresh(20_000).or_else(|| {
                let dpi = unsafe { win::GetDpiForWindow(ctrl_hwnd) };
                (dpi > 0).then_some(dpi as f32 / 96.0)
            });
            gadget::request_geometry_check(hint);
            slog(47, &format!(
                "display_change_apply_done prim=none cur={:?} last={} win=? -> geo",
                cur_id.as_deref().unwrap_or("?"),
                if last_mon_id.is_empty() { "?" } else { &last_mon_id }
            ));
        }
        Some(p) => {
            // Registrar el estado que consumió este evento (referencia
            // para la detección de GDI stale del próximo cambio). Si el WM
            // sobreescribió la escala de la muestra (GDI stale), la firma
            // conserva la escala REAL: es lo que más discrimina en la
            // detección del swap siguiente.
            *LAST_DISP.lock().unwrap() = Some(if (scale_final - p.scale).abs() > 0.01 {
                (p.id.clone(), scale_final, p.mon.left, p.mon.top, p.mon.right, p.mon.bottom)
            } else {
                disp_sig(&p)
            });
            // Escala FINAL de este evento: la resuelta en 1.5 (muestra +
            // puerta WM_DPICHANGED); puede ser más fresca que p.scale.
            let s_final = scale_final.max(0.1);
            match win_rect {
            None => {
                gadget::request_geometry_check(Some(s_final));
                slog(47, &format!(
                    "display_change_apply_done prim={} cur={:?} win=? -> geo esc={}",
                    p.id,
                    cur_id.as_deref().unwrap_or("?"),
                    s_final
                ));
            }
            Some(wr) => {
                let (vx0, vy0) = unsafe {
                    (
                        win::GetSystemMetrics(win::SM_XVIRTUALSCREEN),
                        win::GetSystemMetrics(win::SM_YVIRTUALSCREEN),
                    )
                };
                let (vw, vh) = unsafe {
                    (
                        win::GetSystemMetrics(win::SM_CXVIRTUALSCREEN),
                        win::GetSystemMetrics(win::SM_CYVIRTUALSCREEN),
                    )
                };
                let off = wr.right <= vx0
                    || wr.left >= vx0 + vw
                    || wr.bottom <= vy0
                    || wr.top >= vy0 + vh;
                // ¿Está en un monitor DISTINTO al primario? (Solo si se pudo
                // identificar; si no se pudo, no mover por eso solo.)
                let known_on_other = matches!(cur_id.as_deref(), Some(c) if c != p.id);
                // ¿Vivía en otro monitor antes del cambio?
                let was_other = !last_mon_id.is_empty() && last_mon_id != p.id;
                // v6: ¿cambió la ESCALA respecto del último estado
                // consumido y hay memoria para este (monitor|escala)?
                // Entonces "snapear" a la posición recordada aunque el
                // gadget esté en pantalla: tras un hot-swap 2K↔4K GDI
                // puede seguir reportando el mundo viejo, donde la posición
                // actual se lee "en pantalla" pero con escala/tamaño
                // equivocados.
                let scale_changed =
                    last.as_ref().map_or(false, |b| (b.1 - s_final).abs() > 0.01);
                let mem_hit = {
                    let app = APP.lock().unwrap();
                    app.as_ref()
                        .and_then(|a| a.cfg.monitor_mem.get(&mem_key(&p.id, s_final)).copied())
                };
                if !(off || known_on_other || was_other || (mem_hit.is_some() && scale_changed)) {
                    // Mismo monitor y en pantalla (p. ej. cambio de
                    // resolución): no mover; solo resize canónico con la
                    // escala nueva como hint.
                    gadget::request_geometry_check(Some(s_final));
                    slog(47, &format!(
                        "display_change_apply_done prim={} cur={:?} win=({},{}) off={} last={} -> geo esc={}",
                        p.id,
                        cur_id.as_deref().unwrap_or("?"),
                        wr.left,
                        wr.top,
                        off as u8,
                        if last_mon_id.is_empty() { "?" } else { &last_mon_id },
                        s_final
                    ));
                    return;
                }
                // Mover al primario: memoria de ese (monitor|escala) >
                // posición actual relativa (si está en pantalla) > esquina
                // inf-derecha.
                let s = s_final;
                let (lw, lh) = draw::size_of(&size_name);
                let gw = (lw as f32 * s).round() as i32;
                let gh = (lh as f32 * s).round() as i32;
                let (rel, src) = match mem_hit {
                    Some(r) => (r, "mem"),
                    None if !off => (
                        [
                            (((wr.left - p.mon.left) as f32) / s).round() as i64,
                            (((wr.top - p.mon.top) as f32) / s).round() as i64,
                        ],
                        "cur",
                    ),
                    None => {
                        let corner_x = (p.work.right - gw - 24).max(p.work.left);
                        let corner_y = (p.work.bottom - gh - 24).max(p.work.top);
                        (
                            [
                                (((corner_x - p.mon.left) as f32) / s).round() as i64,
                                (((corner_y - p.mon.top) as f32) / s).round() as i64,
                            ],
                            "corner",
                        )
                    }
                };
                // rel → absoluto con la escala FINAL (p.scale puede estar
                // stale si el WM lo sobreescribió).
                // v7.1: si la posición ABSOLUTA de la memoria cae FUERA
                // del rango de colocación legítimo de place_rel (work
                // area + margen superior del octágono, que permite
                // y < work.top), la muestra es mixta/stale: la memoria
                // es de otro mundo que no cabe en el monitor que la
                // muestra declara (log 2026-10-27: memoria del 4K
                // (1298,-21) × 2.0 = (2596,-42) no cabe en la muestra
                // con rect 2K 2560×1440 → clamp silencioso a (2400,-42)
                // + resize a mitad). Se chequea el valor raw PRE-clamp:
                // el clamp de place_rel metería la posición al rect y
                // ocultaría la inconsistencia. No usar la memoria:
                // degradar a esquina inf-derecha del work.
                let raw_abs = (
                    p.mon.left + (rel[0] as f32 * s).round() as i32,
                    p.mon.top + (rel[1] as f32 * s).round() as i32,
                );
                let (oct_cy, oct_r) = {
                    let (_, cyg, r, _) = draw::octagon_geom(gw, gh);
                    (cyg, r)
                };
                let top_floor = p.work.top - (oct_cy.round() as i32 - oct_r.round() as i32);
                let (rel, src) = if src == "mem"
                    && (raw_abs.0 < p.work.left
                        || raw_abs.0 + gw > p.work.right
                        || raw_abs.1 < top_floor
                        || raw_abs.1 + gh > p.work.bottom)
                {
                    slog(47, &format!(
                        "skip_mem_stale_rect mem_abs=({},{}) work=({},{},{},{}) top_floor={} gw={} gh={}",
                        raw_abs.0,
                        raw_abs.1,
                        p.work.left,
                        p.work.top,
                        p.work.right,
                        p.work.bottom,
                        top_floor,
                        gw,
                        gh
                    ));
                    let corner_x = (p.work.right - gw - 24).max(p.work.left);
                    let corner_y = (p.work.bottom - gh - 24).max(p.work.top);
                    (
                        [
                            (((corner_x - p.mon.left) as f32) / s).round() as i64,
                            (((corner_y - p.mon.top) as f32) / s).round() as i64,
                        ],
                        "corner_stale_rect",
                    )
                } else {
                    (rel, src)
                };
                let (nx, ny) = place_rel(&p.mon, &p.work, rel, s, gw, gh);
                {
                    let mut app = APP.lock().unwrap();
                    if let Some(a) = app.as_mut() {
                        a.primary_scale = s;
                        a.monitor_id = p.id.clone();
                        a.cfg.monitor_mem.insert(mem_key(&p.id, s), rel);
                        config::save(&a.cfg);
                    }
                }
                // v7: la escala que pasa al worker es la FINAL resuelta
                // (s); p.scale puede estar stale si el WM lo sobreescribió
                // — con p.scale el geo-check del worker achicaba el gadget
                // (log 2026-10-27: 160x200 -> 80x100).
                gadget::request_reposition(nx, ny, gw, gh, s);
                slog(47, &format!(
                    "display_change_apply_done prim={} cur={:?} win=({},{}) off={} last={} mem={} -> move ({},{}) {}x{} esc={} rel=({},{}) src={}",
                    p.id,
                    cur_id.as_deref().unwrap_or("?"),
                    wr.left,
                    wr.top,
                    off as u8,
                    if last_mon_id.is_empty() { "?" } else { &last_mon_id },
                    mem_hit.is_some() as u8,
                    nx,
                    ny,
                    gw,
                    gh,
                    s,
                    rel[0],
                    rel[1],
                    src
                ));
            }
            }
        },
    }
}

/// Resuelve el estado de displays post-hotplug con PUERTA DE ESCALA:
/// nunca actuar con una muestra de GDI cuya escala contradice un
/// WM_DPICHANGED fresco (≤20 s) — GDI puede tardar ~15–20 s en reflejar
/// el monitor nuevo, y el rect/id oscilan entre valores viejos/nuevos
/// (muestras "chiméricas"). v7: un flip de escala/id SIN WM fresco solo
/// se actúa tras VERIFICACIÓN DWM (`verify_flip_scale`): una muestra
/// chimérica "estable" puede durar HORAS (log 2026-10-27), así que la
/// estabilidad ya no basta. Devuelve `(MonInfo, escala final)`; la escala
/// final es la del hint WM o de la verificación DWM cuando aplica. Este
/// hilo es efímero: dormir/pollearear aquí no congela la UI.
/// v7.1: la rama rect-only (misma id+escala, rect nuevo) valida la
/// escala contra DWM de la ventana de control al cumplirse la grace de
/// 8 s: la escala GDI de esa rama puede estar stale (mundo de monitor
/// único → id+escala viejos + rect nuevo) y actuar con ella dejaba el
/// gadget a medio tamaño en el monitor reconectado (log 2026-10-27).
fn resolve_display_state(
    s0: Option<MonInfo>,
    baseline: &Option<(String, f32, i32, i32, i32, i32)>,
    hwnd_gadget: *mut c_void,
    hwnd_ctrl: *mut c_void,
) -> Option<(MonInfo, f32)> {
    let t0 = std::time::Instant::now();
    // Fast path v6: actuar YA solo si la muestra inicial es consistente
    // con un WM_DPICHANGED fresco (GDI despierto) o si nunca se procesó un
    // estado (baseline None). El v5 devolvía la muestra cuando "difiera de
    // la firma base y no hubiera WM fresco" — justo el camino peligroso:
    // tras el swap, una muestra stale/chimérica actuaba a los 8 s con
    // datos del mundo viejo (log S4: move con escala 1.0 sobre el 4K a
    // los +31 s).
    if let Some(s) = &s0 {
        if baseline.is_none() {
            let g = gadget::wm_dpi_fresh(20_000);
            return Some((s.clone(), g.unwrap_or(s.scale)));
        }
        if let Some(g) = gadget::wm_dpi_fresh(20_000) {
            if (s.scale - g).abs() <= 0.01 {
                slog(47, "act_start_gdi_consistent_with_wm");
                return Some((s.clone(), g));
            }
        }
    } else {
        slog(47, "wait_start_no_sample");
    }
    // Espera orientada a meta (tope 60 s = 120 × 500 ms). v6: el
    // WM_DPICHANGED posterior al último DISPLAYCHANGE es la AUTORIDAD de
    // escala (ver wm_hint_invalidate): una muestra de GDI que lo contradice
    // es stale/chimérica y se actúa con la ESCALA DEL WM, no con la de la
    // muestra. El tamaño se auto-cura solo vía request_geometry_check (lo
    // dispara on_dpi_changed); la posición la da la memoria por
    // (monitor|escala).
    let mut last: Option<MonInfo> = s0;
    // Estabilidad para "flip de escala sin WM": si una muestra distinta de
    // la firma base persiste 12 s, se asume GDI despierto (el DPICHANGED
    // llegó tarde o no llega) y se actúa con su escala.
    let mut stable_at: Option<(MonInfo, std::time::Instant)> = None;
    for _ in 0..120 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let Some(sample) = primary_monitor() else {
            continue; // GDI aún saturado
        };
        last = Some(sample.clone());
        if let Some(g) = gadget::wm_dpi_fresh(20_000) {
            if (sample.scale - g).abs() <= 0.01 {
                // GDI despierto y consistente con el WM: actuar ya.
                slog(47, &format!(
                    "wait_gdi_caught_up_after_ms={}",
                    t0.elapsed().as_millis()
                ));
                return Some((sample, g));
            }
            // WM fresco que DESMIENTE la escala de la muestra: GDI
            // stale/chimérico. El cambio es REAL (el WM lo confirma);
            // actuar YA con la escala del WM y el rect de la muestra: en un
            // hot-swap de monitor único el origen del primario es (0,0) y
            // la memoria por (monitor|escala) da la posición real. Esperar
            // a que GDI despierte aquí es lo que dejaba el gadget mal
            // ubicado (GDI stale hasta 31 s observado en el log).
            slog(47, &format!(
                "act_wm_overrides_stale_gdi after_ms={} gdi_esc={} wm={}",
                t0.elapsed().as_millis(),
                sample.scale,
                g
            ));
            return Some((sample, g));
        }
        let b = match baseline {
            Some(b) => b,
            // Sin baseline (nunca se procesó un estado): primera muestra.
            None => {
                slog(47, &format!(
                    "wait_first_sample_after_ms={}",
                    t0.elapsed().as_millis()
                ));
                return Some((sample.clone(), sample.scale));
            }
        };
        let sig = disp_sig(&sample);
        if !sig_differs(&sig, b) {
            // GDI aún reporta el mundo viejo: nada fresco con qué actuar.
            stable_at = None;
            continue;
        }
        let only_rect = sig.0 == b.0 && (sig.1 - b.1).abs() <= 0.01;
        if only_rect {
            // Solo cambió el rect (misma escala): cambio de resolución a la
            // misma escala (legítimo, no trae DPICHANGED) o muestra
            // chimérica temprana (el rect oscila al inicio del swap).
            // Grace 8 s y luego actuar: un swap real ya habrá traído su
            // DPICHANGED para entonces (tomó la rama WM arriba).
            // v7.1: la ESCALA de la muestra en esta rama puede estar
            // STALE aunque el rect sea fresco — tras un mundo de monitor
            // único, GDI puede reportar el id+escala del mundo viejo con
            // el rect del nuevo (log 2026-10-27: 4K reconectado con
            // escala 1.0 del mundo 2K-solo → resize 160x200 → 80x100 en
            // el 4K). Al cumplirse la grace, validar contra DWM de la
            // ventana de control (primario real): si difiere, actuar con
            // la de DWM (mismo patrón que act_wm_overrides_stale_gdi);
            // si coincide (o no se puede leer), con la de la muestra
            // (cambio de resolución a misma escala, legítimo).
            stable_at = None;
            if t0.elapsed().as_millis() >= 8_000 {
                let s = match dwm_probe(hwnd_ctrl) {
                    Some(c) if (c - sample.scale).abs() > 0.01 => c,
                    _ => sample.scale,
                };
                if (s - sample.scale).abs() > 0.01 {
                    slog(47, &format!(
                        "act_rect_only_dw_fixed after_ms={} gdi_esc={} dw_c={}",
                        t0.elapsed().as_millis(),
                        sample.scale,
                        s
                    ));
                } else {
                    slog(47, &format!(
                        "wait_no_wm_rect_only_after_ms={}",
                        t0.elapsed().as_millis()
                    ));
                }
                return Some((sample.clone(), s));
            }
        } else {
            // Flip de ESCALA/ID sin WM confirmando: puede ser una muestra
            // chimérica temprana (cualquier mezcla id/escala/rect de dos
            // mundos). v7: 12 s de ESTABILIDAD ya no autoriza actuar —
            // una muestra chimérica puede mantenerse "estable" HORAS
            // (log 2026-10-27: id-viejo + escala-nueva duró ~1.8 h y la
            // regla v6 actuó con ella). Al cumplirse la estabilidad se
            // VERIFICA contra DWM (GetDpiForWindow: ventana de control =
            // primario real; ventana del gadget = su monitor): si la
            // verificación da una escala, se actúa con ELLA (puede ser
            // distinta de la de la muestra); si no, se sigue esperando
            // (se re-verifica cada 12 s; el timeout de 60 s resuelve de
            // forma conservadora, sin mover).
            match &stable_at {
                Some((sp, since)) if !sig_differs(&disp_sig(sp), &sig) => {
                    if since.elapsed().as_millis() >= 12_000 {
                        match verify_flip_scale(&sample, hwnd_gadget, hwnd_ctrl) {
                            Some(s) => {
                                slog(47, &format!(
                                    "act_stable_flip_verified after_ms={} gdi_esc={} ver_esc={} dw_g={:?} dw_c={:?}",
                                    t0.elapsed().as_millis(),
                                    sample.scale,
                                    s,
                                    dwm_probe(hwnd_gadget),
                                    dwm_probe(hwnd_ctrl)
                                ));
                                return Some((sample.clone(), s));
                            }
                            None => {
                                // Chimérica no verificable: no actuar con
                                // esta muestra; reiniciar el cronómetro de
                                // estabilidad para re-verificar en 12 s
                                // (DWM/GDI pueden despertar en el medio).
                                stable_at = Some((sample.clone(), std::time::Instant::now()));
                                slog(47, &format!(
                                    "skip_flip_unverified after_ms={} gdi_esc={} dw_g={:?} dw_c={:?}",
                                    t0.elapsed().as_millis(),
                                    sample.scale,
                                    dwm_probe(hwnd_gadget),
                                    dwm_probe(hwnd_ctrl)
                                ));
                            }
                        }
                    }
                }
                _ => stable_at = Some((sample.clone(), std::time::Instant::now())),
            }
        }
    }
    // Timeout: actuar conservador con lo último leído (NUNCA mover por
    // memoria de un id stale; el tamaño sí desde el WM fresco si hay).
    slog(47, "wait_timeout_60s_conservative");
    match (&last, baseline) {
        (Some(s), Some(b)) if !sig_differs(&disp_sig(s), b) => {
            // GDI nunca se actualizó en 60 s: no hay nada fresco con qué
            // actuar → None (geo-check sin mover; LAST_DISP intacto).
            let g = gadget::wm_dpi_fresh(20_000);
            ensure_gadget_ok(s, g);
            None
        }
        (Some(s), _) => {
            let g = gadget::wm_dpi_fresh(20_000);
            ensure_gadget_ok(s, g);
            Some((s.clone(), g.unwrap_or(s.scale)))
        }
        (None, _) => None,
    }
}

/// Mantener el gadget VISIBLE (dentro de la pantalla virtual) y con el
/// tamaño canónico de `scale_hint` mientras se espera a GDI fresco. Solo
/// GetWindowRect/GetSystemMetrics + canales del worker de paint (el
/// SetWindowPos SIEMPRE en ese hilo — AppHangB1).
fn ensure_gadget_ok(sample: &MonInfo, scale_hint: Option<f32>) {
    let app = APP.lock().unwrap();
    let Some(a) = app.as_ref() else {
        return;
    };
    if a.gadget.hwnd.is_null() {
        return;
    }
    let hwnd = a.gadget.hwnd;
    let size_name = a.gadget.size_name.clone();
    drop(app);
    let (vx0, vy0) = unsafe {
        (
            win::GetSystemMetrics(win::SM_XVIRTUALSCREEN),
            win::GetSystemMetrics(win::SM_YVIRTUALSCREEN),
        )
    };
    let (vw, vh) = unsafe {
        (
            win::GetSystemMetrics(win::SM_CXVIRTUALSCREEN),
            win::GetSystemMetrics(win::SM_CYVIRTUALSCREEN),
        )
    };
    let mut wr = unsafe { std::mem::zeroed::<win::RECT>() };
    if unsafe { win::GetWindowRect(hwnd, &mut wr) } == 0 {
        return;
    }
    let (gw, gh) = (wr.right - wr.left, wr.bottom - wr.top);
    let off = gw > 0
        && gh > 0
        && (wr.right <= vx0 || wr.left >= vx0 + vw || wr.bottom <= vy0 || wr.top >= vy0 + vh);
    let s = scale_hint.unwrap_or(sample.scale).max(0.1);
    let (lw, lh) = draw::size_of(&size_name);
    let (ew, eh) = ((lw as f32 * s).round() as i32, (lh as f32 * s).round() as i32);
    if off {
        // Esquina inf-derecha de la pantalla VIRTUAL (el work area
        // primario puede estar stale en plena reconfiguración).
        let nx = (vx0 + vw - gw - 24).max(vx0);
        let ny = (vy0 + vh - gh - 24).max(vy0);
        gadget::request_reposition(nx, ny, ew, eh, s);
        slog(47, &format!(
            "wait_ensure_visible ({},{}) -> ({},{}) {}x{} esc={}",
            wr.left,
            wr.top,
            nx,
            ny,
            ew,
            eh,
            s
        ));
    } else if (gw as i64 - ew as i64).abs() > 2 || (gh as i64 - eh as i64).abs() > 2 {
        gadget::request_geometry_check(Some(s));
        slog(47, &format!("wait_ensure_resized {}x{} -> {}x{} esc={}", gw, gh, ew, eh, s));
    }
}

/// Cada 2 s: si el estado real cambió (cable, otro app), se refleja.
///
/// La consulta corre en un worker (mismo patrón que el toggle): la API de
/// DisplayConfig/DWM puede bloquearse en el stack de video (reconfiguración
/// de monitor, interacción con PrintWindow externo, etc.); si se hiciera en
/// el hilo de mensajes, se congelaría toda la app (timers, arrastre, bandeja).
fn refresh_state() {
    let (hwnd, skip) = {
        let mut app = APP.lock().unwrap();
        let Some(a) = app.as_mut() else {
            return;
        };
        // Watchdog: un toggle legítimo dura a lo sumo ~8.5 s (poll 7 s +
        // re-consulta DC). Si lleva >20 s "en curso", su worker está
        // colgado (stack de video): se suelta la traba para que la app
        // no ignore los clicks para siempre.
        if a.busy
            && a
                .busy_since
                .map(|t| t.elapsed() > std::time::Duration::from_secs(20))
                .unwrap_or(false)
        {
            a.busy = false;
            a.busy_since = None;
            slog(59, "busy_watchdog_reset");
        }
        (a.hwnd_ctrl, a.busy)
    };
    if skip {
        return;
    }
    // Anti-sobreposición con timeout: si una consulta anterior sigue "en
    // curso" hace >10 s se da por muerta (get_state() puede bloquearse
    // en el stack de video) y se reintenta. El worker colgado se fuga,
    // pero la app sigue respondiendo.
    if STATE_CHECKING.swap(true, Ordering::Relaxed)
        && now_ms().saturating_sub(STATE_CHECK_SINCE.load(Ordering::Relaxed)) < 10_000
    {
        return;
    }
    STATE_CHECK_SINCE.store(now_ms(), Ordering::Relaxed);
    let hwnd_s = win::Shared::new(hwnd);
    std::thread::spawn(move || {
        let (st, _) = hdr::get_state();
        STATE_CHECKING.store(false, Ordering::Relaxed);
        let code = usize::from(st == Some(hdr::HDR)); // desconocido → SDR (como antes)
        unsafe {
            win::PostMessageW(hwnd_s.ptr(), WM_STATE_DONE, code, 0);
        }
    });
}

/// La 2ª instancia (eventos nombrados) pidió mostrar el gadget.
fn handle_show_signal() {
    let sig = {
        let app = APP.lock().unwrap();
        app.as_ref()
            .map(|a| a.single.take_show_signal())
            .unwrap_or(false)
    };
    if sig {
        let mut app = APP.lock().unwrap();
        if let Some(a) = app.as_mut() {
            a.gadget.show();
            a.cfg.gadget_visible = true;
            config::save(&a.cfg);
        }
    }
}

/// Cada 120 ms: ¿el cursor está sobre el icono?
fn tip_tick(hwnd: *mut c_void) {
    let mut app = APP.lock().unwrap();
    let Some(a) = app.as_mut() else {
        return;
    };
    if a.tip_suppressed {
        a.tip_armed = false;
        unsafe {
            win::KillTimer(hwnd, T_TIP_SHOW);
        }
        if a.tip.is_visible() {
            a.tip.hide();
        }
        return;
    }
    // Shell_NotifyIconGetRect es inter-proceso (explorer): durante la
    // reconfiguración de la bandeja (cambio de monitor) puede tardar;
    // se mide para identificar cuelgues (90 = lento).
    let t0 = Instant::now();
    let rect = a.tray.get_position();
    let ms = t0.elapsed().as_millis();
    if ms > 250 {
        slog(90, &format!("getpos_slow_ms={}", ms));
    }
    let width = rect.map(|r| r.right - r.left).unwrap_or(0);
    if rect.is_none() || width < 4 {
        // Icono en el área de desborde: tip nativo de una línea.
        if !a.tip_overflow {
            slog(41, "tip_OVERFLOW_on");
        }
        a.tip_overflow = true;
        a.tray.set_native_tip(Some(tray::native_tip_text(a.state)));
        a.tip_armed = false;
        unsafe {
            win::KillTimer(hwnd, T_TIP_SHOW);
        }
        if a.tip.is_visible() {
            a.tip.hide();
        }
        return;
    }
    let r = rect.unwrap();
    if a.tip_overflow {
        slog(42, &format!(
            "tip_OVERFLOW_off rect=({},{},{},{})",
            r.left, r.top, r.right, r.bottom
        ));
        a.tip_overflow = false;
        a.tray.set_native_tip(None);
    }
    let mut pt: win::POINT = unsafe { std::mem::zeroed() };
    let inside = unsafe { win::GetCursorPos(&mut pt) } != 0
        && pt.x >= r.left
        && pt.x < r.right
        && pt.y >= r.top
        && pt.y < r.bottom;
    if inside {
        if !a.tip_armed {
            a.tip_armed = true;
            unsafe {
                win::SetTimer(hwnd, T_TIP_SHOW, 350, std::ptr::null());
            }
        }
        if a.tip.is_visible() {
            a.tip.place_and_show(&r); // re-posicionar si se movió la bandeja
        }
    } else {
        a.tip_armed = false;
        unsafe {
            win::KillTimer(hwnd, T_TIP_SHOW);
        }
        if a.tip.is_visible() {
            a.tip.hide();
        }
    }
}

/// Tras el delay de 350 ms: mostrar el tip.
fn tip_show() {
    let mut app = APP.lock().unwrap();
    let Some(a) = app.as_mut() else {
        return;
    };
    if a.tip_suppressed {
        return;
    }
    let t0 = Instant::now();
    if let Some(r) = a.tray.get_position() {
        let ms = t0.elapsed().as_millis();
        if ms > 250 {
            slog(90, &format!("getpos_slow_ms={}", ms));
        }
        if r.right - r.left >= 4 {
            a.tip.place_and_show(&r);
        }
    }
}

// ---------------------------------------------------------------------------
// Eventos de la bandeja
// ---------------------------------------------------------------------------

/// Click corto: alterna HDR/SDR (con guarda anti doble-click: el 2º
/// LBUTTONUP dentro de SM_DBLCLKTIME se traga; el doble click completo
/// toca la visibilidad del gadget).
fn tray_lbuttonup() -> bool {
    let go = {
        let mut app = APP.lock().unwrap();
        let Some(a) = app.as_mut() else {
            return false;
        };
        if a.busy {
            return false;
        }
        let dbl = (unsafe { win::GetSystemMetrics(win::SM_DBLCLKTIME) } as u64).max(1);
        let now = Instant::now();
        let ok = a
            .last_tray_toggle
            .map(|t| now.duration_since(t).as_millis() >= dbl as u128)
            .unwrap_or(true);
        if ok {
            a.last_tray_toggle = Some(now);
        }
        ok
    };
    if go {
        request_toggle();
    }
    go
}

/// Doble click: alterna la visibilidad del gadget.
fn tray_lbuttondblclick() {
    let mut app = APP.lock().unwrap();
    let Some(a) = app.as_mut() else {
        return;
    };
    a.cfg.gadget_visible = !a.cfg.gadget_visible;
    config::save(&a.cfg);
    if a.cfg.gadget_visible {
        a.gadget.show();
    } else {
        a.gadget.hide();
    }
}

/// Worker terminó (toggle completado).
///
/// `target_hdr`: el estado que se pidió (lParam del WM_TOGGLE_DONE). No
/// se re-lee con get_state() aquí: es síncrono y en el hilo de mensajes
/// podría bloquearse en el stack de video; T_STATE re-valida cada 2 s.
fn on_toggle_done(ok: bool, target_hdr: bool) {
    let mut app = APP.lock().unwrap();
    let Some(a) = app.as_mut() else {
        return;
    };
    a.busy = false;
    a.busy_since = None;
    if ok {
        slog(53, "toggle_ok");
        a.state = if target_hdr { hdr::HDR } else { hdr::SDR };
        a.gadget.set_state(a.state);
        refresh_icon(a);
    } else {
        slog(54, "toggle_fail_warn");
        // No se pudo (monitor sin HDR u otro fallo).
        a.gadget.set_state(a.state);
        a.gadget.show_warning("HDR No compatible");
        unsafe {
            win::SetTimer(a.hwnd_ctrl, T_WARN, 3500, std::ptr::null());
        }
        if !a.gadget.visible {
            a.tray
                .balloon(lang::t(&a.cfg.language, "Botón HDR", "HDR Button"), "HDR No compatible");
        }
    }
}

// ---------------------------------------------------------------------------
// Arranque / salida
// ---------------------------------------------------------------------------

pub(crate) fn primary_work_area() -> Option<win::RECT> {
    unsafe {
        let mut r: win::RECT = std::mem::zeroed();
        if win::SystemParametersInfoW(
            win::SPI_GETWORKAREA,
            0,
            &mut r as *mut win::RECT as *mut c_void,
            0,
        ) != 0
        {
            Some(r)
        } else {
            None
        }
    }
}

/// Monitor primario: ID estable + rect completo + work area + escala.
#[derive(Clone)]
pub(crate) struct MonInfo {
    pub id: String,
    pub mon: win::RECT,
    pub work: win::RECT,
    pub scale: f32,
}

/// El monitor primario contiene el origen (0,0) de la pantalla virtual.
pub(crate) fn primary_monitor() -> Option<MonInfo> {
    unsafe {
        let hmon = win::MonitorFromPoint(win::POINT { x: 0, y: 0 }, win::MONITOR_DEFAULTTOPRIMARY);
        if hmon.is_null() {
            return None;
        }
        let mut mi: win::MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<win::MONITORINFO>() as u32;
        if win::GetMonitorInfoW(hmon, &mut mi) == 0 {
            return None;
        }
        let device = win::from_wchar(&mi.szDevice);
        let mut dpi: u32 = 0;
        let _ =
            win::GetDpiForMonitor(hmon, win::MDT_EFFECTIVE_DPI, &mut dpi, std::ptr::null_mut());
        let scale = if dpi > 0 {
            dpi as f32 / 96.0
        } else {
            1.0
        };
        Some(MonInfo {
            id: hdr::monitor_id_for_device(&device),
            mon: mi.rcMonitor,
            work: mi.rcWork,
            scale,
        })
    }
}

/// Esquina sup-izq absoluta (px físicos) de una posición relativa al
/// monitor (px lógicos = físicos ÷ escala del monitor), con tope en el
/// work area.
fn place_rel(
    mon: &win::RECT,
    work: &win::RECT,
    rel: [i64; 2],
    scale: f32,
    gw: i32,
    gh: i32,
) -> (i32, i32) {
    let s = scale.max(0.1);
    let x = mon.left + (rel[0] as f32 * s).round() as i32;
    let y = mon.top + (rel[1] as f32 * s).round() as i32;
    // Tope superior: igual que el clamp del drag, el octágono puede llegar
    // al tope de la pantalla (la ventana puede quedar hasta el margen
    // superior del octágono por encima del work area).
    let (_, cyg, r, _) = draw::octagon_geom(gw, gh);
    let top_floor = work.top - (cyg.round() as i32 - r.round() as i32);
    (
        x.clamp(work.left, (work.right - gw).max(work.left)),
        y.clamp(top_floor, (work.bottom - gh).max(top_floor)),
    )
}

/// Posición inicial: memoria de ESTE monitor; si no, legacy global
/// (el origen virtual = sup-izq del primario); si no, esquina
/// inf-derecha del work area primario.
fn place_gadget(cfg: &config::Cfg, gw: i32, gh: i32, prim: &MonInfo) -> (i32, i32) {
    let corner = (
        (prim.work.right - gw - 24).max(prim.work.left),
        (prim.work.bottom - gh - 24).max(prim.work.top),
    );
    cfg.monitor_mem
        .get(&mem_key(&prim.id, prim.scale))
        .map(|&r| place_rel(&prim.mon, &prim.work, r, prim.scale, gw, gh))
        .or_else(|| {
            cfg.position
                .map(|p| place_rel(&prim.mon, &prim.work, p, prim.scale, gw, gh))
        })
        .unwrap_or(corner)
}

fn cleanup() {
    let mut app = APP.lock().unwrap();
    if let Some(a) = app.as_mut() {
        unsafe {
            win::KillTimer(a.hwnd_ctrl, T_STATE);
            win::KillTimer(a.hwnd_ctrl, T_SHOW);
            win::KillTimer(a.hwnd_ctrl, T_TIP);
            win::KillTimer(a.hwnd_ctrl, T_WARN);
            win::KillTimer(a.hwnd_ctrl, T_TIP_SHOW);
        }
        unsafe {
            win::UnregisterHotKey(a.hwnd_ctrl, HOTKEY_ID);
        }
        a.tip.destroy();
        a.gadget.destroy();
        a.tray.delete();
        unsafe {
            win::KillTimer(a.hwnd_ctrl, T_DISP);
            if !a.hwnd_ctrl.is_null() {
                win::DestroyWindow(a.hwnd_ctrl);
            }
            a.hwnd_ctrl = std::ptr::null_mut();
        }
    }
    drop(app.take()); // cae el SingleInstance: cierra mutex y evento
}

fn main() {
    install_panic_hook();
    let _ = std::fs::write(config::log_path(), "");
    slog(0, "start");
    // 1) Toasts de Game Bar al cambiar HDR (best effort).
    gamebar::suppress_hdr_toasts();
    slog(1, "gamebar");
    // 2) Config (corre las migraciones one-shot). `mut`: la migración
    //    v7 (paso 6.6) y la siembra de memoria (paso 7.5) lo editan.
    let mut cfg = config::load();
    slog(2, "config");
    // 2.5) Lock persistido ("Bloquear SDR/HDR"): se re-aplica tras el
    //     estado inicial (paso 11.5) y la sentinela cubre desde entonces.
    let lock_init: Option<&'static str> = cfg
        .hdr_lock
        .as_deref()
        .map(|s| if s == "HDR" { hdr::HDR } else { hdr::SDR });
    // 3) Single instance: la 2ª instancia solo pide mostrar el gadget.
    let single = singleinstance::SingleInstance::new();
    slog(3, "single");
    if !single.is_first {
        return;
    }
    // 4) DPI aware por monitor (v2), fallback system-aware.
    unsafe {
        if win::SetProcessDpiAwarenessContext(win::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0
        {
            let _ = win::SetProcessDpiAwarenessContext(win::DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        }
    }
    slog(4, "dpi");
    // 5) Monitor primario: ID + rectángulos + escala (memoria por
    //    monitor: el gadget recuerda su posición en cada uno). `mut`: el
    //    paso 6.5 puede corregir la escala con GetDpiForWindow (DWM fresco).
    let mut prim = primary_monitor().unwrap_or_else(|| {
        let (vw, vh) = (
            unsafe { win::GetSystemMetrics(win::SM_CXVIRTUALSCREEN) },
            unsafe { win::GetSystemMetrics(win::SM_CYVIRTUALSCREEN) },
        );
        let scale = unsafe {
            let hdc = win::GetWindowDC(std::ptr::null_mut());
            let dpi = win::GetDeviceCaps(hdc, win::LOGPIXELSX);
            win::ReleaseDC(std::ptr::null_mut(), hdc);
            if dpi > 0 {
                dpi as f32 / 96.0
            } else {
                1.0
            }
        };
        let r = win::RECT { left: 0, top: 0, right: vw.max(1), bottom: vh.max(1) };
        MonInfo {
            id: String::new(),
            mon: r,
            work: primary_work_area().unwrap_or(r),
            scale,
        }
    });
    // 6) Ventana de control oculta (bombea timers/mensajes).
    let hwnd_ctrl = create_ctrl_window();
    slog(6, "ctrl_window");
    if hwnd_ctrl.is_null() {
        eprintln!("BotonHDR: no se pudo crear la ventana de control");
        return;
    }
    // 6.5) Verificación DWM (v6): al arrancar en un monitor recién
    //      conectado, GetDpiForMonitor puede devolver todavía la escala
    //      VIEJA (observado: arranque en 4K@200% reportando 100% → gadget
    //      nacido a medio tamaño y memoria sembrada con escala errónea).
    //      GetDpiForWindow sobre una ventana recién creada es fresco (el
    //      DWM asigna el DPI en el CreateWindow): si difiere, manda él.
    let mut primary_scale = prim.scale;
    {
        let dpi = unsafe { win::GetDpiForWindow(hwnd_ctrl) };
        if dpi > 0 {
            let ds = dpi as f32 / 96.0;
            if (ds - primary_scale).abs() > 0.01 {
                slog(5, &format!("scale_fixed_gdi={} dwm={}", prim.scale, ds));
                primary_scale = ds;
                prim.scale = ds;
            }
        }
    }
    *LAST_DISP.lock().unwrap() = Some(disp_sig(&prim));
    slog(5, "scale");
    // 6.6) Migración one-shot v7: purga claves de memoria chiméricas
    //      (id|escala imposible) escritas por el bug pre-v7. Corre DESPUÉS
    //      del 6.5 para no purgar con la escala GDI stale del arranque.
    //      (No fija el flag si la enumeración falla: reintenta al
    //      próximo arranque.)
    migrate_monitor_mem_v7(&mut cfg, &prim.id, primary_scale);
    // 7) Gadget (tamaño lógico × escala; px físicos).
    let (lw, lh) = draw::size_of(&cfg.size);
    let gw = (lw as f32 * primary_scale).round() as i32;
    let gh = (lh as f32 * primary_scale).round() as i32;
    let (gx, gy) = place_gadget(&cfg, gw, gh, &prim);
    let mut g = match gadget::create(gw, gh, primary_scale, &cfg.size) {
        Some(g) => g,
        None => {
            eprintln!("BotonHDR: no se pudo crear el gadget");
            return;
        }
    };
    g.set_accent(&cfg.color);
    g.set_opacity_pct(cfg.opacity);
    g.set_position(gx, gy);
    // 7.5) Memoria por monitor (v6): clave compuesta "id|escala%" +
    //      migración one-shot de entradas legacy (solo id). La escala de
    //      atribución de las legacy es la del arranque (corregida por
    //      DWM): el último drag casi siempre fue en este monitor; se cura
    //      con el primer arrastre.
    if !prim.id.is_empty() {
        let boot_key = mem_key(&prim.id, primary_scale);
        let legacy: Vec<(String, [i64; 2])> = cfg
            .monitor_mem
            .iter()
            .filter(|(k, _)| !k.contains('|'))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        for (k, v) in &legacy {
            let nk = mem_key(k, primary_scale);
            if !cfg.monitor_mem.contains_key(&nk) {
                cfg.monitor_mem.insert(nk, *v);
            }
            cfg.monitor_mem.remove(k);
        }
        let seeded = !cfg.monitor_mem.contains_key(&boot_key);
        if seeded {
            // Primera vez en este (monitor|escala): guardar la posición
            // inicial en px lógicos relativos al primario (físicos ÷ escala).
            cfg.monitor_mem.insert(
                boot_key,
                [
                    (((gx - prim.mon.left) as f32) / primary_scale.max(0.1)).round() as i64,
                    (((gy - prim.mon.top) as f32) / primary_scale.max(0.1)).round() as i64,
                ],
            );
        }
        if seeded || !legacy.is_empty() {
            config::save(&cfg);
        }
    }
    slog(7, "gadget");
    // 8) Bandeja.
    let mut tr = tray::Tray::new(hwnd_ctrl);
    tr.add();
    slog(8, "tray");
    // 9) Tip propio de la bandeja.
    let tip = match traytip::TipState::create() {
        Some(t) => t,
        None => {
            eprintln!("BotonHDR: no se pudo crear el tip");
            return;
        }
    };
    slog(9, "tip");
    // 10) Estado global.
    *APP.lock().unwrap() = Some(App {
        single,
        cfg,
        state: hdr::SDR,
        busy: false,
        busy_since: None,
        hwnd_ctrl,
        gadget: g,
        tray: tr,
        tip,
        last_tray_toggle: None,
        primary_scale,
        monitor_id: prim.id.clone(),
        tip_suppressed: false,
        tip_armed: false,
        tip_overflow: false,
        locked: lock_init.is_some(),
        lock_state: lock_init.unwrap_or(hdr::SDR),
    });
    slog(10, "global");
    // 11) Estado inicial. Se pinta el valor por defecto (SDR) y un
    //     worker consulta el real (mismo patrón que T_STATE). NUNCA se
    //     hace get_state() en el hilo de mensajes: la API DisplayConfig/
    //     DWM puede bloquearse y se congelaría toda la app (timers,
    //     bandeja, arrastre del gadget).
    {
        let mut app = APP.lock().unwrap();
        if let Some(a) = app.as_mut() {
            a.gadget.set_state(a.state);
            refresh_icon(a);
        }
    }
    refresh_state();
    slog(11, "state_worker");
    // 11.5) Lock persistido: candado en el gadget + chip en bandeja +
    //     captura del hotkey. (Si el estado real contradice el lock,
    //     WM_STATE_DONE lo ignora y la sentinela intenta revertirlo.)
    if lock_init.is_some() {
        let mut app = APP.lock().unwrap();
        if let Some(a) = app.as_mut() {
            a.gadget.set_locked(true);
            refresh_icon(a);
            set_hotkey(a.hwnd_ctrl, true);
        }
    }
    // 12) Workers de paint (gadget + tip) y visibilidad inicial.
    //     El worker del gadget ejecuta ShowWindow + ancle al fondo + ULW;
    //     el hilo de mensajes jamás espera a DWM (AppHangB1).
    {
        let mut app = APP.lock().unwrap();
        if let Some(a) = app.as_mut() {
            a.gadget.start_worker();
            a.tip.start_worker();
            if a.cfg.gadget_visible {
                a.gadget.show(); // intención: el worker pinta y muestra
            }
        }
    }
    slog(12, "gadget_show_requested");
    // 13) Timers.
    unsafe {
        win::SetTimer(hwnd_ctrl, T_STATE, 2000, std::ptr::null());
        win::SetTimer(hwnd_ctrl, T_SHOW, 200, std::ptr::null());
        win::SetTimer(hwnd_ctrl, T_TIP, 120, std::ptr::null());
    }
    slog(13, "timers");
    // 13.5) Sentinela del lock (hilo dedicado; solo consulta el estado
    //       mientras hay lock activo, si no duerme 400 ms).
    std::thread::Builder::new()
        .name("botonhdr-lock-sentinel".into())
        .spawn(lock_sentinel)
        .ok();
    // 13.6) Watchdog: heartbeat 10 s + chequeo del lock de APP
    //       (diagnóstico de cuelgues: 99 = vivo, 91 = APP retenido).
    std::thread::Builder::new()
        .name("botonhdr-watchdog".into())
        .spawn(watchdog)
        .ok();
    // 14) Bucle de mensajes (termina con WM_QUIT).
    slog(14, "loop");
    unsafe {
        let mut msg: win::MSG = std::mem::zeroed();
        loop {
            let r = win::GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r == 0 || r == -1 {
                break;
            }
            let _ = win::TranslateMessage(&msg);
            let _ = win::DispatchMessageW(&msg);
        }
    }
    cleanup();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(
        id: &str,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        work_right: i32,
        work_bottom: i32,
        scale: f32,
    ) -> MonInfo {
        MonInfo {
            id: id.into(),
            mon: win::RECT { left, top, right, bottom },
            work: win::RECT { left, top, right: work_right, bottom: work_bottom },
            scale,
        }
    }

    #[test]
    fn place_rel_escala_y_clamp() {
        // Primario 2560x1400 @100%: relativo (100, 50) → (100, 50).
        let p = mon("A", 0, 0, 2560, 1440, 2560, 1400, 1.0);
        assert_eq!(
            place_rel(&p.mon, &p.work, [100, 50], p.scale, 120, 150),
            (100, 50)
        );
        // @200%: (100, 50) lógicos → (200, 100) físicos.
        let p2 = mon("B", 0, 0, 3840, 2160, 3840, 2100, 2.0);
        assert_eq!(
            place_rel(&p2.mon, &p2.work, [100, 50], p2.scale, 120, 150),
            (200, 100)
        );
        // Clamp: fuera a la derecha y arriba del work area → borde
        // (arriba: hasta el margen del octágono, no el tope del work area).
        let (_, cyg, r, _) = draw::octagon_geom(120, 150);
        let top_floor = 0 - (cyg.round() as i32 - r.round() as i32);
        assert_eq!(
            place_rel(&p.mon, &p.work, [99999, -500], p.scale, 120, 150),
            (2560 - 120, top_floor)
        );
        // Un y negativo dentro del margen del octágono se conserva
        // (el octágono llega al tope de la pantalla).
        assert_eq!(
            place_rel(&p.mon, &p.work, [1302, -20], p.scale, 120, 150),
            (1302, -20)
        );
    }

    #[test]
    fn place_rel_monitor_desplazado() {
        // Segundo monitor a x=2560 (4K @200%): relativo (0,0) → (2560, 0).
        let p = mon("B", 2560, 0, 6400, 2160, 6400, 2100, 2.0);
        assert_eq!(
            place_rel(&p.mon, &p.work, [0, 0], p.scale, 120, 150),
            (2560, 0)
        );
    }

    #[test]
    fn place_gadget_prioridad_memoria_legacy_esquina() {
        let prim = mon("A", 0, 0, 2560, 1400, 2560, 1400, 1.0);
        let mut cfg = config::Cfg::default();
        // Sin nada guardado: esquina inf-derecha del work area.
        assert_eq!(
            place_gadget(&cfg, 120, 150, &prim),
            (2560 - 120 - 24, 1400 - 150 - 24)
        );
        // Legacy global: se usa si no hay memoria de ESTE monitor.
        cfg.position = Some([100, 50]);
        assert_eq!(place_gadget(&cfg, 120, 150, &prim), (100, 50));
        // Memoria del monitor actual: tiene prioridad sobre el legacy.
        cfg.monitor_mem.insert("A|100".into(), [200, 60]);
        assert_eq!(place_gadget(&cfg, 120, 150, &prim), (200, 60));
        // Memoria de OTRO monitor (solo esa): no se aplica → legacy.
        let mut cfg2 = config::Cfg::default();
        cfg2.position = Some([100, 50]);
        cfg2.monitor_mem.insert("B|100".into(), [99999, 99999]);
        assert_eq!(place_gadget(&cfg2, 120, 150, &prim), (100, 50));
    }

    #[test]
    fn mem_key_compuesta() {
        // Mismo ID PnP, escalas distintas → claves distintas (el swap
        // 2K/4K del usuario comparte el id ACR0490).
        assert_eq!(mem_key("X", 1.0), "X|100");
        assert_eq!(mem_key("X", 2.0), "X|200");
        assert_eq!(mem_key("X", 1.5), "X|150");
        assert_ne!(mem_key("X", 1.0), mem_key("X", 2.0));
    }

    #[test]
    fn parse_mem_key_roundtrip() {
        // v7: mem_key → parse_mem_key es la identidad.
        for (id, s) in [
            ("X", 1.0),
            ("X", 2.0),
            ("MONITOR\\A\\{g}\\0001", 1.5),
            ("SAM7140", 1.25),
        ] {
            let k = mem_key(id, s);
            let (pid, ps) = parse_mem_key(&k).unwrap();
            assert_eq!(pid, id);
            assert!((ps - s).abs() < 0.001);
        }
        // Clave real del log 2026-10-27.
        assert_eq!(parse_mem_key("ACR0490|100").unwrap().1, 1.0);
        // Legacy (solo id) y corruptas → None.
        assert_eq!(parse_mem_key("solo_id"), None);
        assert_eq!(parse_mem_key("X|zz"), None);
        assert_eq!(parse_mem_key("|100"), None);
    }

    #[test]
    fn place_gadget_memoria_mismo_id_distinta_escala() {
        // Mismo ID PnP pero distinta escala: la posición es por
        // (id|escala); la de 100% no se aplica en el mundo de 200%.
        let mut cfg = config::Cfg::default();
        cfg.monitor_mem.insert("A|100".into(), [200, 60]);
        let prim_2k = mon("A", 0, 0, 2560, 1400, 2560, 1400, 1.0);
        assert_eq!(place_gadget(&cfg, 120, 150, &prim_2k), (200, 60));
        // @200% sin memoria de esa escala → esquina inf-derecha.
        let prim_4k = mon("A", 0, 0, 3840, 2100, 3840, 2100, 2.0);
        assert_eq!(
            place_gadget(&cfg, 240, 300, &prim_4k),
            (3840 - 240 - 24, 2100 - 300 - 24)
        );
    }
}