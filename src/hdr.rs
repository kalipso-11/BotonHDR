//! Detección y cambio de estado HDR/SDR del monitor activo.
//!
//! 1. Cambio (camino principal): API de DisplayConfig de user32
//!    (`DisplayConfigSetDeviceInfo`): `SET_HDR_STATE` en build >= 26100
//!    (Windows 11 24H2) y `SET_ADVANCED_COLOR_STATE` en builds anteriores.
//!    No pasa por el atajo global → no dispara el aviso de la Game Bar.
//! 2. Cambio (fallback): si no se identifica el modo o la API rechaza,
//!    se simula `Win+Alt+B` como último recurso.
//! 3. Estado: modo activo en DisplayConfig (`GET_ADVANCED_COLOR_INFO_2`
//!    24H2+ / legacy) y, si no, el registro
//!    `HKLM\SYSTEM\...\GraphicsDrivers\MonitorDataStore\{clave}\HDREnabled`.
//! 4. Verificación: tras el cambio se relee el MonitorDataStore unos
//!    segundos y, si no confirma, re-consulta final de DisplayConfig
//!    (los índices de modo se reasignan tras el toggle).

use crate::win::{
    from_wchar, utf16, DCGetAdvColorInfo, DCGetAdvColorInfo2, DCModeInfo, DCSetColorState,
    DCTargetName, DISPLAY_DEVICEW, INPUT, INPUT_0, KEYBDINPUT,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const SDR: &str = "SDR";
pub const HDR: &str = "HDR";

const MDS_BASE: &str = r"SYSTEM\CurrentControlSet\Control\GraphicsDrivers\MonitorDataStore";
const NOEDID_KEY: &str = "NOEDID";
const VERIFY_SECONDS: f64 = 7.0;

// Flags de StateFlags de EnumDisplayDevicesW.
const EDD_ACTIVE: u32 = 0x00000001;
const EDD_PRIMARY: u32 = 0x00000002;

// ---------------------------------------------------------------------------
// API DisplayConfig (user32): constantes
// ---------------------------------------------------------------------------
const QDC_ONLY_ACTIVE_PATHS: u32 = 2;
const PATH_INFO_SIZE: usize = 72; // DISPLAYCONFIG_PATH_INFO
const MODE_INFO_SIZE: usize = 64; // DISPLAYCONFIG_MODE_INFO
const TARGET_NAME_TYPE: u32 = 2; // GET_TARGET_NAME
const ADV_INFO_TYPE: u32 = 9; // GET_ADVANCED_COLOR_INFO (legacy)
const SET_ADV_STATE_TYPE: u32 = 10; // SET_ADVANCED_COLOR_STATE
const ADV_INFO_2_TYPE: u32 = 15; // GET_ADVANCED_COLOR_INFO_2 (24H2+)
const SET_HDR_STATE_TYPE: u32 = 16; // SET_HDR_STATE (24H2+)
const BUILD_24H2: u32 = 26100;

// Bits del campo ``value`` de GET_ADVANCED_COLOR_INFO_2.
const AC2_HDR_SUPPORTED: u32 = 1 << 4; // highDynamicRangeSupported
// Bits del campo ``value`` de GET_ADVANCED_COLOR_INFO (legacy).
const AC_SUPPORTED: u32 = 1 << 0;
const AC_ENABLED: u32 = 1 << 1;
// Valores de ``activeColorMode``.
const ACM_HDR: u32 = 2;

// ---------------------------------------------------------------------------
// Build de Windows (RtlGetVersion de ntdll: no miente como GetVersionEx)
// ---------------------------------------------------------------------------
static BUILD: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
static IS_24H2: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn build_number() -> u32 {
    *BUILD.get_or_init(|| unsafe {
        let mut osv: crate::win::OSVERSIONINFOEXW = core::mem::zeroed();
        osv.dwOSVersionInfoSize = core::mem::size_of::<crate::win::OSVERSIONINFOEXW>() as u32;
        if crate::win::RtlGetVersion(&mut osv) != 0 {
            0
        } else {
            osv.dwBuildNumber
        }
    })
}

fn is_24h2() -> bool {
    *IS_24H2.get_or_init(|| build_number() >= BUILD_24H2)
}

// ---------------------------------------------------------------------------
// API DisplayConfig: enumeración, estado y cambio
// ---------------------------------------------------------------------------
/// Consulta los paths y modos activos: `(índices de modo por path, modos)`.
fn dc_query() -> Option<(Vec<u32>, Vec<DCModeInfo>)> {
    unsafe {
        let mut paths_n: u32 = 0;
        let mut modes_n: u32 = 0;
        if crate::win::GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths_n, &mut modes_n)
            != 0
        {
            return None;
        }
        if paths_n == 0 || modes_n == 0 {
            return None;
        }
        let mut paths_buf = vec![0u8; (paths_n as usize) * PATH_INFO_SIZE];
        let mut modes_buf = vec![0u8; (modes_n as usize) * MODE_INFO_SIZE];
        if crate::win::QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut paths_n,
            paths_buf.as_mut_ptr().cast(),
            &mut modes_n,
            modes_buf.as_mut_ptr().cast(),
            0,
        ) != 0
        {
            return None;
        }
        // Dentro de DISPLAYCONFIG_PATH_INFO: SourceInfo (20 B) + TargetInfo;
        // TargetInfo = AdapterId (8 B) + ID (4 B) + ModeInfoIdx (4 B) →
        // el índice del modo objetivo queda en el offset 32.
        let mut paths = Vec::with_capacity(paths_n as usize);
        for i in 0..paths_n as usize {
            let off = i * PATH_INFO_SIZE + 32;
            let idx = u32::from_le_bytes([
                paths_buf[off],
                paths_buf[off + 1],
                paths_buf[off + 2],
                paths_buf[off + 3],
            ]);
            paths.push(idx);
        }
        let mut modes = Vec::with_capacity(modes_n as usize);
        for i in 0..modes_n as usize {
            let off = i * MODE_INFO_SIZE;
            let m = core::ptr::read_unaligned(modes_buf[off..].as_ptr().cast::<DCModeInfo>());
            modes.push(m);
        }
        Some((paths, modes))
    }
}

