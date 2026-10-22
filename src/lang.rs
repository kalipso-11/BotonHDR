//! Idioma de la interfaz (menú de la bandeja + textos de configuración).
//!
//! - `Cfg.language`: `"es"` | `"en"`. Default `"en"`: una instalación nueva
//!   arranca en inglés; el usuario elige en el menú → "Idioma"/"Language".
//! - El cambio aplica al instante (el menú se reconstruye en cada apertura)
//!   y persiste en `config.json`.
//! - Los VALORES guardados en config (tamaño "Chico"/"Medio"/..., color
//!   "#22d3ee") NO cambian con el idioma: son opacos y estables entre
//!   versiones. Solo cambia el TEXTO mostrado, vía los mapas de abajo
//!   (clave = valor interno).

/// Traduce un par (español, inglés) según el idioma activo.
/// Cualquier valor de `lang` distinto de `"es"` → inglés (default).
pub fn t<'a>(lang: &'a str, es: &'a str, en: &'a str) -> &'a str {
    if lang == "es" { es } else { en }
}

/// Nombre mostrado de un color (clave: el hex interno de `draw::COLORS`).
pub fn color_name(hexv: &str, lang: &str) -> &'static str {
    match (hexv, lang) {
        ("#22d3ee", "es") => "Cian",
        ("#34d399", "es") => "Verde",
        ("#facc15", "es") => "Amarillo",
        ("#fb923c", "es") => "Naranja",
        ("#f87171", "es") => "Rojo",
        ("#a78bfa", "es") => "Violeta",
        ("#f1f5f9", "es") => "Blanco",
        ("#22d3ee", _) => "Cyan",
        ("#34d399", _) => "Green",
        ("#facc15", _) => "Yellow",
        ("#fb923c", _) => "Orange",
        ("#f87171", _) => "Red",
        ("#a78bfa", _) => "Purple",
        ("#f1f5f9", _) => "White",
        _ => "Color",
    }
}

/// Nombre mostrado de un tamaño (clave: el valor interno de `draw::SIZES`,
/// que es lo que guarda `Cfg.size`). Desconocido → tal cual (defensivo).
pub fn size_name<'a>(internal: &'a str, lang: &'a str) -> &'a str {
    match (internal, lang) {
        ("Chico", "es") => "Chico",
        ("Medio", "es") => "Medio",
        ("Grande", "es") => "Grande",
        ("Enorme", "es") => "Enorme",
        ("Chico", _) => "Small",
        ("Medio", _) => "Medium",
        ("Grande", _) => "Large",
        ("Enorme", _) => "Huge",
        _ => internal,
    }
}

/// Segunda línea del tooltip de la bandeja (línea 1 ya es EN: "HDR On/Off").
pub fn tip_line2(lang: &str) -> &'static str {
    if lang == "es" { "click derecho para configurar" } else { "right-click to configure" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_elige_idioma() {
        assert_eq!(t("es", "A", "B"), "A");
        assert_eq!(t("en", "A", "B"), "B");
        // valor inválido → inglés (default)
        assert_eq!(t("fr", "A", "B"), "B");
    }

    #[test]
    fn colores_por_idioma() {
        assert_eq!(color_name("#22d3ee", "es"), "Cian");
        assert_eq!(color_name("#22d3ee", "en"), "Cyan");
        assert_eq!(color_name("#a78bfa", "es"), "Violeta");
        assert_eq!(color_name("#a78bfa", "en"), "Purple");
        assert_eq!(color_name("#f1f5f9", "en"), "White");
        // hex desconocido → genérico
        assert_eq!(color_name("#000000", "en"), "Color");
    }

    #[test]
    fn tamanos_por_idioma() {
        assert_eq!(size_name("Chico", "es"), "Chico");
        assert_eq!(size_name("Chico", "en"), "Small");
        assert_eq!(size_name("Enorme", "en"), "Huge");
        // valor desconocido → tal cual (defensivo)
        assert_eq!(size_name("Otros", "en"), "Otros");
    }

    #[test]
    fn tip_line2_por_idioma() {
        assert_eq!(tip_line2("es"), "click derecho para configurar");
        assert_eq!(tip_line2("en"), "right-click to configure");
    }
}
