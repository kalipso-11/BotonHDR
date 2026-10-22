//! Icono de la bandeja de sistema (Shell_NotifyIconW) + menú contextual.
//!
//! - El callback de la bandeja llega como `WM_TRAY` (WM_APP+1) a la
//!   ventana de control; `lparam` es el mensaje de mouse (WM_LBUTTONUP,
//!   WM_LBUTTONDBLCLK, WM_RBUTTONUP o WM_CONTEXTMENU).
//! - `szTip` nativo vacío por defecto: el tip de 2 líneas lo dibuja
//!   `traytip.rs`; solo en el área de desborde (sin rect) se usa el
//!   nativo de una línea ("HDR On"/"HDR Off").
//! - El menú se reconstruye en cada apertura (los checks reflejan el
//!   estado actual) y se aplica el comando DESPUÉS de soltar el lock.

use crate::draw;
use crate::lang;
use crate::win as w;
use std::ffi::c_void;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Mensaje que recibe la ventana de control ante eventos de la bandeja.
pub const WM_TRAY: u32 = w::WM_APP + 1;

const ICON_ID: u32 = 1;

/// IDs de items del menú (los submenús usan rangos base+1..base+N).
pub const IDM_STATUS: usize = 100;
pub const IDM_TOGGLE: usize = 101;
pub const IDM_GADGET: usize = 200;
pub const IDM_LOCK: usize = 202;
pub const IDM_OPACITY_BASE: usize = 300; // 301..305
pub const IDM_COLOR_BASE: usize = 400; // 401..407
pub const IDM_SIZE_BASE: usize = 500; // 501..504
pub const IDM_AUTOSTART: usize = 204;
pub const IDM_QUIT: usize = 206;
pub const IDM_LANG_BASE: usize = 600; // 601=es, 602=en

const OPACITIES: [i32; 5] = [20, 40, 60, 80, 100];

/// Pedido de re-render del icono (lo procesa el worker, fuera del hilo
/// de mensajes).
struct IconReq {
    state: String,
    accent: String,
    locked: bool,
}

pub struct Tray {
    hwnd: *mut c_void,
    /// HICON actual (envuelto en `Shared`: Send+Sync). Lo escribe el
    /// worker (reemplazos) y lo lee `nid()` (con NIF_TIP/NIF_INFO el
    /// hIcon se ignora por uFlags: race inofensivo, nunca se
    /// des-referencia el handle viejo).
    hicon: Arc<Mutex<w::Shared>>,
    /// Cola de pedidos para el worker de GDI del icono.
    tx: Option<Sender<IconReq>>,
    /// Worker de GDI: el hilo de mensajes NUNCA renderiza el icono (GDI
    /// puede bloquearse en plena reconfiguración de displays → AppHangB1,
    /// WER 2026-10-21). Si el worker se bloquea, se fuga al morir el
    /// proceso: la app sigue respondiendo.
    worker: Option<JoinHandle<()>>,
    /// Texto del tip nativo actual (evita reescribir el mismo NIM_MODIFY).
    native_tip: Option<String>,
}

fn fill_wchar(dst: &mut [u16], s: &str) {
    let u = w::utf16(s);
    let n = u.len().min(dst.len());
    dst[..n].copy_from_slice(&u[..n]);
}

impl Tray {
    pub fn new(hwnd: *mut c_void) -> Self {
        Tray {
            hwnd,
            hicon: Arc::new(Mutex::new(w::Shared::null())),
            tx: None,
            worker: None,
            native_tip: None,
        }
    }