/// Nombre del target de un modo (`None` en modos huérfanos).
fn target_name(mode: &DCModeInfo) -> Option<DCTargetName> {
    unsafe {
        let mut tn: DCTargetName = core::mem::zeroed();
        tn.header.r#type = TARGET_NAME_TYPE;
        tn.header.size = core::mem::size_of::<DCTargetName>() as u32;
        tn.header.adapterId = mode.adapterId;
        tn.header.id = mode.id;
        if crate::win::DisplayConfigGetDeviceInfo(&mut tn as *mut DCTargetName as *mut core::ffi::c_void) != 0
        {
            return None;
        }
        Some(tn)
    }
}

/// Modo de DisplayConfig del monitor activo (o `None`).
fn active_mode() -> Option<DCModeInfo> {
    let q = dc_query()?;
    let (paths, modes) = q;
    let mut active: Vec<u32> = Vec::new();
    for &idx in &paths {
        if (idx as usize) < modes.len() && !active.contains(&idx) {
            active.push(idx);
        }
    }
    if active.is_empty() {
        return None;
    }
    if active.len() == 1 {
        let m = &modes[active[0] as usize];
        return if target_name(m).is_some() { Some(*m) } else { None };
    }
    let hwid = active_hwid()?;
    let hwid_l = hwid.to_lowercase();
    for &idx in &active {
        let tn = target_name(&modes[idx as usize])?;
        let path = from_wchar(&tn.devicePath);
        let tokens: Vec<&str> = path.split('#').collect();
        if tokens.len() >= 2 && tokens[1].to_lowercase() == hwid_l {
            return Some(modes[idx as usize]);
        }
    }
    for &idx in &active {
        let Some(tn) = target_name(&modes[idx as usize]) else {
            continue;
        };
        let friendly = from_wchar(&tn.friendlyName).to_lowercase();
        if friendly.contains(&hwid_l) {
            return Some(modes[idx as usize]);
        }
    }
    None
}

