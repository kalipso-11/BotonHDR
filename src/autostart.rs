//! Opción 'Iniciar con Windows' mediante la clave Run del registro (HKCU):
//! valor "BotonHDR" = "ruta al exe".

use crate::win::{self, from_wchar, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "BotonHDR";

fn command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.to_string_lossy()))
}

pub fn is_enabled() -> bool {
    unsafe {
        let mut h: i64 = 0;
        if win::RegOpenKeyExW(
            HKEY_CURRENT_USER,
            win::utf16(RUN_KEY).as_ptr(),
            0,
            win::KEY_READ,
            &mut h,
        ) != win::ERROR_SUCCESS
        {
            return false;
        }
        let name = win::utf16(VALUE_NAME);
        let ok = win::RegQueryValueExW(
            h,
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) == win::ERROR_SUCCESS;
        let _ = win::RegCloseKey(h);
        ok
    }
}

pub fn set_enabled(enabled: bool) -> bool {
    unsafe {
        let mut h: i64 = 0;
        let key = win::utf16(RUN_KEY);
        let name = win::utf16(VALUE_NAME);
        if enabled {
            let open = win::RegOpenKeyExW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut h,
            );
            if open != win::ERROR_SUCCESS {
                crate::slog(83, &format!("autostart_on_open_fail gle={}", open));
                return false;
            }
            let cmd = match command() {
                Some(c) => c,
                None => {
                    let _ = win::RegCloseKey(h);
                    crate::slog(83, "autostart_on_noexe");
                    return false;
                }
            };
            let data = win::utf16(&cmd); // incluye null terminador
            let ok = win::RegSetValueExW(
                h,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            ) == win::ERROR_SUCCESS;
            let _ = win::RegCloseKey(h);
            if ok {
                crate::slog(81, &format!("autostart_on cmd={}", cmd));
            } else {
                crate::slog(
                    83,
                    &format!("autostart_on_set_fail gle={}", win::GetLastError()),
                );
            }
            ok
        } else {
            if win::RegOpenKeyExW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut h,
            ) != win::ERROR_SUCCESS
            {
                return false;
            }
            // Si no existe, borrarlo es inofensivo.
            let del = win::RegDeleteValueW(h, name.as_ptr());
            let _ = win::RegCloseKey(h);
            crate::slog(82, &format!("autostart_off gle={}", del));
            true
        }
    }
}

/// Utilidad para tests: imprime el valor guardado (no lo usa la app).
#[allow(dead_code)]
fn debug_value() -> Option<String> {
    unsafe {
        let mut h: i64 = 0;
        let key = win::utf16(RUN_KEY);
        let name = win::utf16(VALUE_NAME);
        if win::RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, win::KEY_READ, &mut h)
            != win::ERROR_SUCCESS
        {
            return None;
        }
        let mut buf = vec![0u8; 1024 * 8];
        let mut size = buf.len() as u32;
        let r = win::RegQueryValueExW(
            h,
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            &mut size,
        );
        let _ = win::RegCloseKey(h);
        if r != win::ERROR_SUCCESS {
            return None;
        }
        let wbuf: Vec<u16> = buf[..size as usize]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(from_wchar(&wbuf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip contra el HKCU real: escribir → verificar → restaurar.
    /// Regresión del bug "Iniciar con Windows" (2026-10-21): la constante
    /// `HKEY_CURRENT_USER` estaba en 0xFFFFFFFF80000003 (HKEY_USERS) y
    /// `RegOpenKeyExW` fallaba con ERROR_FILE_NOT_FOUND. Restaura el
    /// estado previo al terminar.
    #[test]
    fn run_value_roundtrip() {
        let before = is_enabled();
        assert!(set_enabled(true), "set_enabled(true) devolvió false");
        assert!(is_enabled(), "el valor no es visible tras escribirlo");
        assert!(set_enabled(before), "no se pudo restaurar el estado previo");
        assert_eq!(is_enabled(), before, "el estado previo no quedó restaurado");
    }
}