    fn nid(&self, flags: u32) -> w::NOTIFYICONDATAW {
        w::NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<w::NOTIFYICONDATAW>() as u32,
            _pad0: 0,
            hWnd: self.hwnd,
            uID: ICON_ID,
            uFlags: flags,
            uCallbackMessage: WM_TRAY,
            hIcon: self.hicon.lock().unwrap().ptr(),
            szTip: [0; 128],
            dwState: 0,
            dwStateMask: 0,
            szInfo: [0; 256],
            uTimeout: 3000,
            szInfoTitle: [0; 64],
            dwInfoFlags: 0,
            guidItem: [0; 16],
            hBalloonIcon: std::ptr::null_mut(),
        }
    }

    /// NIM_ADD con el icono inicial (szTip vacío: lo maneja traytip.rs) +
    /// spawnea el worker de GDI del icono: desde ese punto `refresh()`
    /// solo encola (render + NIM_MODIFY fuera del hilo de mensajes).
    pub fn add(&mut self) {
        // El icono inicial sí se renderiza en el hilo de mensajes: es el
        // único render de arranque, antes del bucle, sin reconfiguración de
        // displays en curso (sin riesgo de hang).
        let icon = draw::make_tray_icon("SDR", "#22d3ee", 128, false);
        unsafe {
            if let Some(h) = icon {
                *self.hicon.lock().unwrap() = w::Shared::new(h);
            }
            let nid = self.nid(w::NIF_MESSAGE | w::NIF_ICON | w::NIF_TIP);
            let ret = w::Shell_NotifyIconW(w::NIM_ADD, &nid);
            crate::slog(20, &format!("nim_add={} gle={}", ret, w::GetLastError()));
        }
        let (tx, rx) = channel();
        let hicon = self.hicon.clone();
        let hwnd = w::Shared::new(self.hwnd);
        match std::thread::Builder::new()
            .name("botonhdr-tray-icon".into())
            .spawn(move || tray_icon_worker(hwnd.ptr(), hicon, rx))
        {
            Ok(h) => {
                self.tx = Some(tx);
                self.worker = Some(h);
            }
            Err(_) => {
                // Sin worker: refresh() queda inoperativo (drop tx → sin
                // acumulación de pedidos).
                drop(tx);
            }
        }
    }

    /// Cambia el icono: encola un pedido para el worker de GDI. NUNCA
    /// renderiza en el hilo de mensajes (GDI puede bloquearse en plena
    /// reconfiguración de displays → AppHangB1). Los pedidos se procesan
    /// en orden; el último gana. `locked` pinta chip + candado.
    pub fn refresh(&mut self, state: &str, accent: &str, locked: bool) {
        if let Some(tx) = self.tx.as_ref() {
            let _ = tx.send(IconReq {
                state: state.to_string(),
                accent: accent.to_string(),
                locked,
            });
        }
    }

    /// Tip nativo de 1 línea (solo en el área de desborde).
    pub fn set_native_tip(&mut self, tip: Option<&str>) {
        let text: Option<String> = tip.map(|t| t.to_string());
        if text == self.native_tip {
            return;
        }
        unsafe {
            let mut nid = self.nid(w::NIF_MESSAGE | w::NIF_TIP);
            if let Some(t) = &text {
                fill_wchar(&mut nid.szTip, t);
            }
            let _ = w::Shell_NotifyIconW(w::NIM_MODIFY, &nid);
        }
        self.native_tip = text;
    }

    /// Balloon de aviso (3 s).
    pub fn balloon(&mut self, title: &str, text: &str) {
        unsafe {
            let mut nid = self.nid(w::NIF_MESSAGE | w::NIF_INFO);
            fill_wchar(&mut nid.szInfo, text);
            fill_wchar(&mut nid.szInfoTitle, title);
            nid.dwInfoFlags = w::NIIF_WARNING;
            let _ = w::Shell_NotifyIconW(w::NIM_MODIFY, &nid);
        }
    }

    /// Rect del icono en coords de pantalla (`Shell_NotifyIconGetRect`).
    /// `None` si la API falla o el icono está en el área de desborde.
    pub fn get_position(&self) -> Option<w::RECT> {
        unsafe {
            let nii = w::NOTIFYICONIDENTIFIER {
                cbSize: std::mem::size_of::<w::NOTIFYICONIDENTIFIER>() as u32,
                _pad0: 0,
                hWnd: self.hwnd,
                uID: ICON_ID,
                guidItem: [0; 16],
            };
            let mut rect: w::RECT = std::mem::zeroed();
            if w::Shell_NotifyIconGetRect(&nii, &mut rect) != 0 {
                return None;
            }
            Some(rect)
        }
    }

    /// NIM_DELETE. El HICON lo destruye el worker al salir (cerrar el
    /// canal detiene su recv); no se libera aquí para no competir con un
    /// NIM_MODIFY en curso del worker.
    pub fn delete(&mut self) {
        unsafe {
            let nid = self.nid(0);
            let _ = w::Shell_NotifyIconW(w::NIM_DELETE, &nid);
        }
        self.tx = None; // el worker sale del recv() y libera el HICON
    }
}