/// `("HDR"|"SDR"|None, soporta_HDR)` de un modo.
fn dc_mode_state(mode: &DCModeInfo) -> (Option<&'static str>, bool) {
    if is_24h2() {
        let gi = {
            let mut gi: DCGetAdvColorInfo2 = unsafe { core::mem::zeroed() };
            gi.header.r#type = ADV_INFO_2_TYPE;
            gi.header.size = core::mem::size_of::<DCGetAdvColorInfo2>() as u32;
            gi.header.adapterId = mode.adapterId;
            gi.header.id = mode.id;
            if unsafe {
                crate::win::DisplayConfigGetDeviceInfo(
                    &mut gi as *mut DCGetAdvColorInfo2 as *mut core::ffi::c_void,
                )
            } != 0
            {
                None
            } else {
                Some(gi)
            }
        };
        if let Some(gi) = gi {
            return (
                if gi.activeColorMode == ACM_HDR { Some(HDR) } else { Some(SDR) },
                (gi.value & AC2_HDR_SUPPORTED) != 0,
            );
        }
    }
    let gi = {
        let mut gi: DCGetAdvColorInfo = unsafe { core::mem::zeroed() };
        gi.header.r#type = ADV_INFO_TYPE;
        gi.header.size = core::mem::size_of::<DCGetAdvColorInfo>() as u32;
        gi.header.adapterId = mode.adapterId;
        gi.header.id = mode.id;
        if unsafe {
            crate::win::DisplayConfigGetDeviceInfo(
                &mut gi as *mut DCGetAdvColorInfo as *mut core::ffi::c_void,
            )
        } != 0
        {
            None
        } else {
            Some(gi)
        }
    };
    if let Some(gi) = gi {
        return (
            if (gi.value & AC_ENABLED) != 0 { Some(HDR) } else { Some(SDR) },
            (gi.value & AC_SUPPORTED) != 0,
        );
    }
    (None, false)
}

/// Pone HDR/SDR a un modo vía la API de DisplayConfig.
fn dc_set_state(mode: &DCModeInfo, target: &str) -> bool {
    unsafe {
        let mut s: DCSetColorState = core::mem::zeroed();
        s.header.r#type = if is_24h2() { SET_HDR_STATE_TYPE } else { SET_ADV_STATE_TYPE };
        s.header.size = core::mem::size_of::<DCSetColorState>() as u32;
        s.header.adapterId = mode.adapterId;
        s.header.id = mode.id;
        s.value = if target == HDR { 1 } else { 0 };
        crate::win::DisplayConfigSetDeviceInfo(&mut s as *mut DCSetColorState as *mut core::ffi::c_void)
            == 0
    }
}

// ---------------------------------------------------------------------------
// Tienda de datos de monitores (MonitorDataStore) — fallback de estado
// ---------------------------------------------------------------------------

/// LastWriteTime de una clave MDS en ticks de FILETIME (0 si falla).
///
/// En Windows 11 10.0.26xxx hay que abrir la clave con KEY_READ (0x20019);
/// con 0x20000 la consulta da ERROR_ACCESS_DENIED.
fn key_lastwrite(name: &str) -> u128 {
    let path = utf16(&format!("{MDS_BASE}\\{name}"));
    unsafe {
        let mut h: i64 = 0;
        if crate::win::RegOpenKeyExW(
            crate::win::HKEY_LOCAL_MACHINE,
            path.as_ptr(),
            0,
            crate::win::KEY_READ,
            &mut h,
        ) != crate::win::ERROR_SUCCESS
        {
            return 0;
        }
        let mut ft: crate::win::FILETIME = core::mem::zeroed();
        // 12 args reales: hKey, lpClass,
        // 9×LPDWORD (class_len..security_desc_len), PFILETIME.
        let r = crate::win::RegQueryInfoKeyW(
            h,
            std::ptr::null_mut(), // lpClass
            std::ptr::null_mut(), // lpcchClass
            std::ptr::null_mut(), // lpReserved
            std::ptr::null_mut(), // lpcSubKeys
            std::ptr::null_mut(), // lpcchMaxSubKey
            std::ptr::null_mut(), // lpcchMaxClass
            std::ptr::null_mut(), // lpcValues
            std::ptr::null_mut(), // lpcchMaxValue
            std::ptr::null_mut(), // lpcchMaxValueName
            std::ptr::null_mut(), // lpcbMaxValueData
            &mut ft,              // lpftLastWriteTime
        );
        let _ = crate::win::RegCloseKey(h);
        if r != crate::win::ERROR_SUCCESS {
            return 0;
        }
        ((ft.dwHighDateTime as u128) << 32) | (ft.dwLowDateTime as u128)
    }
}

