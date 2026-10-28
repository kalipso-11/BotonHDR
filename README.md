# BotonHDR — HDR/SDR desktop toggle (Rust)

A desktop gadget in the background + a system tray icon that shows the **HDR/SDR**
state of the main monitor and lets you toggle it with a click.

<p align="center">
  <img src="https://github.com/kalipso-11/BotonHDR/raw/main/docs/captura.jpg" alt="BotonHDR: desktop gadget and tray icon" />
</p>

## Features
- **Desktop gadget**: a stylized octagon with a halo that shows `HDR` or `SDR`.
  - Always **behind the windows** (only visible when looking at the desktop).
  - **Draggable** to anywhere (the position is saved); it can be moved almost
    to the top of the screen.
  - **Per-monitor memory**: remembers its position on each monitor; when the
    primary monitor changes, the gadget moves back to where you left it, at
    that monitor's scale.
  - **Left click**: toggles HDR ↔ SDR.
  - **Hover**: hand cursor.
  - Sizes: **Small 80×100 / Medium 96×120 / Large 120×150 / Huge 160×200**.
  - Colors: cyan, green, yellow, orange, red, purple, white.
  - Opacity 20–100 % (true transparency, not darkening).
  - If the monitor does not support HDR, a small badge appears on top:
    **"HDR No compatible"**.
- **System tray**: a rounded-rectangle chip with the `HDR`/`SDR` text
  (slightly larger, easy to read).
  - **Left click**: toggles HDR ↔ SDR.
  - **Tooltip**: "HDR On" / "HDR Off" + "right click to configure".
  - **Right click**: menu with state, toggle, show/hide gadget, opacity, color,
    size, **Lock SDR/HDR**, **Language**, launch at Windows startup and quit.
- **Interface language**: English (default on new installs) or Spanish — pick
  it in the right-click menu → **Language**; applies instantly and is saved.
- **State lock**: "Lock SDR" (when in SDR) or "Lock HDR" (when in HDR) prevents
  the state from changing to anything else — from the app, the Windows shortcut
  or any other program. Visible padlock on the gadget and in the tray.
- **Global hotkey**: `Win + Alt + B` toggles HDR ↔ SDR (the same one Windows uses).
- When the state changes, the **Xbox Game Bar notification does not appear**
  (it is suppressed).
- The state refreshes every 2 s (if it changes elsewhere, the UI updates).
- Single instance: if it is already running, opening it again just shows the
  gadget.
- Preferences are stored in `%APPDATA%\BotonHDR2\config.json` (created on first
  run; includes `hdr_lock` for the lock).

## Usage
### Executable
Download the latest installer from https://github.com/kalipso-11/BotonHDR/releases.
Requirements: Windows 10/11 x64. No dependencies.

### Building from source
```
cargo build --release
```
Requires the Rust `x86_64-pc-windows-gnu` target. Note about the `mingw/` shim:
the local mingw toolchain (retcomm/cmake-clang) does not ship
`libgcc.a`/`libgcc_eh.a`, so `.cargo/config.toml` points to that directory
(with an **absolute** path — if you move the project, update it).

### Installer
A Windows installer (a single `.exe` built with Inno Setup) installs the
program to `C:\Program Files\BotonHDR`, creates a Start menu entry, and offers
an optional desktop shortcut plus a "run now" checkbox. The uninstaller is
registered in "Add or remove programs". Script: `installer/botonhdr.iss`
(requires Inno Setup 6 to compile); the built installer goes to
`installer/output/`.