/// Worker de GDI del icono de bandeja.
///
/// ÚNICO dueño del ciclo de vida de los HICON: crea el nuevo icono, hace
/// el NIM_MODIFY y destruye el viejo (al salir, destruye el último).
/// `Shell_NotifyIconW` es thread-safe: el hWnd solo identifica el icono;
/// el callback sigue llegando al hilo de mensajes. Si GDI se bloquea en
/// plena reconfiguración de displays, el hilo que se traba es ESTE (se
/// fuga al morir el proceso), no el de mensajes: no hay AppHangB1.
fn tray_icon_worker(
    hwnd: *mut c_void,
    hicon: Arc<Mutex<w::Shared>>,
    rx: Receiver<IconReq>,
) {
    crate::slog(22, "tray_icon_worker_start");
    while let Ok(req) = rx.recv() {
        crate::slog(22, &format!("tray_icon_req {} {}", req.state, req.locked));
        let t0 = std::time::Instant::now();
        let new = match draw::make_tray_icon(&req.state, &req.accent, 128, req.locked) {
            Some(h) => h,
            None => {
                crate::slog(23, "tray_icon_make_none");
                continue;
            }
        };
        crate::slog(
            23,
            &format!("tray_icon_make_ok ms={}", t0.elapsed().as_millis()),
        );
        let nid = w::NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<w::NOTIFYICONDATAW>() as u32,
            _pad0: 0,
            hWnd: hwnd,
            uID: ICON_ID,
            uFlags: w::NIF_MESSAGE | w::NIF_ICON,
            uCallbackMessage: WM_TRAY,
            hIcon: new,
            szTip: [0; 128],
            dwState: 0,
            dwStateMask: 0,
            szInfo: [0; 256],
            uTimeout: 3000,
            szInfoTitle: [0; 64],
            dwInfoFlags: 0,
            guidItem: [0; 16],
            hBalloonIcon: std::ptr::null_mut(),
        };
        let (ret, gle) = unsafe {
            let ret = w::Shell_NotifyIconW(w::NIM_MODIFY, &nid);
            (ret, w::GetLastError())
        };
        if ret == 0 {
            crate::slog(21, &format!("nim_modify={} gle={}", ret, gle));
        }
        {
            let mut g = hicon.lock().unwrap();
            let old = g.ptr();
            *g = w::Shared::new(new);
            if !old.is_null() {
                unsafe { w::DestroyIcon(old) };
            }
        }
    }
    // Salida (canal cerrado por delete): destruye el icono actual.
    let cur = hicon.lock().unwrap().ptr();
    if !cur.is_null() {
        unsafe { w::DestroyIcon(cur) };
    }
}

/// Texto del tip nativo de 1 línea según el estado.
pub fn native_tip_text(state: &str) -> &'static str {
    if state == "HDR" {
        "HDR On"
    } else {
        "HDR Off"
    }
}