/// `{clave MDS: HDREnabled (0/1) o None si el valor no existe}`.
fn mds_states() -> HashMap<String, Option<i32>> {
    let mut out: HashMap<String, Option<i32>> = HashMap::new();
    let base = utf16(MDS_BASE);
    unsafe {
        let mut h: i64 = 0;
        if crate::win::RegOpenKeyExW(
            crate::win::HKEY_LOCAL_MACHINE,
            base.as_ptr(),
            0,
            crate::win::KEY_READ,
            &mut h,
        ) != crate::win::ERROR_SUCCESS
        {
            return out;
        }
        let mut i: u32 = 0;
        loop {
            let mut name_buf = [0u16; 512];
            let mut name_len = name_buf.len() as u32;
            let r = crate::win::RegEnumKeyExW(
                h,
                i,
                name_buf.as_mut_ptr(),
                &mut name_len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if r != crate::win::ERROR_SUCCESS {
                break;
            }
            i += 1;
            let name = from_wchar(&name_buf);
            let sub = utf16(&format!("{MDS_BASE}\\{name}"));
            let mut sk: i64 = 0;
            if crate::win::RegOpenKeyExW(
                crate::win::HKEY_LOCAL_MACHINE,
                sub.as_ptr(),
                0,
                crate::win::KEY_READ,
                &mut sk,
            ) != crate::win::ERROR_SUCCESS
            {
                continue;
            }
            let val = utf16("HDREnabled");
            let mut typ: u32 = 0;
            let mut data: u32 = 0;
            let mut size: u32 = 4;
            let ok = crate::win::RegQueryValueExW(
                sk,
                val.as_ptr(),
                std::ptr::null_mut(),
                &mut typ,
                &mut data as *mut u32 as *mut u8,
                &mut size,
            ) == crate::win::ERROR_SUCCESS
                && typ == crate::win::REG_DWORD;
            let _ = crate::win::RegCloseKey(sk);
            out.insert(name, if ok { Some(data as i32) } else { None });
        }
        let _ = crate::win::RegCloseKey(h);
    }
    out
}

/// Claves MDS cuyo nombre empieza por el hwid del monitor.
fn candidates(hwid: &str, mds: &HashMap<String, Option<i32>>) -> Vec<String> {
    mds.keys()
        .filter(|k| k.as_str() == hwid || k.starts_with(hwid))
        .cloned()
        .collect()
}

/// Clave MDS del monitor activo (desambiguada por LastWriteTime).
fn active_mds_key(hwid: Option<&str>, mds: &HashMap<String, Option<i32>>) -> Option<String> {
    let hwid = hwid?;
    let cands = candidates(hwid, mds);
    if cands.is_empty() {
        // Sin EDID (clave NOEDID) o monitor sin clave todavía.
        return mds.contains_key(NOEDID_KEY).then_some(NOEDID_KEY.to_string());
    }
    if cands.len() == 1 {
        return Some(cands.into_iter().next().unwrap());
    }
    // Varios monitores con el mismo hwid: clave con LastWriteTime más
    // reciente; empate → la que trae el valor, luego alfabética:
    // `max(cands)` por (lastwrite, trae valor, clave).
    cands.into_iter().max_by(|a, b| {
        let sa = (
            key_lastwrite(a),
            if mds.get(a).is_some_and(|v| v.is_some()) { 1 } else { 0 },
        );
        let sb = (
            key_lastwrite(b),
            if mds.get(b).is_some_and(|v| v.is_some()) { 1 } else { 0 },
        );
        sa.cmp(&sb).then_with(|| a.cmp(b))
    })
}

// ---------------------------------------------------------------------------
// Monitor activo (EnumDisplayDevicesW)
// ---------------------------------------------------------------------------

/// hwids de los monitores (`DISPLAY\...`) hijos de una GPU (PnP).
fn gpu_monitor_hwids(gpu_id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let gpu = utf16(gpu_id);
    unsafe {
        let mut dev: u32 = 0;
        if crate::win::CM_Locate_DevNodeW(2, gpu.as_ptr(), &mut dev) != 0 {
            return out;
        }
        let mut child: u32 = 0;
        if crate::win::CM_Get_Child(&mut dev, 0, &mut child) != 0 {
            return out;
        }
        let mut buf = [0u16; 256];
        loop {
            let n = buf.len() as u32;
            if crate::win::CM_Get_Device_IDW(child, buf.as_mut_ptr(), n, 0) == 0 {
                let did = from_wchar(&buf);
                if let Some(hwid) = did
                    .strip_prefix("DISPLAY\\")
                    .and_then(|rest| rest.split('\\').next())
                    .filter(|h| !h.is_empty())
                {
                    out.push(hwid.to_string());
                }
            }
            if crate::win::CM_Get_Sibling(&mut dev, child, &mut child) != 0 {
                break;
            }
        }
    }
    out
}

/// `[(hwid, StateFlags)]` de los monitores visibles.
fn monitor_devices() -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();
    unsafe {
        for n in 1..9 {
            let display = utf16(&format!(r"\\.\DISPLAY{n}"));
            for i in 0..8 {
                let mut d: DISPLAY_DEVICEW = core::mem::zeroed();
                d.cb = core::mem::size_of::<DISPLAY_DEVICEW>() as u32;
                if crate::win::EnumDisplayDevicesW(display.as_ptr(), i, &mut d, 0) == 0 {
                    break;
                }
                let dev_id = from_wchar(&d.DeviceID);
                if dev_id.is_empty() || !seen.insert(dev_id.clone()) {
                    continue;
                }
                if let Some(rest) = dev_id.strip_prefix("MONITOR\\") {
                    if let Some(hwid) = rest.split('\\').next() {
                        if !hwid.is_empty() && known.insert(hwid.to_string()) {
                            out.push((hwid.to_string(), d.StateFlags));
                        }
                    }
                } else if dev_id.starts_with("PCI\\") {
                    for hwid in gpu_monitor_hwids(&dev_id) {
                        if known.insert(hwid.clone()) {
                            out.push((hwid, EDD_ACTIVE | EDD_PRIMARY));
                        }
                    }
                }
            }
        }
    }
    out
}

