//! FFI cruda de Win32/GDI/shell para toda la app.
//!
//! - DisplayConfig / ntdll / CfgMgr32 / registro HKLM: firmas x64.
//! - UI (ventanas, mensajes, menú, layered, cursor, métricas, DPI,
//!   GDI, tray): firmas estándar x64. Los tamaños de structs se
//!   validan con asserts de compilación (verificados en Windows 11 x64:
//!   NOTIFYICONDATAW=976, WNDCLASSEXW=80, etc.).
//!
//! Los nombres de structs/campos conservan la nomenclatura de la
//! documentación de Win32 (no snake_case).
#![allow(non_snake_case)]

use core::ffi::c_void;
use std::os::raw::{c_int, c_ulong};

/// Puntero crudo que puede cruzar threads.
///
/// Solo para el hwnd del worker de toggle (single-thread, nunca se
/// libera en paralelo): los recursos se crean y destruyen siempre en
/// el hilo de mensajes.
#[derive(Clone, Copy)]
pub struct Shared(*mut c_void);

// Safe: ver nota del tipo (manejo single-thread de los recursos).
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    pub const fn new(p: *mut c_void) -> Self {
        Shared(p)
    }
    pub const fn null() -> Self {
        Shared(std::ptr::null_mut())
    }
    pub fn ptr(&self) -> *mut c_void {
        self.0
    }
    pub fn set(&mut self, p: *mut c_void) {
        self.0 = p
    }
    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }
}

// ---------------------------------------------------------------------------
// Estructuras de DisplayConfig / PnP (ABI estable documentada por Microsoft)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct FILETIME {
    pub dwLowDateTime: u32,
    pub dwHighDateTime: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LUID {
    pub LowPart: u32,
    pub HighPart: i32,
}

/// Cabecera común de los structs de info de DisplayConfig (20 B).
#[repr(C)]
pub struct DCHeader {
    pub r#type: u32,
    pub size: u32,
    pub adapterId: LUID,
    pub id: u32,
}

/// DISPLAYCONFIG_MODE_INFO (64 B, x64).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DCModeInfo {
    pub infoType: u32,
    pub id: u32,
    pub adapterId: LUID,
    pub tail: [u8; 48],
}

/// DISPLAYCONFIG_TARGET_NAME (420 B, x64).
#[repr(C)]
pub struct DCTargetName {
    pub header: DCHeader,
    pub flags: u32,
    pub outputTechnology: u32,
    pub edidMfrId: u16,
    pub edidProdId: u16,
    pub connectorInstance: u32,
    pub friendlyName: [u16; 64],
    pub devicePath: [u16; 128],
}

/// GET_ADVANCED_COLOR_INFO (32 B, legacy).
#[repr(C)]
pub struct DCGetAdvColorInfo {
    pub header: DCHeader,
    pub value: u32,
    pub colorEncoding: u32,
    pub bitsPerChannel: u32,
}

/// GET_ADVANCED_COLOR_INFO_2 (36 B, 24H2+).
#[repr(C)]
pub struct DCGetAdvColorInfo2 {
    pub header: DCHeader,
    pub value: u32,
    pub colorEncoding: u32,
    pub bitsPerChannel: u32,
    pub activeColorMode: u32,
}

/// SET_ADVANCED_COLOR_STATE / SET_HDR_STATE (24 B).
#[repr(C)]
pub struct DCSetColorState {
    pub header: DCHeader,
    pub value: u32,
}

/// Validación de tamaños: si la ABI no coincide, que falle en tiempo de
/// compilación (assert de compilación).
const _: () = {
    assert!(core::mem::size_of::<DCHeader>() == 20);
    assert!(core::mem::size_of::<DCModeInfo>() == 64);
    assert!(core::mem::size_of::<DCTargetName>() == 420);
    assert!(core::mem::size_of::<DCGetAdvColorInfo>() == 32);
    assert!(core::mem::size_of::<DCGetAdvColorInfo2>() == 36);
    assert!(core::mem::size_of::<DCSetColorState>() == 24);
};

#[repr(C)]
pub struct DISPLAY_DEVICEW {
    pub cb: u32,
    pub DeviceName: [u16; 32],
    pub DeviceString: [u16; 128],
    pub StateFlags: u32,
    pub DeviceID: [u16; 128],
    pub DeviceKey: [u16; 128],
}