/// Muestra el menú contextual en el cursor y aplica el comando elegido.
///
/// Regla de oro: `TrackPopupMenu` bombea mensajes; el lock de `crate::APP`
/// debe estar SUELTO mientras se muestra (si no, deadlock contra los
/// timers del mismo hilo).
pub fn open_menu(hwnd_ctrl: *mut c_void) {
    crate::slog(31, "menu_begin");
    // 1) Estado para los checks (se lee y se suelta al instante).
    let (state, color, size, opacity, gadget_visible, auto, locked, lock_state, language) = {
        let app = crate::APP.lock().unwrap();
        let Some(a) = app.as_ref() else {
            return;
        };
        (
            a.state,
            a.cfg.color.clone(),
            a.cfg.size.clone(),
            a.cfg.opacity,
            a.cfg.gadget_visible,
            crate::autostart::is_enabled(),
            a.locked,
            a.lock_state,
            a.cfg.language.clone(),
        )
    };

    // 2) Menú reconstruido (los checks reflejan el estado actual).
    let menu = unsafe { w::CreatePopupMenu() };
    unsafe {
        let status = w::utf16(&format!(
            "{} {state}",
            lang::t(&language, "Estado actual:", "Current state:")
        ));
        w::AppendMenuW(menu, w::MF_STRING | w::MF_GRAYED, IDM_STATUS, status.as_ptr());
        let toggle = w::utf16(&format!(
            "{} {}",
            lang::t(&language, "Cambiar a", "Switch to"),
            if state == "HDR" { "SDR" } else { "HDR" }
        ));
        w::AppendMenuW(menu, w::MF_STRING, IDM_TOGGLE, toggle.as_ptr());
        // "Bloquear SDR/HDR": con tilde mientras el estado actual está
        // bloqueado (impide que cualquier app/cable cambie el estado).
        let lock = w::utf16(&format!(
            "{} {}",
            lang::t(&language, "Bloquear", "Lock"),
            if locked { lock_state } else { state }
        ));
        w::AppendMenuW(
            menu,
            w::MF_STRING | if locked { w::MF_CHECKED } else { 0 },
            IDM_LOCK,
            lock.as_ptr(),
        );
        w::AppendMenuW(menu, w::MF_SEPARATOR, 0, std::ptr::null());
        let gadget = w::utf16(lang::t(
            &language,
            "Gadget del escritorio",
            "Desktop gadget",
        ));
        w::AppendMenuW(
            menu,
            w::MF_STRING | if gadget_visible { w::MF_CHECKED } else { 0 },
            IDM_GADGET,
            gadget.as_ptr(),
        );

        let sub_op = w::CreatePopupMenu();
        for (i, &pct) in OPACITIES.iter().enumerate() {
            let label = w::utf16(&format!("{pct} %"));
            w::AppendMenuW(
                sub_op,
                w::MF_STRING | if pct == opacity { w::MF_CHECKED } else { 0 },
                IDM_OPACITY_BASE + i + 1,
                label.as_ptr(),
            );
        }
        let title = w::utf16(lang::t(&language, "Opacidad", "Opacity"));
        w::AppendMenuW(menu, w::MF_POPUP, sub_op as usize, title.as_ptr());

        let sub_color = w::CreatePopupMenu();
        for (i, (_name, hexv)) in draw::COLORS.iter().enumerate() {
            let label = w::utf16(lang::color_name(hexv, &language));
            w::AppendMenuW(
                sub_color,
                w::MF_STRING | if *hexv == color { w::MF_CHECKED } else { 0 },
                IDM_COLOR_BASE + i + 1,
                label.as_ptr(),
            );
        }
        let title = w::utf16(lang::t(&language, "Color", "Color"));
        w::AppendMenuW(menu, w::MF_POPUP, sub_color as usize, title.as_ptr());

        let sub_size = w::CreatePopupMenu();
        for (i, (name, _)) in draw::SIZES.iter().enumerate() {
            let label = w::utf16(lang::size_name(name, &language));
            w::AppendMenuW(
                sub_size,
                w::MF_STRING | if *name == size { w::MF_CHECKED } else { 0 },
                IDM_SIZE_BASE + i + 1,
                label.as_ptr(),
            );
        }
        let title = w::utf16(lang::t(&language, "Tamaño", "Size"));
        w::AppendMenuW(menu, w::MF_POPUP, sub_size as usize, title.as_ptr());

        // Idioma: aplica al instante (el menú se reconstruye en cada
        // apertura) y persiste en config.json.
        let sub_lang = w::CreatePopupMenu();
        w::AppendMenuW(
            sub_lang,
            w::MF_STRING | if language == "es" { w::MF_CHECKED } else { 0 },
            IDM_LANG_BASE + 1,
            w::utf16("Español").as_ptr(),
        );
        w::AppendMenuW(
            sub_lang,
            w::MF_STRING | if language != "es" { w::MF_CHECKED } else { 0 },
            IDM_LANG_BASE + 2,
            w::utf16("English").as_ptr(),
        );
        let title = w::utf16(lang::t(&language, "Idioma", "Language"));
        w::AppendMenuW(menu, w::MF_POPUP, sub_lang as usize, title.as_ptr());

        let auto_lbl =
            w::utf16(lang::t(&language, "Iniciar con Windows", "Start with Windows"));
        w::AppendMenuW(
            menu,
            w::MF_STRING | if auto { w::MF_CHECKED } else { 0 },
            IDM_AUTOSTART,
            auto_lbl.as_ptr(),
        );
        w::AppendMenuW(menu, w::MF_SEPARATOR, 0, std::ptr::null());
        let quit = w::utf16(lang::t(&language, "Salir", "Exit"));
        w::AppendMenuW(menu, w::MF_STRING, IDM_QUIT, quit.as_ptr());
    }

    // 3) Suprimir el tip propio mientras el menú está abierto.
    {
        let mut app = crate::APP.lock().unwrap();
        if let Some(a) = app.as_mut() {
            a.tip_suppressed = true;
        }
    }

    // 4) Track (bloquea; bombea mensajes → el lock debe estar suelto).
    let cmd = unsafe {
        let mut pt: w::POINT = std::mem::zeroed();
        w::GetCursorPos(&mut pt);
        w::SetForegroundWindow(hwnd_ctrl);
        let cmd = w::TrackPopupMenu(
            menu,
            w::TPM_RETURNCMD | w::TPM_LEFTALIGN | w::TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            hwnd_ctrl,
            std::ptr::null(),
        );
        // Re-entrega de mensajes tras el modal del menú.
        w::PostMessageW(hwnd_ctrl, w::WM_NULL, 0, 0);
        w::DestroyMenu(menu);
        crate::slog(32, &format!("menu_cmd={} x={} y={}", cmd, pt.x, pt.y));
        cmd as usize
    };

    {
        let mut app = crate::APP.lock().unwrap();
        if let Some(a) = app.as_mut() {
            a.tip_suppressed = false;
        }
    }

    // 5) Aplicar el comando (0 = cerró sin elegir).
    if cmd == 0 {
        return;
    }
    match cmd {
        IDM_TOGGLE => crate::request_toggle(),
        IDM_LOCK => crate::toggle_lock(),
        IDM_GADGET => {
            let mut app = crate::APP.lock().unwrap();
            let Some(a) = app.as_mut() else {
                return;
            };
            a.cfg.gadget_visible = !a.cfg.gadget_visible;
            crate::config::save(&a.cfg);
            if a.cfg.gadget_visible {
                a.gadget.show();
            } else {
                a.gadget.hide();
            }
        }
        c if (IDM_OPACITY_BASE + 1..=IDM_OPACITY_BASE + 5).contains(&c) => {
            let pct = OPACITIES[c - IDM_OPACITY_BASE - 1];
            let mut app = crate::APP.lock().unwrap();
            let Some(a) = app.as_mut() else {
                return;
            };
            a.cfg.opacity = pct;
            crate::config::save(&a.cfg);
            a.gadget.set_opacity_pct(pct);
        }
        c if (IDM_COLOR_BASE + 1..=IDM_COLOR_BASE + 7).contains(&c) => {
            let hexv = draw::COLORS[c - IDM_COLOR_BASE - 1].1.to_string();
            let mut app = crate::APP.lock().unwrap();
            let Some(a) = app.as_mut() else {
                return;
            };
            a.cfg.color = hexv.clone();
            crate::config::save(&a.cfg);
            a.gadget.set_accent(&hexv);
            crate::refresh_icon(a);
        }
        c if (IDM_SIZE_BASE + 1..=IDM_SIZE_BASE + 4).contains(&c) => {
            let name = draw::SIZES[c - IDM_SIZE_BASE - 1].0.to_string();
            let mut app = crate::APP.lock().unwrap();
            let Some(a) = app.as_mut() else {
                return;
            };
            if name != a.cfg.size {
                a.cfg.size = name.clone();
                crate::config::save(&a.cfg);
                a.gadget.set_size(&name);
            }
        }
        IDM_AUTOSTART => {
            let want = !crate::autostart::is_enabled();
            let ok = crate::autostart::set_enabled(want);
            if !ok {
                let mut app = crate::APP.lock().unwrap();
                if let Some(a) = app.as_mut() {
                    a.tray.balloon(
                        lang::t(&a.cfg.language, "Botón HDR", "HDR Button"),
                        lang::t(
                            &a.cfg.language,
                            "No se pudo cambiar la opción de arranque.",
                            "Could not change the startup option.",
                        ),
                    );
                }
            }
        }
        c if (IDM_LANG_BASE + 1..=IDM_LANG_BASE + 2).contains(&c) => {
            let new = if c == IDM_LANG_BASE + 1 { "es" } else { "en" };
            let mut app = crate::APP.lock().unwrap();
            let Some(a) = app.as_mut() else {
                return;
            };
            if a.cfg.language != new {
                a.cfg.language = new.to_string();
                crate::config::save(&a.cfg);
            }
        }
        IDM_QUIT => unsafe { w::PostQuitMessage(0) },
        _ => {}
    }
}