/// ID estable de un monitor por su nombre GDI (`\\.\DISPLAYn`): el
/// DeviceID PnP completo (`MONITOR\{hwid}\{guid}\{inst}`) si se resuelve
/// — único por panel físico aunque haya dos paneles del mismo modelo —;
/// si no, el propio nombre GDI (pantallas virtuales). Lo usa la memoria
/// por monitor del gadget.
pub fn monitor_id_for_device(device: &str) -> String {
    let mut found = String::new();
    let dev_ci = device.to_ascii_lowercase();
    unsafe {
        for n in 1..9 {
            let display = utf16(&format!(r"\\.\DISPLAY{n}"));
            for i in 0..8 {
                let mut d: DISPLAY_DEVICEW = core::mem::zeroed();
                d.cb = core::mem::size_of::<DISPLAY_DEVICEW>() as u32;
                if crate::win::EnumDisplayDevicesW(display.as_ptr(), i, &mut d, 0) == 0 {
                    break;
                }
                // El nombre puede ser el GDI puro (`\\.\DISPLAY1`) o con
                // sufijo de monitor (`\\.\DISPLAY1\Monitor0`): se hace match
                // por prefix (con `\\` para no chocar DISPLAY1/DISPLAY10).
                let name = from_wchar(&d.DeviceName).to_ascii_lowercase();
                if name != dev_ci && !name.starts_with(&format!("{}\\", dev_ci)) {
                    continue;
                }
                let dev_id = from_wchar(&d.DeviceID);
                if dev_id.starts_with("MONITOR\\") {
                    found = dev_id;
                    break;
                }
            }
            if !found.is_empty() {
                break;
            }
        }
    }
    if found.is_empty() {
        device.to_string()
    } else {
        found
    }
}

