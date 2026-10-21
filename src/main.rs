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

/// Milisegundos desde la época (para el timeout, sin cronómetro propio).
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
                a.tray.balloon("Botón HDR", &msg);
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
        a.tray.balloon("Botón HDR", &msg);
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

/// Tras un cambio de resolución/monitor (WM_DISPLAYCHANGE + 300 ms):
/// 1) actualiza la escala del monitor primario (lectura: GetDpiForWindow
///    sobre la ventana de control; no muta estado de ventanas);
/// 2) pide al worker de paint del gadget el chequeo de geometría
///    (TAMAÑO CANÓNICO + off-screen): al reconfigurar displays, Windows
///    recorta/redimensiona de forma NO uniforme las ventanas que no
///    caben en la nueva pantalla virtual, y el worker fuerza el tamaño
///    canónico manteniendo el centro del octágono (paso 3.5).
///
/// IMPORTANTE (AppHangB1, WER 2026-10-21 10:56): el SetWindowPos/
/// FindWindowW/ancle al fondo ahora corren en el WORKER, no en este
/// hilo: en plena reconfiguración de displays el hilo de mensajes
/// bloqueado en el stack de ventanas congelaba timers, bandeja y
/// clicks (el log moría justo en `display_change`, sin procesar el
/// click siguiente del usuario).
fn on_display_change() {
    slog(46, "display_change_apply");
    let mut app = APP.lock().unwrap();
    let Some(a) = app.as_mut() else {
        return;
    };
    if a.gadget.hwnd.is_null() {
        return;
    }
    let dpi = unsafe { win::GetDpiForWindow(a.hwnd_ctrl) };
    if dpi > 0 {
        a.primary_scale = dpi as f32 / 96.0;
    }
    gadget::request_geometry_check();
    drop(app);
    slog(47, "display_change_apply_done");
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
            a.tray.balloon("Botón HDR", "HDR No compatible");
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

/// Posición inicial: la guardada (lógica × escala) o esquina inf-derecha
/// del work area primario; si queda fuera, vuelve a la esquina.
fn place_gadget(cfg: &config::Cfg, gw: i32, gh: i32, scale: f32) -> (i32, i32) {
    let Some(av) = primary_work_area() else {
        return (0, 0);
    };
    let corner = (av.right - gw - 24, av.bottom - gh - 24);
    let (x, y) = cfg
        .position
        .map(|[px, py]| ((px as f32 * scale).round() as i32, (py as f32 * scale).round() as i32))
        .unwrap_or(corner);
    if x + gw <= av.left || x >= av.right || y + gh <= av.top || y >= av.bottom {
        corner
    } else {
        (x, y)
    }
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
    // 2) Config (corre las migraciones one-shot).
    let cfg = config::load();
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
    // 5) Escala del monitor primario.
    let primary_scale = unsafe {
        let hdc = win::GetWindowDC(std::ptr::null_mut());
        let dpi = win::GetDeviceCaps(hdc, win::LOGPIXELSX);
        win::ReleaseDC(std::ptr::null_mut(), hdc);
        if dpi > 0 {
            dpi as f32 / 96.0
        } else {
            1.0
        }
    };
    slog(5, "scale");
    // 6) Ventana de control oculta (bombea timers/mensajes).
    let hwnd_ctrl = create_ctrl_window();
    slog(6, "ctrl_window");
    if hwnd_ctrl.is_null() {
        eprintln!("BotonHDR: no se pudo crear la ventana de control");
        return;
    }
    // 7) Gadget (tamaño lógico × escala; px físicos).
    let (lw, lh) = draw::size_of(&cfg.size);
    let gw = (lw as f32 * primary_scale).round() as i32;
    let gh = (lh as f32 * primary_scale).round() as i32;
    let (gx, gy) = place_gadget(&cfg, gw, gh, primary_scale);
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
