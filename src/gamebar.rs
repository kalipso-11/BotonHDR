//! Silenciar el toast de notificación de HDR de Windows (best effort):
//! Enabled=0 en los dos AUMIDs Graphics.
//! CINTURÓN REDUNDANTE: el toggle normal ya no usa Win+Alt+B (API de
//! DisplayConfig); esto cubre el fallback y los toasts de Auto-HDR.

use crate::win::{self, HKEY_CURRENT_USER, KEY_SET_VALUE, KEY_WRITE};

const SETTINGS: &str = r"Software\Microsoft\Windows\CurrentVersion\Notifications\Settings";
const AUMIDS: [&str; 2] = [
    "Windows.SystemToast.Graphics.AutoHDR",
    "Windows.SystemToast.Graphics.HDR",
];

pub fn suppress_hdr_toasts() {
    unsafe {
        for aumid in AUMIDS {
            let path = win::utf16(&format!("{SETTINGS}\\{aumid}"));
            let enabled = win::utf16("Enabled");
            let mut h: i64 = 0;
            if win::RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_SET_VALUE, &mut h)
                != win::ERROR_SUCCESS
            {
                // Si el sender no existe, crear la clave vacía es inofensivo.
                if win::RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    path.as_ptr(),
                    0,
                    std::ptr::null(),
                    0,
                    KEY_WRITE,
                    std::ptr::null_mut(),
                    &mut h,
                    std::ptr::null_mut(),
                ) != win::ERROR_SUCCESS
                {
                    continue;
                }
            }
            let zero: u32 = 0;
            let _ = win::RegSetValueExW(
                h,
                enabled.as_ptr(),
                0,
                win::REG_DWORD,
                zero.to_le_bytes().as_ptr(),
                4,
            );
            let _ = win::RegCloseKey(h);
        }
    }
}