/// Hwid PnP del monitor activo.
fn active_hwid() -> Option<String> {
    let devs = monitor_devices();
    if devs.is_empty() {
        return None;
    }
    for (hwid, flags) in &devs {
        if (flags & EDD_ACTIVE) != 0 && (flags & EDD_PRIMARY) != 0 {
            return Some(hwid.clone());
        }
    }
    for (hwid, flags) in &devs {
        if (flags & EDD_ACTIVE) != 0 {
            return Some(hwid.clone());
        }
    }
    Some(devs[0].0.clone())
}

// ---------------------------------------------------------------------------
// Último recurso: Win+Alt+B
// ---------------------------------------------------------------------------
fn send_win_alt_b() {
    unsafe {
        let mk = |vk: u16, up: bool| -> INPUT {
            INPUT {
                r#type: 1, // INPUT_KEYBOARD
                u: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: crate::win::MapVirtualKeyW(vk as u32, 0) as u16,
                        dwFlags: if up { 0x0002 } else { 0 },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }
        };
        const VK_LWIN: u16 = 0x5B;
        const VK_MENU: u16 = 0x12;
        const VK_B: u16 = 0x42;
        let seq = [
            mk(VK_LWIN, false),
            mk(VK_MENU, false),
            mk(VK_B, false),
            mk(VK_B, true),
            mk(VK_MENU, true),
            mk(VK_LWIN, true),
        ];
        let _ = crate::win::SendInput(
            seq.len() as u32,
            seq.as_ptr(),
            core::mem::size_of::<INPUT>() as i32,
        );
    }
}

// ---------------------------------------------------------------------------
// API pública
// ---------------------------------------------------------------------------

/// Estado actual del monitor activo: `(estado, fuente)`.
pub fn get_state() -> (Option<&'static str>, Option<String>) {
    if let Some(mode) = active_mode() {
        let (state, _supported) = dc_mode_state(&mode);
        if let Some(state) = state {
            return (Some(state), Some(format!("DisplayConfig modo id={}", mode.id)));
        }
    }
    let mds = mds_states();
    if mds.is_empty() {
        return (None, None);
    }
    let hwid = match active_hwid() {
        Some(h) => h,
        None => return (None, None),
    };
    let key = match active_mds_key(Some(&hwid), &mds) {
        Some(k) => k,
        None => return (None, None),
    };
    (
        if mds.get(&key).copied().flatten().unwrap_or(0) == 1 {
            Some(HDR)
        } else {
            Some(SDR)
        },
        Some(format!("{MDS_BASE}\\{key}")),
    )
}