#[repr(C)]
pub struct OSVERSIONINFOEXW {
    pub dwOSVersionInfoSize: u32,
    pub dwMajorVersion: u32,
    pub dwMinorVersion: u32,
    pub dwBuildNumber: u32,
    pub dwPlatformId: u32,
    pub szCSDVersion: [u16; 128],
    pub wServicePackMajor: u16,
    pub wServicePackMinor: u16,
    pub wSuiteMask: u16,
    pub wProductType: u8,
    pub wReserved: u8,
}

// ---------------------------------------------------------------------------
// UI: structs básicos
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SIZE {
    pub cx: i32,
    pub cy: i32,
}

#[repr(C)]
pub struct MSG {
    pub hwnd: *mut c_void,
    pub message: u32,
    pub wParam: usize,
    pub lParam: isize,
    pub time: u32,
    pub pt: POINT,
}

#[repr(C)]
pub struct PAINTSTRUCT {
    pub hdc: *mut c_void,
    pub fErase: i32,
    pub rcPaint: RECT,
    pub fRestore: i32,
    pub fIncUpdate: i32,
    pub rgbReserved: [u8; 32],
}

#[repr(C)]
pub struct BLENDFUNCTION {
    pub blendOp: u8,
    pub blendFlags: u8,
    pub sourceConstantAlpha: u8,
    pub alphaFormat: u8,
}

