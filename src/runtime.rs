//! Estado activo de render en runtime (tema, estilo y modo del visualizador).
//!
//! Estas celdas cambian en caliente al previsualizar/aplicar desde los menús y
//! se inicializan de forma perezosa desde `config()` en el primer acceso.
//! Dependencia unidireccional: `runtime` usa `config`; `config` no referencia
//! a `runtime`.

use std::env;
use std::sync::{OnceLock, RwLock};

use crate::config::{Theme, VisualizerMode, VisualizerStyle, config, resolve_visualizer_style};

// ---------------------------------------------------------------------------
// Tema activo en runtime
// ---------------------------------------------------------------------------

/// Estado que cambia en caliente al previsualizar/aplicar temas desde el menú.
/// Se eligió una celda `RwLock` con copia por lectura (11 `Color` por frame)
/// en vez de pasar `&Theme` por todo el render: un solo punto de acceso
/// (`ui::theme()`), cero cambios en los ~100 call sites y sin dependencias.
struct ActiveTheme {
    name: String,
    theme: Theme,
}

static ACTIVE: OnceLock<RwLock<ActiveTheme>> = OnceLock::new();

fn active_cell() -> &'static RwLock<ActiveTheme> {
    ACTIVE.get_or_init(|| {
        let config = config();
        RwLock::new(ActiveTheme {
            name: config.theme_name.clone(),
            theme: config.theme.clone(),
        })
    })
}

/// Tema activo para renderizar. Copia barata: 11 `Color` (~44 B).
pub fn active_theme() -> Theme {
    active_cell()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .theme
        .clone()
}

/// Id del tema activo (el que quedó aplicado; la previsualización no lo toca).
pub fn active_theme_name() -> String {
    active_cell()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .name
        .clone()
}

/// Previsualización (↑/↓ del menú): cambia solo los colores.
pub fn preview_theme(theme: Theme) {
    active_cell()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .theme = theme;
}

/// Aplica tema con su nombre (Enter del menú, después de persistir).
pub fn activate_theme(name: &str, theme: Theme) {
    let mut guard = active_cell()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.name = name.to_string();
    guard.theme = theme;
}

/// Estilo efectivo para renderizar: estilo activo + `WT_SESSION` real del
/// entorno. El estilo activo arranca del cargado al arrancar y cambia en
/// caliente al previsualizar/aplicar desde el selector de Bandas, igual
/// que la celda del tema: así el fondo previsualiza en vivo.
pub fn effective_visualizer_style() -> VisualizerStyle {
    resolve_visualizer_style(
        active_visualizer_style(),
        env::var("WT_SESSION").ok().as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Estilo activo en runtime
// ---------------------------------------------------------------------------

/// Estilo configurado que cambia en caliente al previsualizar/aplicar desde
/// el selector de Bandas: un solo punto de acceso y cero cambios en el
/// render (que ya lee `effective_visualizer_style()` por frame).
static ACTIVE_VISUALIZER: OnceLock<RwLock<VisualizerStyle>> = OnceLock::new();

fn visualizer_cell() -> &'static RwLock<VisualizerStyle> {
    ACTIVE_VISUALIZER.get_or_init(|| RwLock::new(config().visualizer_style))
}

/// Estilo configurado activo (el aplicado; la previsualización lo cambia en
/// vivo y `Esc` lo restaura).
pub fn active_visualizer_style() -> VisualizerStyle {
    *visualizer_cell()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Previsualización (↑/↓ del selector): cambia el estilo en vivo.
pub fn preview_visualizer_style(style: VisualizerStyle) {
    *visualizer_cell()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = style;
}

/// Aplica el estilo (Enter del selector, después de persistir).
pub fn activate_visualizer_style(style: VisualizerStyle) {
    preview_visualizer_style(style);
}

/// Modo configurado que cambia en caliente al previsualizar/aplicar desde
/// el selector de Modo: un solo punto de acceso; el dispatch de render lo
/// lee por frame.
static ACTIVE_VISUALIZER_MODE: OnceLock<RwLock<VisualizerMode>> = OnceLock::new();

fn visualizer_mode_cell() -> &'static RwLock<VisualizerMode> {
    ACTIVE_VISUALIZER_MODE.get_or_init(|| RwLock::new(config().visualizer_mode))
}

/// Modo configurado activo (el aplicado; la previsualización lo cambia en
/// vivo y `Esc` lo restaura).
pub fn active_visualizer_mode() -> VisualizerMode {
    *visualizer_mode_cell()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Previsualización (↑/↓ del selector): cambia el modo en vivo.
pub fn preview_visualizer_mode(mode: VisualizerMode) {
    *visualizer_mode_cell()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
}

/// Aplica el modo (Enter del selector, después de persistir).
pub fn activate_visualizer_mode(mode: VisualizerMode) {
    preview_visualizer_mode(mode);
}