## How the HDR toggle works
1. Reads the active monitor's state from the registry
   (`HKLM\SYSTEM\CCS\Control\GraphicsDrivers\MonitorDataStore\{key}\HDREnabled`,
   key = the monitor's PnP hwid; it is a mirror, never written to).
2. To change it: simulates the global shortcut **Win + Alt + B** (the native
   "Use Windows HD Color" toggle) and verifies the result.
3. If after ~7 s the state did not change → the monitor does not expose HDR →
   **"HDR No compatible"**.

> Note: on monitors without HDR the button shows SDR and, when trying to switch
> to HDR, shows the "HDR No compatible" notice. On HDR monitors the toggle works
> without administrator rights.

## Files (source)
| File | Description |
| --- | --- |
| `src/main.rs` | Entry point and orchestration |
| `src/win.rs` | All Win32 FFI (no crates: raw ABI, single source of truth) |
| `src/draw.rs` | SDF drawing: gadget, tray chip, padlock, warning pill |
| `src/gadget.rs` | Gadget window (background, drag, hover, opacity) |
| `src/tray.rs` | System tray and menu |
| `src/traytip.rs` | "HDR On/Off + right click to configure" tooltip |
| `src/hdr.rs` | HDR state detection/toggle (registry + Win+Alt+B) |
| `src/gamebar.rs` | Xbox Game Bar notification suppression |
| `src/config.rs` | Preferences persistence (JSON, serde) |
| `src/lang.rs` | Interface strings EN/ES (menu, colors, sizes, tooltip) |
| `src/autostart.rs` | "Launch at Windows startup" (HKCU Run) |
| `src/singleinstance.rs` | Single instance |
| `mingw/` | `libgcc.a`/`libgcc_eh.a` shim for the linker (see `.cargo/config.toml`) |

---

# BotonHDR — Botón HDR/SDR de escritorio (Rust)

Gadget de escritorio al fondo + icono en la bandeja de sistema que muestra el estado
**HDR/SDR** del monitor principal y permite alternarlo con un click.

## Funcionalidad
- **Gadget del escritorio**: octágono estilizado con halo que muestra `HDR` o `SDR`.
  - Siempre **detrás de las ventanas** (solo se ve mirando el escritorio).
  - **Arrastrable** a cualquier lado (la posición se guarda); se lo puede llevar
    casi hasta el tope de la pantalla.
  - **Memoria por monitor**: recuerda su posición en cada monitor; al cambiar
    de monitor primario vuelve a donde lo dejaste, con la escala de ese monitor.
  - **Click izquierdo**: alterna HDR ↔ SDR.
  - **Hover**: cursor de "manito".
  - Tamaños: **Chico 80×100 / Medio 96×120 / Grande 120×150 / Enorme 160×200**.
  - Colores: cian, verde, amarillo, naranja, rojo, violeta, blanco.
  - Opacidad 20–100 % (transparencia real, no oscurecimiento).
  - Si el monitor no soporta HDR, aparece un cartelito arriba:
    **"HDR No compatible"**.
- **Bandeja de sistema**: chip rectangular de bordes redondeados con el texto
  `HDR`/`SDR` (un poco grande, fácil de leer).
  - **Click izquierdo**: alterna HDR ↔ SDR.
  - **Tooltip**: "HDR On" / "HDR Off" + "click derecho para configurar".
  - **Click derecho**: menú con estado, cambiar, mostrar/ocultar gadget,
    opacidad, color, tamaño, **Bloquear SDR/HDR**, **Idioma**, iniciar con
    Windows y salir.
- **Idioma de la interfaz**: inglés (default en instalaciones nuevas) o
  español — se elige en el click derecho → **Idioma**; aplica al instante y
  persiste.
- **Bloqueo de estado**: "Bloquear SDR" (en SDR) o "Bloquear HDR" (en HDR)
  impide cambiar a cualquier otro estado — desde la app, el atajo de Windows o
  cualquier otro programa. Candado visible en el gadget y en la bandeja.
- **Atajo global**: `Win + Alt + B` alterna HDR ↔ SDR (el mismo que usa Windows).
- Al cambiar el estado **no aparece la notificación de Xbox Game Bar**
  (se suprime).
- El estado se refresca cada 2 s (si se cambia desde otro lado, la UI se actualiza).
- Solo una instancia a la vez: si ya está corriendo, al volver a abrirlo se
  muestra el gadget.
- Las preferencias se guardan en `%APPDATA%\BotonHDR2\config.json`
  (se crea sola al primer arranque; incluye `hdr_lock` para el bloqueo).

## Uso
### Ejecutable
Bajá el último instalador desde https://github.com/kalipso-11/BotonHDR/releases.
Requisitos: Windows 10/11 x64. Sin dependencias.

### Compilar desde fuente
```
cargo build --release
```
Requiere el target `x86_64-pc-windows-gnu` de Rust. Ojo con el shim de
`mingw/`: el toolchain mingw de la máquina (retcomm/cmake-clang) no trae
`libgcc.a`/`libgcc_eh.a`, por eso `.cargo/config.toml` apunta a ese directorio
(con ruta **absoluta** — si movés el proyecto, actualizala).

### Instalador
Un instalador de Windows (un único `.exe` hecho con Inno Setup) instala el
programa en `C:\Program Files\BotonHDR`, crea la entrada en el menú de inicio y
ofrece un acceso directo opcional en el escritorio y un checkbox "Ejecutar
ahora". El desinstalador queda registrado en "Agregar o quitar programas".
Script: `installer/botonhdr.iss` (requiere Inno Setup 6 para compilar); el
instalador compilado queda en `installer/output/`.

## Cómo alterna HDR
1. Lee el estado del monitor activo desde el registro
   (`HKLM\SYSTEM\CCS\Control\GraphicsDrivers\MonitorDataStore\{clave}\HDREnabled`,
   clave = hwid PnP del monitor; es un mirror, nunca se escribe).
2. Para cambiar: simula el atajo global **Win + Alt + B** (alternador nativo de
   "Usar Windows HD Color") y verifica el resultado.
3. Si tras ~7 s el estado no cambió → el monitor no expone HDR →
   **"HDR No compatible"**.

> Nota: en monitores sin HDR el botón muestra SDR y, al intentar pasar a HDR,
> muestra el aviso "HDR No compatible". En monitores HDR el alternado funciona
> sin necesidad de administrador.

## Archivos (fuente)
| Archivo | Descripción |
| --- | --- |
| `src/main.rs` | Punto de entrada y orquestación |
| `src/win.rs` | Toda la FFI Win32 (sin crates: ABI cruda, una sola fuente de verdad) |
| `src/draw.rs` | Dibujo por SDF: gadget, chip de bandeja, candado, pill de warning |
| `src/gadget.rs` | Ventana del gadget (fondo, drag, hover, opacidad) |
| `src/tray.rs` | Bandeja de sistema y menú |
| `src/traytip.rs` | Tooltip "HDR On/Off + click derecho para configurar" |
| `src/hdr.rs` | Detección/cambio de estado HDR (registro + Win+Alt+B) |
| `src/gamebar.rs` | Supresión de la notificación de Xbox Game Bar |
| `src/config.rs` | Persistencia de preferencias (JSON, serde) |
| `src/lang.rs` | Textos de la interfaz EN/ES (menú, colores, tamaños, tooltip) |
| `src/autostart.rs` | "Iniciar con Windows" (HKCU Run) |
| `src/singleinstance.rs` | Instancia única |
| `mingw/` | Shim de `libgcc.a`/`libgcc_eh.a` para el linker (ver `.cargo/config.toml`) |