pub type WndProc = unsafe extern "system" fn(
    hwnd: *mut c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize;

/// WNDCLASSEXW (80 B en x64, verificado en Win11).
#[repr(C)]
pub struct WNDCLASSEXW {
    pub cbSize: u32,
    pub style: u32,
    pub lpfnWndProc: WndProc,
    pub cbClsExtra: i32,
    pub cbWndExtra: i32,
    pub hInstance: *mut c_void,
    pub hIcon: *mut c_void,
    pub hCursor: *mut c_void,
    pub hbrBackground: *mut c_void,
    pub lpszMenuName: *const u16,
    pub lpszClassName: *const u16,
    pub hIconSm: *mut c_void,
}

/// MONITORINFO (cbSize=32).
#[repr(C)]
pub struct MONITORINFO {
    pub cbSize: u32,
    pub rcMonitor: RECT,
    pub rcWork: RECT,
    pub dwFlags: u32,
}

const _: () = {
    assert!(core::mem::size_of::<MONITORINFO>() == 40); // x64: 4+16+16+4
};

#[repr(C)]
pub struct ICONINFO {
    pub fIcon: i32,
    pub xHotspot: u32,
    pub yHotspot: u32,
    pub hbmMask: *mut c_void,
    pub hbmColor: *mut c_void,
}

#[repr(C)]
pub struct BITMAPINFOHEADER {
    pub biSize: u32,
    pub biWidth: i32,
    pub biHeight: i32,
    pub biPlanes: u16,
    pub biBitCount: u16,
    pub biCompression: u32,
    pub biSizeImage: u32,
    pub biXPelsPerMeter: i32,
    pub biYPelsPerMeter: i32,
    pub biClrUsed: u32,
    pub biClrImportant: u32,
}

#[repr(C)]
pub struct RGBQUAD {
    pub b: u8,
    pub g: u8,
    pub r: u8,
    pub a: u8,
}

#[repr(C)]
pub struct BITMAPINFO {
    pub bmiHeader: BITMAPINFOHEADER,
    pub bmiColors: RGBQUAD,
}

/// NOTIFYICONIDENTIFIER (identifica un icono: hWnd+uID o guidItem).
#[repr(C)]
pub struct NOTIFYICONIDENTIFIER {
    pub cbSize: u32,
    pub _pad0: u32,
    pub hWnd: *mut c_void,
    pub uID: u32,
    pub guidItem: [u8; 16],
}

const _: () = {
    assert!(core::mem::size_of::<NOTIFYICONIDENTIFIER>() == 40); // x64: 36 + pad a 8
};

/// NOTIFYICONDATAW (976 B en x64, verificado en Win11).
#[repr(C)]
pub struct NOTIFYICONDATAW {
    pub cbSize: u32,
    pub _pad0: u32,
    pub hWnd: *mut c_void,
    pub uID: u32,
    pub uFlags: u32,
    pub uCallbackMessage: u32,
    pub hIcon: *mut c_void,
    pub szTip: [u16; 128],
    pub dwState: u32,
    pub dwStateMask: u32,
    pub szInfo: [u16; 256],
    pub uTimeout: u32,
    pub szInfoTitle: [u16; 64],
    pub dwInfoFlags: u32,
    pub guidItem: [u8; 16],
    pub hBalloonIcon: *mut c_void,
}

const _: () = {
    assert!(core::mem::size_of::<POINT>() == 8);
    assert!(core::mem::size_of::<RECT>() == 16);
    assert!(core::mem::size_of::<SIZE>() == 8);
    assert!(core::mem::size_of::<MSG>() == 48);
    assert!(core::mem::size_of::<PAINTSTRUCT>() == 72);
    assert!(core::mem::size_of::<BLENDFUNCTION>() == 4);
    assert!(core::mem::size_of::<WNDCLASSEXW>() == 80);
    assert!(core::mem::size_of::<ICONINFO>() == 32); // x64 (20 era x86)
    assert!(core::mem::size_of::<BITMAPINFO>() == 44);
    assert!(core::mem::size_of::<NOTIFYICONDATAW>() == 976);
};

// ---------------------------------------------------------------------------
// INPUT (SendInput) — layout x64 según la documentación de Win32
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KEYBDINPUT {
    pub wVk: u16,
    pub wScan: u16,
    pub dwFlags: u32,
    pub time: u32,
    pub dwExtraInfo: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MOUSEINPUT {
    pub dx: i32,
    pub dy: i32,
    pub mouseData: u32,
    pub dwFlags: u32,
    pub time: u32,
    pub dwExtraInfo: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HARDWAREINPUT {
    pub u: u16,
    pub f: u16,
    pub d: u32,
    pub i: usize,
}

#[repr(C)]
pub struct INPUT {
    pub r#type: u32,
    pub u: INPUT_0,
}

#[repr(C)]
pub union INPUT_0 {
    pub ki: KEYBDINPUT,
    pub mi: MOUSEINPUT,
    pub hi: HARDWAREINPUT,
}

const _: () = {
    assert!(core::mem::size_of::<INPUT>() == 40);
};

// ---------------------------------------------------------------------------
// UI: constantes
// ---------------------------------------------------------------------------
// Mensajes
pub const WM_NULL: u32 = 0x0000;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_PAINT: u32 = 0x000F;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_QUIT: u32 = 0x0012;
pub const WM_ERASEBKGND: u32 = 0x0014;
pub const WM_SETCURSOR: u32 = 0x0020;
pub const WM_NCHITTEST: u32 = 0x0084;
pub const WM_COMMAND: u32 = 0x0111;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_LBUTTONDBLCLK: u32 = 0x0203;
pub const WM_RBUTTONDOWN: u32 = 0x0204;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_CONTEXTMENU: u32 = 0x007B;
pub const WM_DPICHANGED: u32 = 0x02E0;
pub const WM_DISPLAYCHANGE: u32 = 0x007E;
pub const WM_PRINT: u32 = 0x0317;
pub const WM_PRINTCLIENT: u32 = 0x0318;
pub const WM_APP: u32 = 0x8000;
pub const WM_HOTKEY: u32 = 0x0312;

// Hotkeys (Win+Alt+B se captura mientras un estado está bloqueado: solo
// un proceso puede retener un hotkey global; mientras lo tenemos, el
// sistema no puede cambiar el HDR con el atajo).
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_WIN: u32 = 0x0008;
pub const VK_B: u32 = 0x42;

// Estilos de ventana
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_EX_TOPMOST: u32 = 0x0000_0008;
pub const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
pub const WS_EX_LAYERED: u32 = 0x0008_0000;
pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
pub const GWL_EXSTYLE: i32 = -20;

// ShowWindow
pub const SW_HIDE: i32 = 0;
pub const SW_SHOWNOACTIVATE: i32 = 4;
pub const SW_SHOW: i32 = 5;
pub const SW_SHOWNA: i32 = 8;

// SetWindowPos
pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOZORDER: u32 = 0x0004;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_FRAMECHANGED: u32 = 0x0020;
pub const SWP_SHOWWINDOW: u32 = 0x0040;

// Cursor
pub const IDC_ARROW: i32 = 32512;
pub const IDC_HAND: i32 = 32649;

// Métricas de sistema
pub const SM_DBLCLKTIME: i32 = 0;
pub const SM_X: i32 = 0;
pub const SM_Y: i32 = 1;
pub const SM_CXSCREEN: i32 = 8;
pub const SM_CYSCREEN: i32 = 10;
pub const SM_XVIRTUALSCREEN: i32 = 76;
pub const SM_YVIRTUALSCREEN: i32 = 77;
pub const SM_CXVIRTUALSCREEN: i32 = 78;
pub const SM_CYVIRTUALSCREEN: i32 = 79;

// SystemParametersInfo
pub const SPI_GETWORKAREA: u32 = 0x0030;
pub const SPIF_UPDATEINIFILE: u32 = 0x01;
pub const SPIF_SENDCHANGE: u32 = 0x02;

// Layered (valores de MSDN UpdateLayeredWindow, dwFlags)
pub const ULW_COLORKEY: u32 = 0x0001; // usar crKey como color transparente
pub const ULW_ALPHA: u32 = 0x0002; // usar pblend (alpha per-píxel del buffer)
pub const ULW_OPAQUE: u32 = 0x0004; // ventana opaca
/// OJO (bug real que se caza fácil): ULW_ALPHA es 0x0002, NO 0x0001.
/// Con 0x0001 (ULW_COLORKEY) y crKey=0 la ventana se compone como
/// "blanco/negro": píxeles RGB==(0,0,0) transparentes y el resto
/// OPACO — el gadget se veía como una forma oscura plana sobre negro
/// en vez de compisarse con alpha per-píxel sobre el wallpaper.
pub const AC_SRC_OVER: u8 = 0;
pub const AC_SRC_ALPHA: u8 = 1;
pub const LWA_ALPHA: u32 = 0x0000_0002; // SetLayeredWindowAttributes

// Regiones (gdi32)
pub const COMBINE_REGIONS: i32 = 0;
pub const RGN_AND: i32 = 1;
pub const RGN_OR: i32 = 2;
pub const RGN_XOR: i32 = 3;
pub const RGN_COPY: i32 = 4;

// Menú
pub const MF_STRING: u32 = 0x0000;
pub const MF_GRAYED: u32 = 0x0001;
pub const MF_CHECKED: u32 = 0x0008;
pub const MF_POPUP: u32 = 0x0010;
pub const MF_SEPARATOR: u32 = 0x0800;
pub const TPM_LEFTALIGN: u32 = 0x0000;
pub const TPM_RIGHTBUTTON: u32 = 0x0002;
pub const TPM_RETURNCMD: u32 = 0x0100;

// Tray (Shell_NotifyIconW)
pub const NIM_ADD: u32 = 0;
pub const NIM_MODIFY: u32 = 1;
pub const NIM_DELETE: u32 = 2;
pub const NIM_GETPOSITION: u32 = 13;
pub const NIF_MESSAGE: u32 = 0x0001;
pub const NIF_ICON: u32 = 0x0002;
pub const NIF_INFO: u32 = 0x0100;
pub const NIF_TIP: u32 = 0x0004;
pub const NIIF_WARNING: u32 = 0x02;
pub const NIS_HIDDEN: u32 = 0x0001;

// GDI
pub const BI_RGB: u32 = 0;
pub const DIB_RGB_COLORS: u32 = 0;
pub const LOGPIXELSX: i32 = 88; // GetDeviceCaps: 88=LOGPIXELSX (9 es ASPECT, bug viejo)
pub const TRANSPARENT: i32 = 1; // BkMode
pub const ANTIALIASED_QUALITY: i32 = 4;
pub const FW_NORMAL: i32 = 400;
pub const FW_BOLD: i32 = 700;
pub const DT_LEFT: u32 = 0;
pub const DT_CENTER: u32 = 1;
pub const DT_VCENTER: u32 = 4;
pub const DT_SINGLELINE: u32 = 0x20;

// DPI awareness
pub const DPI_AWARENESS_CONTEXT_UNAWARE: isize = -1;
pub const DPI_AWARENESS_CONTEXT_SYSTEM_AWARE: isize = -3;
pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

// Espera de objetos
pub const WAIT_OBJECT_0: u32 = 0;
pub const WAIT_TIMEOUT: u32 = 258;
pub const EVENT_MODIFY_STATE: u32 = 0x0002;

// ---------------------------------------------------------------------------
// user32: DisplayConfig + EnumDisplayDevicesW + SendInput
// ---------------------------------------------------------------------------
#[link(name = "user32")]
extern "system" {
    pub fn GetDisplayConfigBufferSizes(
        flags: u32,
        path_count: *mut u32,
        mode_count: *mut u32,
    ) -> c_int;
    pub fn QueryDisplayConfig(
        flags: u32,
        path_count: *mut u32,
        paths: *mut c_void,
        mode_count: *mut u32,
        modes: *mut c_void,
        string_info: u32,
    ) -> c_int;
    pub fn DisplayConfigGetDeviceInfo(info: *mut c_void) -> c_int;
    pub fn DisplayConfigSetDeviceInfo(info: *mut c_void) -> c_int;
    pub fn EnumDisplayDevicesW(
        device_name: *const u16,
        device_index: u32,
        device_info: *mut DISPLAY_DEVICEW,
        flags: u32,
    ) -> c_int;
    pub fn MapVirtualKeyW(uCode: u32, uMapType: u32) -> u32;
    pub fn SendInput(cInputs: u32, pInputs: *const INPUT, cbSize: c_int) -> u32;
}

// ---------------------------------------------------------------------------
// user32: ventanas, mensajes, input, cursor, métricas, menú, layered, DPI
// ---------------------------------------------------------------------------
#[link(name = "user32")]
extern "system" {
    pub fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    pub fn RegisterClassExW(cls: *const WNDCLASSEXW) -> u16;
    pub fn CreateWindowExW(
        exstyle: u32,
        cls: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: *mut c_void,
        menu: *mut c_void,
        inst: *mut c_void,
        param: *const c_void,
    ) -> *mut c_void;
    pub fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
    pub fn UpdateWindow(hwnd: *mut c_void) -> i32;
    pub fn InvalidateRect(hwnd: *mut c_void, rect: *const RECT, erase: i32) -> i32;
    pub fn ValidateRect(hwnd: *mut c_void, rect: *const RECT) -> i32;
    pub fn DestroyWindow(hwnd: *mut c_void) -> i32;
    pub fn DefWindowProcW(hwnd: *mut c_void, msg: u32, wParam: usize, lParam: isize) -> isize;
    pub fn GetMessageW(msg: *mut MSG, hwnd: *mut c_void, min: u32, max: u32) -> i32;
    pub fn TranslateMessage(msg: *const MSG) -> i32;
    pub fn DispatchMessageW(msg: *const MSG) -> isize;
    pub fn PostQuitMessage(code: i32) -> ();
    pub fn PostMessageW(hwnd: *mut c_void, msg: u32, wParam: usize, lParam: isize) -> i32;
    pub fn SetWindowPos(
        hwnd: *mut c_void,
        insertafter: *mut c_void,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    pub fn FindWindowW(cls: *const u16, name: *const u16) -> *mut c_void;
    pub fn SetCursor(cursor: *mut c_void) -> *mut c_void;
    pub fn LoadCursorW(inst: *mut c_void, name: i32) -> *mut c_void;
    pub fn GetCursorPos(pt: *mut POINT) -> i32;
    pub fn SetCapture(hwnd: *mut c_void) -> *mut c_void;
    pub fn ReleaseCapture() -> i32;
    pub fn ClientToScreen(hwnd: *mut c_void, pt: *mut POINT) -> i32;
    pub fn GetSystemMetrics(index: i32) -> i32;
    pub fn SystemParametersInfoW(action: u32, param: u32, pvparam: *mut c_void, ini: u32) -> i32;
    pub fn GetWindowRect(hwnd: *mut c_void, rect: *mut RECT) -> i32;
    pub fn GetLastError() -> u32;
    pub fn GetWindowLongPtrW(hwnd: *mut c_void, index: i32) -> isize;
    pub fn SetWindowLongPtrW(hwnd: *mut c_void, index: i32, new: isize) -> isize;
    pub fn GetWindowDC(hwnd: *mut c_void) -> *mut c_void;
    pub fn ReleaseDC(hwnd: *mut c_void, dc: *mut c_void) -> u32;
    pub fn GetDeviceCaps(dc: *mut c_void, index: i32) -> i32;
    pub fn BeginPaint(hwnd: *mut c_void, ps: *mut PAINTSTRUCT) -> *mut c_void;
    pub fn EndPaint(hwnd: *mut c_void, ps: *const PAINTSTRUCT) -> i32;
    pub fn UpdateLayeredWindow(
        hwnd: *mut c_void,
        hdcDst: *mut c_void,
        pptDst: *const POINT,
        psize: *const SIZE,
        hdcSrc: *mut c_void,
        pptSrc: *const POINT,
        crkey: u32,
        pblend: *const BLENDFUNCTION,
        flags: u32,
    ) -> i32;
    /// Opacidad global de una ventana layered. OJO (medido en vivo en
    /// esta máquina, Win11 10.0.26300): aplana la superficie contra
    /// negro antes de aplicar bAlpha → las zonas α=0 salen OSCURAS
    /// (wallpaper a ~100−bAlpha %), no transparentes. Por eso la app no
    /// la usa: la opacidad se hornea en el buffer ULW
    /// (`Scanbuf::apply_opacity`). Quirk documentado por MS: una vez
    /// llamado, los UpdateLayeredWindow siguientes fallan con error 87
    /// hasta limpiar y reponer el bit WS_EX_LAYERED (GWL_EXSTYLE).
    pub fn SetLayeredWindowAttributes(
        hwnd: *mut c_void,
        crKey: u32,
        bAlpha: u8,
        dwFlags: u32,
    ) -> i32;
    /// Recorta la ventana a una región (la región no se destruye: quien
    /// llama debe DeleteObject). NO usado en vivo: CreatePolygonRgn
    /// devuelve NULL para procesos nuevos en este entorno (gle 6/183) y la
    /// forma de las ventanas layered ya viene del canal alpha real del ULW.
    pub fn SetWindowRgn(hwnd: *mut c_void, hRgn: *mut c_void, bRedraw: i32) -> i32;
    // Menús
    pub fn CreatePopupMenu() -> *mut c_void;
    pub fn AppendMenuW(hmenu: *mut c_void, flags: u32, id: usize, item: *const u16) -> i32;
    pub fn CheckMenuItem(hmenu: *mut c_void, id: usize, flags: u32) -> i32;
    pub fn EnableMenuItem(hmenu: *mut c_void, id: usize, flags: u32) -> i32;
    pub fn DeleteMenu(hmenu: *mut c_void, pos: usize, flags: u32) -> i32;
    pub fn DestroyMenu(hmenu: *mut c_void) -> i32;
    pub fn TrackPopupMenu(
        hmenu: *mut c_void,
        flags: u32,
        x: i32,
        y: i32,
        reserved: i32,
        hwnd: *mut c_void,
        rect: *const RECT,
    ) -> i32;
    pub fn GetForegroundWindow() -> *mut c_void;
    pub fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
    // Timers
    pub fn SetTimer(hwnd: *mut c_void, id: usize, ms: u32, proc: *const c_void) -> usize;
    pub fn KillTimer(hwnd: *mut c_void, id: usize) -> i32;
    // Hotkeys globales (registrados sobre la ventana de control).
    pub fn RegisterHotKey(hwnd: *mut c_void, id: i32, fsModifiers: u32, vk: u32) -> i32;
    pub fn UnregisterHotKey(hwnd: *mut c_void, id: i32) -> i32;
    // DPI
    pub fn SetProcessDpiAwarenessContext(context: isize) -> i32;
    pub fn GetDpiForWindow(hwnd: *mut c_void) -> u32;
    // Monitores
    pub fn MonitorFromWindow(hwnd: *mut c_void, dwflags: u32) -> *mut c_void;
    pub fn MonitorFromPoint(pt: POINT, dwflags: u32) -> *mut c_void;
    pub fn GetMonitorInfoW(hmonitor: *mut c_void, lpmi: *mut MONITORINFO) -> i32;
}

pub const MONITOR_DEFAULTTONEAREST: u32 = 2;
pub const MONITORINFOF_PRIMARY: u32 = 1;

// ---------------------------------------------------------------------------
// ntdll: RtlGetVersion (no miente, a diferencia de GetVersionEx)
// ---------------------------------------------------------------------------
#[link(name = "ntdll")]
extern "system" {
    pub fn RtlGetVersion(os_version: *mut OSVERSIONINFOEXW) -> c_int;
}

// ---------------------------------------------------------------------------
// CfgMgr32: árbol PnP para hwids de monitores (fallback GPU vieja)
// ---------------------------------------------------------------------------
#[link(name = "cfgmgr32")]
extern "system" {
    pub fn CM_Locate_DevNodeW(
        flags: c_ulong,
        instance_id: *const u16,
        devinst: *mut c_ulong,
    ) -> c_ulong;
    pub fn CM_Get_Child(devinst: *mut c_ulong, index: c_ulong, child: *mut c_ulong) -> c_ulong;
    pub fn CM_Get_Sibling(devinst: *mut c_ulong, devinst_prev: c_ulong, devinst_next: *mut c_ulong) -> c_ulong;
    pub fn CM_Get_Device_IDW(
        devinst: c_ulong,
        id: *mut u16,
        size: c_ulong,
        flags: c_ulong,
    ) -> c_ulong;
}

// ---------------------------------------------------------------------------
// advapi32: registro + kernel32: objetos de sincronización + module
// (las funciones Reg* viven en advapi32.dll; con kernel32 no enlazan)
// ---------------------------------------------------------------------------
/// Pseudo-handle de HKLM (0xFFFFFFFF80000002 como signed).
pub const HKEY_LOCAL_MACHINE: i64 = -2_147_483_646;
/// Pseudo-handle de HKCU (0xFFFFFFFF80000001 como signed).
/// BUG 2026-10-21: antes era -2_147_483_645 (= 0xFFFFFFFF80000003 =
/// HKEY_USERS): autostart fallaba con ERROR_FILE_NOT_FOUND (abría
/// HKU\Software, inexistente) y gamebar.rs/traytip.rs creaban/leían
/// claves en el hive equivocado en silencio.
pub const HKEY_CURRENT_USER: i64 = -2_147_483_647;

pub const KEY_READ: u32 = 0x20019;
pub const KEY_SET_VALUE: u32 = 0x2;
pub const KEY_WRITE: u32 = 0x20006;
pub const REG_SZ: u32 = 1;
pub const REG_DWORD: u32 = 4;
pub const ERROR_SUCCESS: i32 = 0;
pub const ERROR_FILE_NOT_FOUND: i32 = 2;

#[link(name = "advapi32")]
extern "system" {
    pub fn RegOpenKeyExW(
        key: i64,
        subkey: *const u16,
        options: u32,
        access: u32,
        result: *mut i64,
    ) -> c_int;
    pub fn RegCloseKey(key: i64) -> c_int;
    pub fn RegCreateKeyExW(
        hkey: i64,
        lpsubkey: *const u16,
        reserved: u32,
        lpclass: *const u16,
        options: u32,
        samdesired: u32,
        lpsecurityattributes: *mut c_void,
        phkresult: *mut i64,
        lpdwdisposition: *mut u32,
    ) -> c_int;
    pub fn RegEnumKeyExW(
        key: i64,
        index: u32,
        name: *mut u16,
        name_len: *mut u32,
        reserved: *mut u32,
        class: *mut u16,
        class_len: *mut u32,
        types: *mut u32,
        last_write: *mut FILETIME,
    ) -> c_int;
    pub fn RegQueryValueExW(
        key: i64,
        name: *const u16,
        reserved: *mut u32,
        typ: *mut u32,
        data: *mut u8,
        data_len: *mut u32,
    ) -> c_int;
    pub fn RegSetValueExW(
        key: i64,
        name: *const u16,
        reserved: u32,
        typ: u32,
        data: *const u8,
        cbdata: u32,
    ) -> c_int;
    pub fn RegDeleteValueW(key: i64, name: *const u16) -> c_int;
    /// Layout real de 12 argumentos (lpReserved en posición 4,
    /// lpcSecurityDescriptorLength penúltima, lpftLastWriteTime al final);
    /// validado en Windows 11 x64 (con 11 args la llamada es UB).
    pub fn RegQueryInfoKeyW(
        key: i64,
        class: *mut u16,
        class_len: *mut u32,
        reserved: *mut u32,
        subkeys: *mut u32,
        max_subkey: *mut u32,
        max_class: *mut u32,
        max_value: *mut u32,
        max_value_name: *mut u32,
        max_value_data: *mut u32,
        security_desc_len: *mut u32,
        last_write: *mut FILETIME,
    ) -> c_int;
}

#[link(name = "kernel32")]
extern "system" {
    // Objetos de sincronización (single instance)
    pub fn CreateMutexW(sa: *mut c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
    pub fn CreateEventW(sa: *mut c_void, manual_reset: i32, initial_state: i32, name: *const u16) -> *mut c_void;
    pub fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    pub fn SetEvent(h: *mut c_void) -> i32;
    pub fn ResetEvent(h: *mut c_void) -> i32;
    pub fn WaitForSingleObject(h: *mut c_void, ms: u32) -> u32;
    pub fn CloseHandle(h: *mut c_void) -> i32;
}

// ---------------------------------------------------------------------------
// gdi32
// ---------------------------------------------------------------------------
// Nota: el dibujo NO usa GDI+ (en este SO la gdiplus.dll es un stub que
// no renderiza). GDI se usa solo para rasterizar texto con antialiasing
// en un DIB scratch; las formas se componen por píxel en `draw.rs`.
#[link(name = "gdi32")]
extern "system" {
    pub fn CreateCompatibleDC(dc: *mut c_void) -> *mut c_void;
    pub fn DeleteDC(dc: *mut c_void) -> i32;
    pub fn SetBkMode(dc: *mut c_void, mode: i32) -> i32;
    pub fn SetBkColor(dc: *mut c_void, color: u32) -> u32;
    pub fn SetTextColor(dc: *mut c_void, color: u32) -> u32;
    pub fn CreateFontW(
        height: i32,
        width: i32,
        escapement: i32,
        orientation: i32,
        weight: i32,
        italic: i32,
        underline: i32,
        strikeout: i32,
        charset: i32,
        outprecision: i32,
        clipprecision: i32,
        quality: i32,
        pitchandfamily: i32,
        name: *const u16,
    ) -> *mut c_void;
    pub fn DrawTextW(
        hdc: *mut c_void,
        lpchText: *const u16,
        cchText: i32,
        lprc: *mut RECT,
        fmt: u32,
    ) -> i32;
    pub fn GetTextExtentPoint32W(
        hdc: *mut c_void,
        lpString: *const u16,
        cchString: i32,
        lpsize: *mut SIZE,
    ) -> i32;
    pub fn CreateDIBSection(
        dc: *mut c_void,
        pbitmapinfo: *const BITMAPINFO,
        usage: u32,
        bits: *mut *mut u8,
        handle: *mut c_void,
        offset: u32,
    ) -> *mut c_void;
    pub fn CreateBitmap(
        width: i32,
        height: i32,
        planes: u32,
        bitcount: u32,
        bits: *const c_void,
    ) -> *mut c_void;
    pub fn DeleteObject(obj: *mut c_void) -> i32;
    pub fn SelectObject(dc: *mut c_void, obj: *mut c_void) -> *mut c_void;
    pub fn CreateIconIndirect(info: *const ICONINFO) -> *mut c_void;
    pub fn DestroyIcon(icon: *mut c_void) -> i32;
    // Regiones (no usadas en vivo: la forma viene del alpha del ULW)
    pub fn CreateRectRgn(x1: i32, y1: i32, x2: i32, y2: i32) -> *mut c_void;
    pub fn CreatePolygonRgn(pnts: *const POINT, n: i32, mode: i32) -> *mut c_void;
    pub fn CombineRgn(
        dst: *mut c_void,
        src1: *mut c_void,
        src2: *mut c_void,
        mode: i32,
    ) -> i32;
}

// ---------------------------------------------------------------------------
// shell32: bandeja de sistema
// ---------------------------------------------------------------------------
#[link(name = "shell32")]
extern "system" {
    pub fn Shell_NotifyIconW(cmd: u32, data: *const NOTIFYICONDATAW) -> i32;
    /// Rect del icono en coords de pantalla; HRESULT (S_OK=0).
    /// Devuelve error si el icono no es visible (área de desborde).
    pub fn Shell_NotifyIconGetRect(nii: *const NOTIFYICONIDENTIFIER, rect: *mut RECT) -> i32;
}


// ---------------------------------------------------------------------------
// Helpers de texto UTF-16
// ---------------------------------------------------------------------------

/// Convierte `&str` a buffer UTF-16 con null terminador.
pub fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Lee un buffer de wchar (hasta el null) a `String`.
pub fn from_wchar(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
