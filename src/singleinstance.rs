//! Single instance vía mutex + evento nombrados de Win32 (FFI cruda de
//! kernel32, sin crate `windows`). Detección por ownership de la mutex
//! (WAIT_OBJECT_0 vs WAIT_TIMEOUT, sin depender de GetLastError) y evento
//! de manual reset "mostrar el gadget" para la segunda instancia.
//!
//! El nombre del objeto es fijo y estable entre builds: si dos instancias
//! corren a la vez comparten el single-instance (la segunda termina y
//! muestra el gadget de la primera).

use crate::win;

const MUTEX_NAME: &str = "Local\\BotonHDR2.instance";
const EVENT_NAME: &str = "Local\\BotonHDR2.show";

pub struct SingleInstance {
    pub is_first: bool,
    /// La mutex debe mantenerse poseída hasta el fin del proceso.
    _mutex: *mut core::ffi::c_void,
    event: *mut core::ffi::c_void,
}

impl SingleInstance {
    pub fn new() -> Self {
        unsafe {
            let name = win::utf16(MUTEX_NAME);
            let mutex = win::CreateMutexW(core::ptr::null_mut(), 1, name.as_ptr());
            // WAIT_OBJECT_0 (0) = la posee esta instancia → primera.
            // WAIT_TIMEOUT (258) = la posee otra → no es primera.
            let first = !mutex.is_null()
                && win::WaitForSingleObject(mutex, 0) == win::WAIT_OBJECT_0;

            let event = if first {
                let ename = win::utf16(EVENT_NAME);
                win::CreateEventW(core::ptr::null_mut(), 1, 0, ename.as_ptr())
            } else {
                // Segunda instancia: pedirle a la primera que muestre el gadget.
                let _ = win::CloseHandle(mutex);
                let ename = win::utf16(EVENT_NAME);
                let evt = win::OpenEventW(win::EVENT_MODIFY_STATE, 0, ename.as_ptr());
                if !evt.is_null() {
                    let _ = win::SetEvent(evt);
                    let _ = win::CloseHandle(evt);
                }
                core::ptr::null_mut() // la primera murió a medio gas: no señalamos
            };

            Self {
                is_first: first,
                _mutex: if first { mutex } else { core::ptr::null_mut() },
                event,
            }
        }
    }

    /// Consulta no bloqueante del evento "mostrar" (lo resetea si suena).
    pub fn take_show_signal(&self) -> bool {
        if !self.is_first || self.event.is_null() {
            return false;
        }
        unsafe {
            if win::WaitForSingleObject(self.event, 0) == win::WAIT_OBJECT_0 {
                let _ = win::ResetEvent(self.event);
                true
            } else {
                false
            }
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            if !self.event.is_null() {
                let _ = win::CloseHandle(self.event);
            }
            if !self._mutex.is_null() {
                let _ = win::CloseHandle(self._mutex);
            }
        }
    }
}