/// Poll del registro MDS hasta que el monitor activo quede en `target`.
fn poll(target: &str, hwid: Option<&str>, before: &HashMap<String, Option<i32>>, deadline: Instant) -> bool {
    let want = if target == HDR { 1 } else { 0 };
    loop {
        if Instant::now() >= deadline {
            return false;
        }
        let mds = mds_states();
        let key = hwid.and_then(|h| active_mds_key(Some(h), &mds));
        if let Some(ref k) = key {
            if mds.get(k).copied().flatten() == Some(want) {
                return true;
            }
        }
        // Fallback: si la unidad activa resultó ser otra clave con el mismo
        // hwid, aceptamos que una candidata haya CAMBIADO al estado objetivo.
        for (c, prev) in before.iter() {
            let now = mds.get(c).copied().flatten();
            if now == Some(want) && *prev != Some(want) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Pasa el monitor activo a `target` ("HDR" o "SDR").
///
/// Devuelve `true` si el cambio quedó confirmado. Si el driver indica que
/// el monitor no soporta HDR, devuelve `false` inmediato.
pub fn set_state(target: &str) -> bool {
    if target != HDR && target != SDR {
        return false;
    }
    // Logs de fase (solo corren durante un toggle; si el worker no
    // devuelve, la última fase dice dónde se quedó).
    crate::slog(60, &format!("hdr_set target={}", target));
    let mds = mds_states();
    let hwid = active_hwid();
    let before: HashMap<String, Option<i32>> = hwid
        .as_ref()
        .map(|h| candidates(h, &mds))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| mds.get(&c).map(|v| (c.clone(), *v)))
        .collect();
    let key = hwid.as_ref().and_then(|h| active_mds_key(Some(h), &mds));
    let mut cur = key
        .as_ref()
        .map(|k| if mds.get(k).copied().flatten().unwrap_or(0) == 1 { HDR } else { SDR })
        .filter(|_| key.is_some())
        .unwrap_or(SDR);
    if key.is_none() {
        cur = SDR;
    }

    if let Some(mode) = active_mode() {
        let (state, supported) = dc_mode_state(&mode);
        if let Some(state) = state {
            cur = state;
        }
        if cur != target {
            // El driver indica que este monitor no soporta HDR.
            if target == HDR && state.is_some() && !supported {
                crate::slog(62, "hdr_unsupported");
                return false;
            }
            crate::slog(63, "hdr_dc_set_call");
            if dc_set_state(&mode, target) {
                crate::slog(64, "hdr_dc_set_accepted");
                // Verificación: poll del MDS; si no confirma, re-consulta
                // final de DC (los índices de modo se reasignan).
                if poll(target, hwid.as_deref(), &before, Instant::now() + Duration::from_secs_f64(VERIFY_SECONDS))
                {
                    crate::slog(65, "hdr_poll_ok");
                    return true;
                }
                crate::slog(66, "hdr_poll_timeout_requery");
                if let Some(fresh) = active_mode() {
                    let (st2, _s2) = dc_mode_state(&fresh);
                    if st2 == Some(target) {
                        crate::slog(67, "hdr_requery_ok");
                        return true;
                    }
                }
                crate::slog(68, "hdr_unconfirmed_false");
                return false;
            }
            // La API rechazó el cambio: se intenta el último recurso.
            crate::slog(69, "hdr_dc_set_rejected");
        }
    } else {
        crate::slog(70, "hdr_no_dc_mode");
    }
    if cur == target {
        crate::slog(71, "hdr_already_target");
        return true;
    }
    // Último recurso: atajo global Win+Alt+B (puede dar el toast).
    crate::slog(72, "hdr_win_alt_b");
    send_win_alt_b();
    let ok = poll(
        target,
        hwid.as_deref(),
        &before,
        Instant::now() + Duration::from_secs_f64(VERIFY_SECONDS),
    );
    crate::slog(73, &format!("hdr_altb_done ok={}", ok));
    ok
}

/// Cambio rápido SIN polling de confirmación (lo usa el sentinela del
/// bloqueo de estado, que corre en su propio hilo): re-consulta el modo
/// activo (los IDs de modo se reasignan tras cada toggle) y llama
/// `DisplayConfigSetDeviceInfo` directo. `true` si la API aceptó el
/// cambio o el modo ya estaba en `target`.
pub fn set_state_fast(target: &str) -> bool {
    if target != HDR && target != SDR {
        return false;
    }
    let Some(mode) = active_mode() else {
        return false;
    };
    let (state, supported) = dc_mode_state(&mode);
    if state == Some(target) {
        return true;
    }
    if target == HDR && state.is_some() && !supported {
        return false;
    }
    dc_set_state(&mode, target)
}

/// Alterna el estado; `true` si quedó confirmado.
pub fn toggle() -> bool {
    let (current, _) = get_state();
    let target = if current == Some(HDR) { SDR } else { HDR };
    set_state(target)
}
