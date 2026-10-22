//! Persistencia de preferencias en %APPDATA%\BotonHDR2\config.json
//! (keys estables, migraciones one-shot v2 y v3, tolerancia a BOM).

use serde::Serialize;
use std::path::PathBuf;

const APP_NAME: &str = "BotonHDR2";
const MIGRATION_FLAG: &str = "migrated_v2";
const MIGRATION_FLAG3: &str = "migrated_v3";

#[derive(Clone, Debug, Serialize)]
pub struct Cfg {
    #[serde(rename = "gadget_visible")]
    pub gadget_visible: bool,
    pub opacity: i32,
    pub color: String,
    pub size: String,
    /// Idioma de la interfaz: "en" (default: instalación nueva) | "es".
    /// Solo afecta los textos mostrados, nunca los valores de config.
    pub language: String,
    pub position: Option<[i64; 2]>,
    /// Estado bloqueado ("HDR"/"SDR") mientras está activo el menú
    /// "Bloquear SDR/HDR" (None = sin bloqueo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr_lock: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_v2: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_v3: Option<i32>,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg {
            gadget_visible: true,
            opacity: 100, // 20 / 40 / 60 / 80 / 100
            color: "#22d3ee".into(),
            size: "Grande".into(), // Chico / Medio / Grande / Enorme
            language: "en".into(), // instalación nueva en inglés
            position: None,
            hdr_lock: None,
            migrated_v2: None,
            migrated_v3: None,
        }
    }
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| dirs_fallback());
    let d = PathBuf::from(base).join(APP_NAME);
    let _ = std::fs::create_dir_all(&d);
    d
}

fn dirs_fallback() -> String {
    std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into())
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// Carpeta de logs: %APPDATA%\BotonHDR2\log (el programa NO escribe
/// fuera de %APPDATA%\BotonHDR2, salvo dumps de debug opt-in por env).
pub fn log_dir() -> PathBuf {
    let d = config_dir().join("log");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Log de la corrida actual (se trunca al arrancar) + errores/panics.
pub fn log_path() -> PathBuf {
    log_dir().join("botonhdr_start.log")
}

/// Quita BOM UTF-8 si lo hay (Notepad y Set-Content -Encoding UTF8 de
/// PowerShell escriben con BOM; sin esto, el JSON cae a los defaults).
fn strip_bom(bytes: &[u8]) -> &str {
    let s = std::str::from_utf8(bytes).unwrap_or("");
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

pub fn load() -> Cfg {
    let mut data = Cfg::default();
    let Ok(bytes) = std::fs::read(config_path()) else {
        return data;
    };
    let Ok(user) = serde_json::from_str::<serde_json::Value>(strip_bom(&bytes)) else {
        return data;
    };
    let Some(obj) = user.as_object() else {
        return data;
    };

    // Keys de DEFAULTS (solo estas sobreviven al save/load).
    if let Some(v) = obj.get("gadget_visible").and_then(|v| v.as_bool()) {
        data.gadget_visible = v;
    }
    if let Some(v) = obj.get("opacity").and_then(|v| v.as_i64()) {
        data.opacity = v as i32;
    }
    if let Some(v) = obj.get("color").and_then(|v| v.as_str()) {
        data.color = v.to_string();
    }
    if let Some(v) = obj.get("size").and_then(|v| v.as_str()) {
        data.size = v.to_string();
    }
    if let Some(v) = obj.get("language").and_then(|v| v.as_str()) {
        if v == "es" || v == "en" {
            data.language = v.to_string();
        }
    }
    if let Some(arr) = obj.get("position").and_then(|v| v.as_array()) {
        if arr.len() == 2 {
            let x = arr[0].as_i64().unwrap_or(0);
            let y = arr[1].as_i64().unwrap_or(0);
            data.position = Some([x, y]);
        }
    }
    if let Some(v) = obj.get("hdr_lock").and_then(|v| v.as_str()) {
        if v == "HDR" || v == "SDR" {
            data.hdr_lock = Some(v.to_string());
        }
    }

    // ---- migración v2 (legacy: Mini→Medio, opacidades 25/50/75) --------
    let flag_v2 = obj.get(MIGRATION_FLAG).and_then(|v| v.as_i64()).map(|x| x as i32);
    if flag_v2 == Some(1) {
        data.migrated_v2 = Some(1);
    } else {
        let mut changed = false;
        // Tamaño: solo "Mini" es inequívocamente legacy; los nombres
        // ambiguos se asumen ya migrados (nomenclatura nueva).
        let size_present = obj.get("size").is_some();
        let size_v2 = obj.get("size_v").and_then(|v| v.as_i64()) == Some(2);
        if size_present && !size_v2 {
            if obj.get("size").and_then(|v| v.as_str()) == Some("Mini") {
                data.size = "Medio".into();
                changed = true;
            }
        }
        // Opacidad: 25/50/75 viejo → paso más cercano (empates arriba).
        match data.opacity {
            25 => {
                data.opacity = 20;
                changed = true;
            }
            50 => {
                data.opacity = 60;
                changed = true;
            }
            75 => {
                data.opacity = 80;
                changed = true;
            }
            _ => {}
        }
        if changed {
            data.migrated_v2 = Some(1);
        }
    }

    // ---- migración v3 (renombre 9º pedido: Micro→Chico→...→Enorme) -----
    let flag_v3 = obj.get(MIGRATION_FLAG3).and_then(|v| v.as_i64()).map(|x| x as i32);
    if obj.get("size").is_some() && flag_v3 != Some(1) {
        let mapped = match data.size.as_str() {
            "Micro" => Some("Chico"),
            "Chico" => Some("Medio"),
            "Medio" => Some("Grande"),
            "Grande" => Some("Enorme"),
            _ => None,
        };
        if let Some(m) = mapped {
            data.size = m.into();
            data.migrated_v3 = Some(1);
        }
    }
    if flag_v3 == Some(1) {
        data.migrated_v3 = Some(1);
    }

    // Re-escribir si cambió algún flag (one-shot).
    if data.migrated_v2 != flag_v2 || data.migrated_v3 != flag_v3 {
        save(&data);
    }
    data
}

pub fn save(c: &Cfg) {
    if let Ok(json) = serde_json::to_string_pretty(c) {
        let _ = std::fs::write(config_path(), json);
    }
}
