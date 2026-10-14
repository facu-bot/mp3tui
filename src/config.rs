//! Configuración tema visual y atajos remapeables.
//!
//! El archivo `config.toml` es opcional: sin él (o con partes inválidas) la
//! app corre con los mismos valores por defecto que antes de existirlo.
//! Cada campo inválido cae **solo** a su default y deja un aviso en
//! `Config::warnings` (se muestra en stderr al arrancar y en la primera
//! pantalla de la TUI).
//!
//! Ruta por defecto (convención nativa de cada SO, sin dependencias extra):
//! en Windows `%APPDATA%\mp3tui\config.toml`; en Linux/macOS
//! `$XDG_CONFIG_HOME/mp3tui/config.toml` (o `~/.config/mp3tui/config.toml`).
//! La variable `MP3TUI_CONFIG` permite apuntar a otro archivo (útil para
//! pruebas).
//!
//! Formato soportado — un subconjunto razonable de TOML con claves planas:
//!
//! ```toml
//! [theme]
//! accent = "#957FB8"        # «#RRGGBB» o nombre: cyan, blanco, gris,...
//! spectrum_low = "#8992A7"  # gradiente del visualizador (abajo)
//!
//! [keybindings]
//! quit = "q,esc"            # varias teclas separadas por coma
//! seek_fwd = "right"
//! speed_up = "]"
//! ```
//!
//! El hot-reload está fuera del MVP (backlog): la config se lee una vez al
//! arrancar.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;

/// Override de la ruta del archivo (pruebas / instalaciones portables).
pub const CONFIG_ENV_OVERRIDE: &str = "MP3TUI_CONFIG";

/// Id del tema builtin usado cuando no hay `[theme] name` o no se reconoce.
pub const DEFAULT_THEME_ID: &str = "kanagawa-dragon";

/// Configuración efectiva de la app (se carga una vez al arrancar).
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Tema efectivo: paleta del `[theme] name` resuelto + overrides encima.
    pub theme: Theme,
    /// Id del tema builtin resuelto desde `[theme] name` (o el default).
    pub theme_name: String,
    /// Colores personalizados de `[theme]` (clave → color): se aplican
    /// encima de la paleta del tema elegido, también al previsualizar.
    pub theme_overrides: Vec<(String, Color)>,
    pub keys: Keys,
    pub library: LibraryConfig,
    /// `[spotify]`: ajustes de la integración con Spotify.
    pub spotify: SpotifyConfig,
    /// `[ui] mouse`: captura del mouse (clic, arrastre, rueda). Activada por
    /// defecto; con `false` la terminal conserva su selección de texto nativa.
    pub mouse: bool,
    /// `[visualizer] style`: ascii, unicode o auto (según `WT_SESSION`).
    pub visualizer_style: VisualizerStyle,
    /// `[visualizer] mode`: barras (default), onda, espejo, vu, circular o
    /// miku. El valor eliminado `caida` avisa específicamente y cae a `barras`.
    pub visualizer_mode: VisualizerMode,
    /// `true` cuando `[visualizer] mode` viene explicitado en config.toml: un
    /// modo escrito a mano gana sobre el default que el tema declara (el
    /// tema miku arranca en modo bailarina solo si no hay modo explícito).
    pub(crate) visualizer_mode_explicit: bool,
    /// `true` cuando `parse()` encontró el valor eliminado `caida` en
    /// `[visualizer] mode`: señal para que `load()` reescriba la config real
    /// a `barras` una sola vez (`parse()` puro, el que usan los tests, nunca
    /// toca archivos).
    pub(crate) removed_visualizer_mode: bool,
    /// Avisos de la carga: campos inválidos, claves desconocidas o archivo
    /// ilegible. Vacío cuando todo (o el archivo mismo) está bien.
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            theme_name: DEFAULT_THEME_ID.to_string(),
            theme_overrides: Vec::new(),
            keys: Keys::default(),
            library: LibraryConfig::default(),
            spotify: SpotifyConfig::default(),
            mouse: true,
            visualizer_style: VisualizerStyle::default(),
            visualizer_mode: VisualizerMode::default(),
            visualizer_mode_explicit: false,
            removed_visualizer_mode: false,
            warnings: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Ruta y ciclo de vida
// ---------------------------------------------------------------------------

/// Ruta por defecto de `config.toml`, siguiendo la convención nativa del SO:
///
/// - Windows: `%APPDATA%\mp3tui\config.toml`.
/// - Linux/macOS: `$XDG_CONFIG_HOME/mp3tui/config.toml`, o
///   `~/.config/mp3tui/config.toml` si `XDG_CONFIG_HOME` no está definida.
///
/// `None` si no hay ninguna variable base disponible. `MP3TUI_CONFIG` pisa
/// esta ruta (ver [`resolve_path`]).
pub fn default_path() -> Option<PathBuf> {
    base_config_dir().map(|dir| dir.join("config.toml"))
}

/// Carpeta base de la config del usuario (la que contiene `config.toml`).
/// Windows conserva `%APPDATA%\mp3tui`; el resto usa XDG con fallback a
/// `~/.config`.
fn base_config_dir() -> Option<PathBuf> {
    if let Some(appdata) = env::var_os("APPDATA").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(appdata).join("mp3tui"));
    }
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("mp3tui"));
    }
    env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".config").join("mp3tui"))
}

fn resolve_path() -> Option<(PathBuf, bool)> {
    match env::var(CONFIG_ENV_OVERRIDE) {
        Ok(custom) if !custom.trim().is_empty() => Some((PathBuf::from(custom), true)),
        _ => default_path().map(|path| (path, false)),
    }
}

/// Directorio que contiene `config.toml` (o el directorio indicado por `MP3TUI_CONFIG`).
pub fn config_dir() -> Option<PathBuf> {
    resolve_path().and_then(|(path, _)| {
        path.parent().map(|parent| {
            if parent.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                parent.to_path_buf()
            }
        })
    })
}

/// Lee el archivo (si existe) y lo parsea. Sin archivo: default silencioso.
pub fn load() -> Config {
    let Some((path, from_override)) = resolve_path() else {
        return Config::default();
    };
    match fs::read_to_string(&path) {
        Ok(text) => {
            let mut config = parse(&text);
            if config.removed_visualizer_mode {
                migrate_removed_visualizer_mode_at(&path, &mut config.warnings);
            }
            config
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if from_override {
                Config {
                    warnings: vec![format!(
                        "MP3TUI_CONFIG apunta a '{}' y el archivo no existe",
                        path.display()
                    )],
                    ..Config::default()
                }
            } else {
                Config::default()
            }
        }
        Err(err) => Config {
            warnings: vec![format!("no se pudo leer '{}': {err}", path.display())],
            ..Config::default()
        },
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Carga la config (una sola vez) y la deja disponible para toda la app.
pub fn init() -> &'static Config {
    CONFIG.get_or_init(load)
}

/// Config efectiva; default si nadie llamó a `init()` (tests).
pub fn config() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}

// ---------------------------------------------------------------------------
// Parseo por campo
// ---------------------------------------------------------------------------

/// Parsea TOML y hace merge campo a campo sobre los defaults: lo inválido en
/// un campo no contamina a los demás.
pub fn parse(text: &str) -> Config {
    let mut warnings = Vec::new();

    let table = match text.parse::<toml::Table>() {
        Ok(table) => table,
        Err(err) => {
            return Config {
                warnings: vec![format!(
                    "config.toml con sintaxis inválida; se ignoró el archivo \
                     y se usan los valores por defecto: {err}"
                )],
                ..Config::default()
            };
        }
    };

    let mut config = Config::default();

    for section in table.keys() {
        match section.as_str() {
            "theme" | "keybindings" | "library" | "ui" | "visualizer" | "spotify" => {}
            other => warnings.push(format!("sección desconocida «[{other}]» (se ignora)")),
        }
    }

    match table.get("theme") {
        None => {}
        Some(value) => match value.as_table() {
            Some(section) => parse_theme(section, &mut config, &mut warnings),
            None => warnings.push(
                "[theme] debe ser una tabla con claves de la forma \
                 clave = \"valor\""
                    .to_string(),
            ),
        },
    }

    match table.get("keybindings") {
        None => {}
        Some(value) => match value.as_table() {
            Some(section) => parse_keybindings(section, &mut config.keys, &mut warnings),
            None => warnings.push(
                "[keybindings] debe ser una tabla con claves de la forma \
                 clave = \"valor\""
                    .to_string(),
            ),
        },
    }

    match table.get("ui") {
        None => {}
        Some(value) => match value.as_table() {
            Some(section) => parse_ui(section, &mut config, &mut warnings),
            None => {
                warnings.push("[ui] debe ser una tabla, por ejemplo: mouse = false".to_string())
            }
        },
    }

    match table.get("visualizer") {
        None => {}
        Some(value) => match value.as_table() {
            Some(section) => parse_visualizer(section, &mut config, &mut warnings),
            None => warnings.push(
                "[visualizer] debe ser una tabla, por ejemplo: style = \"ascii\"".to_string(),
            ),
        },
    }

    match table.get("library") {
        None => {}
        Some(value) => match value.as_table() {
            Some(section) => parse_library(section, &mut config.library, &mut warnings),
            None => warnings.push(
                "[library] debe ser una tabla con claves de la forma \
                 roots = [\"ruta\", ...]"
                    .to_string(),
            ),
        },
    }

    match table.get("spotify") {
        None => {}
        Some(value) => match value.as_table() {
            Some(section) => parse_spotify(section, &mut config.spotify, &mut warnings),
            None => warnings.push(
                "[spotify] debe ser una tabla, por ejemplo: client_id = \"...\"".to_string(),
            ),
        },
    }

    // el tema Miku arranca en modo bailarina salvo que `[visualizer]
    // mode` venga explicitado (un modo a mano gana, y se cambia desde el
    // sub-modal de modo).
    if !config.visualizer_mode_explicit && config.theme_name == "miku" {
        config.visualizer_mode = VisualizerMode::Miku;
    }

    // Tras aplicar defaults por campo: si dos acciones distintas terminaron
    // con la misma tecla, el dispatch (if/else if con orden fijo) sombrearía
    // la posterior en silencio. Se resuelve aquí y se avisa por warning,
    // igual que en el resto de los casos inválidos de la config.
    resolve_key_collisions(&mut config.keys, &mut warnings);

    config.warnings = warnings;
    config
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// Paleta de la app. El default es la variante **Kanagawa Dragon** de
/// kanagawa.nvim: tonos oscuros y cálidos, acentos desaturados y
/// gradiente frío→cálido. Sigue siendo configurable campo a campo desde
/// `[theme]` del config.toml.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    /// Títulos, bordes destacados, selección de cursor, barras activas.
    pub accent: Color,
    /// Texto principal (nombres de archivo, valores).
    pub text: Color,
    /// Etiquetas y texto secundario.
    pub dim: Color,
    /// Fondo de la fila seleccionada en el explorador.
    pub select_bg: Color,
    /// Línea de error.
    pub error: Color,
    /// Estado «reproduciendo» y pista activa.
    pub ok: Color,
    /// Estado «pausado».
    pub warn: Color,
    /// Gradiente del visualizador: base de las barras.
    pub spectrum_low: Color,
    /// Gradiente del visualizador: mitad.
    pub spectrum_mid: Color,
    /// Gradiente del visualizador: pico.
    pub spectrum_high: Color,
    /// Secciones precargadas en la barra de progreso (estilo franja YouTube).
    pub buffered: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            // oniViolet (palette kanagawa): violeta con el contraste justo
            // para bordes y cursor sin el glare de un violeta saturado.
            accent: Color::Rgb(0x95, 0x7F, 0xB8),
            // dragonWhite: gris cálido claro, el fg de la variante dragon.
            text: Color::Rgb(0xC5, 0xC9, 0xC5),
            // dragonGray3: gris medio-tibio; etiquetas legibles que no
            // compiten con el texto principal.
            dim: Color::Rgb(0x7A, 0x83, 0x82),
            // dragonBlack5: selección visible sobre el fondo casi negro
            // (dragonBlack3, #181616) sin desentonar con la paleta.
            select_bg: Color::Rgb(0x39, 0x38, 0x36),
            // dragonRed: rojo muy desaturado de kanagawa.
            error: Color::Rgb(0xC4, 0x74, 0x6E),
            // dragonGreen: verde apagado, «ok» sin semáforo chillón.
            ok: Color::Rgb(0x87, 0xA9, 0x87),
            // dragonYellow: amarillo cálido para «pausado».
            warn: Color::Rgb(0xC4, 0xB2, 0x8A),
            // Gradiente del visualizador dentro de la paleta Dragon:
            // violeta (base) → celeste (mitad) → rojo (pico): frío en lo
            // bajo, cálido solo en los máximos, coherente con el resto.
            spectrum_low: Color::Rgb(0x89, 0x92, 0xA7), // dragonViolet
            spectrum_mid: Color::Rgb(0x7F, 0xB4, 0xCA), // springBlue
            spectrum_high: Color::Rgb(0xC4, 0x74, 0x6E), // dragonRed
            // dragonSilver / dragonGray: franja clara sobre el gris de la barra.
            buffered: Color::Rgb(0xA6, 0xA6, 0x9C),
        }
    }
}

fn parse_theme(section: &toml::Table, config: &mut Config, warnings: &mut Vec<String>) {
    // Pass 1: `name` — resuelve la paleta base ANTES de los overrides de
    // color, que siguen pisando lo que sea.
    match section.get("name") {
        None => {}
        Some(value) => match value.as_str() {
            Some(raw) => match builtin_themes().iter().find(|(id, _)| *id == raw) {
                Some((id, theme)) => {
                    config.theme_name = id.to_string();
                    config.theme = theme.clone();
                }
                None => warnings.push(format!(
                    "[theme] nombre desconocido «{raw}»; se usa \
                     «{DEFAULT_THEME_ID}» (disponibles: {})",
                    builtin_themes()
                        .iter()
                        .map(|(id, _)| *id)
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            },
            None => warnings.push(format!(
                "[theme] name: se espera un id de tema entre comillas, \
                 p. ej. \"{DEFAULT_THEME_ID}\"; se usa «{DEFAULT_THEME_ID}»"
            )),
        },
    }

    // Pass 2: colores — pisan la paleta elegida y quedan registrados en
    // `theme_overrides` para re-aplicarlos en cada previsualización.
    let theme = &mut config.theme;
    let overrides = &mut config.theme_overrides;
    for (key, value) in section {
        if key == "name" {
            continue;
        }
        let Some(field) = theme_slot(theme, key) else {
            warnings.push(format!("[theme] clave desconocida «{key}»"));
            continue;
        };
        let Some(raw) = value.as_str() else {
            warnings.push(format!(
                "[theme] {key}: se espera un color entre comillas, \
                 p. ej. \"#22D3EE\"; se mantiene el valor por defecto"
            ));
            continue;
        };
        match parse_color(raw) {
            Some(color) => {
                *field = color;
                match overrides.iter().position(|(k, _)| k == key) {
                    Some(pos) => overrides[pos].1 = color,
                    None => overrides.push((key.clone(), color)),
                }
            }
            None => warnings.push(format!(
                "[theme] {key}: color «{raw}» no reconocido («#RRGGBB» o \
                 nombre); se mantiene el valor por defecto"
            )),
        }
    }
}

/// «#RRGGBB» o nombre básico (inglés o español). `None` si no se reconoce.
pub fn parse_color(raw: &str) -> Option<Color> {
    let value = raw.trim();

    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }

    let color = match value.to_ascii_lowercase().as_str() {
        "black" | "negro" => Color::Black,
        "red" | "rojo" => Color::Red,
        "green" | "verde" => Color::Green,
        "yellow" | "amarillo" => Color::Yellow,
        "blue" | "azul" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" | "cian" => Color::Cyan,
        "white" | "blanco" => Color::White,
        "gray" | "gris" => Color::Gray,
        "darkgray" | "dark gray" | "gris oscuro" => Color::DarkGray,
        "lightgray" | "light gray" | "gris claro" => Color::Gray,
        _ => return None,
    };
    Some(color)
}

// ---------------------------------------------------------------------------
// Paletas builtin
// ---------------------------------------------------------------------------

/// Las 7 paletas oficiales, en orden estable de menú. El id es el identificador
/// que `[theme] name` acepta en config.toml y el que se guarda al aplicar.
/// Cada paleta llena los 11 slots de `Theme`; los hex son los colores oficiales
/// de cada proyecto (comentario por paleta).
pub fn builtin_themes() -> &'static [(&'static str, Theme)] {
    static THEMES: OnceLock<Vec<(&'static str, Theme)>> = OnceLock::new();
    THEMES
        .get_or_init(|| {
            vec![
                // Kanagawa Dragon == Theme::default() (rebelot/kanagawa.nvim).
                ("kanagawa-dragon", Theme::default()),
                // folke/tokyonight.nvim, variante `night`.
                (
                    "tokyonight",
                    Theme {
                        accent: Color::Rgb(0x7A, 0xA2, 0xF7),        // blue
                        text: Color::Rgb(0xC0, 0xCA, 0xF5),          // fg
                        dim: Color::Rgb(0x56, 0x5F, 0x89),           // comment
                        select_bg: Color::Rgb(0x29, 0x2E, 0x42),     // bg_highlight
                        error: Color::Rgb(0xF7, 0x76, 0x8E),         // red
                        ok: Color::Rgb(0x9E, 0xCE, 0x6A),            // green
                        warn: Color::Rgb(0xE0, 0xAF, 0x68),          // yellow
                        spectrum_low: Color::Rgb(0x7D, 0xCF, 0xFF),  // cyan
                        spectrum_mid: Color::Rgb(0x7A, 0xA2, 0xF7),  // blue
                        spectrum_high: Color::Rgb(0xF7, 0x76, 0x8E), // red
                        buffered: Color::Rgb(0x54, 0x5C, 0x7E),      // dark3
                    },
                ),
                // catppuccin/catppuccin, flavor `mocha`.
                (
                    "catppuccin-mocha",
                    Theme {
                        accent: Color::Rgb(0xCB, 0xA6, 0xF7),        // mauve
                        text: Color::Rgb(0xCD, 0xD6, 0xF4),          // text
                        dim: Color::Rgb(0x6C, 0x70, 0x86),           // overlay0
                        select_bg: Color::Rgb(0x45, 0x47, 0x5A),     // surface1
                        error: Color::Rgb(0xF3, 0x8B, 0xA8),         // red
                        ok: Color::Rgb(0xA6, 0xE3, 0xA1),            // green
                        warn: Color::Rgb(0xF9, 0xE2, 0xAF),          // yellow
                        spectrum_low: Color::Rgb(0x89, 0xDC, 0xEB),  // sky
                        spectrum_mid: Color::Rgb(0xB4, 0xBE, 0xFE),  // lavender
                        spectrum_high: Color::Rgb(0xF3, 0x8B, 0xA8), // red
                        buffered: Color::Rgb(0x7F, 0x84, 0x9C),      // overlay1
                    },
                ),
                // dracula/dracula-theme (colores core).
                (
                    "dracula",
                    Theme {
                        accent: Color::Rgb(0xBD, 0x93, 0xF9),        // purple
                        text: Color::Rgb(0xF8, 0xF8, 0xF2),          // fg
                        dim: Color::Rgb(0x62, 0x72, 0xA4),           // comment
                        select_bg: Color::Rgb(0x44, 0x47, 0x5A),     // selection
                        error: Color::Rgb(0xFF, 0x55, 0x55),         // red
                        ok: Color::Rgb(0x50, 0xFA, 0x7B),            // green
                        warn: Color::Rgb(0xF1, 0xFA, 0x8C),          // yellow
                        spectrum_low: Color::Rgb(0x8B, 0xE9, 0xFD),  // cyan
                        spectrum_mid: Color::Rgb(0xFF, 0x79, 0xC6),  // pink
                        spectrum_high: Color::Rgb(0xFF, 0x55, 0x55), // red
                        // El set core de Dracula no tiene un neutro claro extra:
                        // la franja `buffered` reusa el fg («franja clara»).
                        buffered: Color::Rgb(0xF8, 0xF8, 0xF2), // fg
                    },
                ),
                // morhetz/gruvbox (dark; tonos bright para estados).
                (
                    "gruvbox-dark",
                    Theme {
                        accent: Color::Rgb(0xFE, 0x80, 0x19),        // orange
                        text: Color::Rgb(0xEB, 0xDB, 0xB2),          // fg bright
                        dim: Color::Rgb(0x7C, 0x6F, 0x64),           // dark4
                        select_bg: Color::Rgb(0x3C, 0x38, 0x36),     // bg1
                        error: Color::Rgb(0xFB, 0x49, 0x34),         // red bright
                        ok: Color::Rgb(0xB8, 0xBB, 0x26),            // green bright
                        warn: Color::Rgb(0xFA, 0xBD, 0x2F),          // yellow bright
                        spectrum_low: Color::Rgb(0x8E, 0xC0, 0x7C),  // aqua bright
                        spectrum_mid: Color::Rgb(0x83, 0xA5, 0x98),  // blue bright
                        spectrum_high: Color::Rgb(0xFE, 0x80, 0x19), // orange
                        buffered: Color::Rgb(0x92, 0x83, 0x74),      // gray
                    },
                ),
                // nordtheme/nord (nord0–nord15).
                (
                    "nord",
                    Theme {
                        accent: Color::Rgb(0x88, 0xC0, 0xD0),        // nord8
                        text: Color::Rgb(0xD8, 0xDE, 0xE9),          // nord4
                        dim: Color::Rgb(0x4C, 0x56, 0x6A),           // nord3
                        select_bg: Color::Rgb(0x3B, 0x42, 0x52),     // nord1
                        error: Color::Rgb(0xBF, 0x61, 0x6A),         // nord11
                        ok: Color::Rgb(0xA3, 0xBE, 0x8C),            // nord14
                        warn: Color::Rgb(0xEB, 0xCB, 0x8B),          // nord13
                        spectrum_low: Color::Rgb(0x8F, 0xBC, 0xBB),  // nord7
                        spectrum_mid: Color::Rgb(0x81, 0xA1, 0xC1),  // nord9
                        spectrum_high: Color::Rgb(0xD0, 0x87, 0x70), // nord12
                        buffered: Color::Rgb(0x5E, 0x81, 0xAC),      // nord10
                    },
                ),
                // rosé-pine/main (Rosé Pine).
                (
                    "rose-pine",
                    Theme {
                        accent: Color::Rgb(0xC4, 0xA7, 0xE7),        // iris
                        text: Color::Rgb(0xE0, 0xDE, 0xF4),          // text
                        dim: Color::Rgb(0x6E, 0x6A, 0x86),           // muted
                        select_bg: Color::Rgb(0x40, 0x3D, 0x52),     // highlightMed
                        error: Color::Rgb(0xEB, 0x6F, 0x92),         // love
                        ok: Color::Rgb(0x31, 0x74, 0x8F),            // pine
                        warn: Color::Rgb(0xF6, 0xC1, 0x77),          // gold
                        spectrum_low: Color::Rgb(0x9C, 0xCF, 0xD8),  // foam
                        spectrum_mid: Color::Rgb(0xC4, 0xA7, 0xE7),  // iris
                        spectrum_high: Color::Rgb(0xEB, 0xBC, 0xBA), // rose
                        buffered: Color::Rgb(0x90, 0x8C, 0xAA),      // subtle
                    },
                ),
                // Hatsune Miku: turquesa/cian de su paleta oficial, con el
                // rosa como acento cálido. El modo visualizador default de
                // este tema es la bailarina (ver parse).
                (
                    "miku",
                    Theme {
                        accent: Color::Rgb(0x39, 0xC5, 0xBB),        // miku turquesa
                        text: Color::Rgb(0xE8, 0xF1, 0xF3),          // hielo
                        dim: Color::Rgb(0x7F, 0xA8, 0xAC),           // teal grisáceo
                        select_bg: Color::Rgb(0x1F, 0x2E, 0x33),     // teal noche
                        error: Color::Rgb(0xFF, 0x6E, 0x9B),         // miku rosa
                        ok: Color::Rgb(0x7B, 0xE0, 0xC0),            // menta
                        warn: Color::Rgb(0xFF, 0xD1, 0x66),          // amarillo cálido
                        spectrum_low: Color::Rgb(0x39, 0xC5, 0xBB),  // turquesa
                        spectrum_mid: Color::Rgb(0x6B, 0xE5, 0xF5),  // cian
                        spectrum_high: Color::Rgb(0xFF, 0x9F, 0xD0), // rosa
                        buffered: Color::Rgb(0x4A, 0x8B, 0x90),      // teal oscuro
                    },
                ),
            ]
        })
        .as_slice()
}

/// Paleta builtin por id; `None` si el id no existe.
pub fn builtin_theme(id: &str) -> Option<&'static Theme> {
    builtin_themes()
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, theme)| theme)
}

/// Paleta efectiva para un id: builtin + los overrides personalizados de
/// `[theme]` (pisan la paleta, también al previsualizar). Id desconocido →
/// default.
pub fn resolve_theme(id: &str) -> Theme {
    let mut theme = builtin_theme(id).cloned().unwrap_or_default();
    apply_theme_overrides(&mut theme, &config().theme_overrides);
    theme
}

/// Aplica los overrides de `[theme]` sobre una paleta base, en el mismo orden
/// que `parse_theme`.
fn apply_theme_overrides(theme: &mut Theme, overrides: &[(String, Color)]) {
    for (key, color) in overrides {
        if let Some(field) = theme_slot(theme, key) {
            *field = *color;
        }
    }
}

/// Campo mutable de `Theme` para una clave de config; `None` si no existe.
fn theme_slot<'a>(theme: &'a mut Theme, key: &str) -> Option<&'a mut Color> {
    match key {
        "accent" => Some(&mut theme.accent),
        "text" => Some(&mut theme.text),
        "dim" => Some(&mut theme.dim),
        "select_bg" => Some(&mut theme.select_bg),
        "error" => Some(&mut theme.error),
        "ok" => Some(&mut theme.ok),
        "warn" => Some(&mut theme.warn),
        "spectrum_low" => Some(&mut theme.spectrum_low),
        "spectrum_mid" => Some(&mut theme.spectrum_mid),
        "spectrum_high" => Some(&mut theme.spectrum_high),
        "buffered" => Some(&mut theme.buffered),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Editor de config.toml por líneas
// ---------------------------------------------------------------------------
//
// Tema, bandas, modo y mouse se guardan con el mismo editor: agregar la
// sección si falta, reemplazar la clave dentro de la sección conservando
// indentación y comentarios, o insertarla debajo del encabezado. Antes había
// cuatro copias del mismo cuerpo, que ya habían divergido entre sí.

/// Valor de una clave de config ya validado: sabe renderizarse como token TOML
/// y compararse contra lo que quedó escrito en el archivo.
enum ConfigValue<'a> {
    /// Cadena; en TOML va entre comillas.
    Str(&'a str),
    /// Booleano; en TOML va sin comillas.
    Bool(bool),
}

impl ConfigValue<'_> {
    /// Token TOML tal como se escribe: `"nord"` o `true`.
    fn render(&self) -> String {
        match self {
            Self::Str(value) => format!("\"{value}\""),
            Self::Bool(value) => value.to_string(),
        }
    }

    /// ¿`value` es exactamente el valor que se pidió guardar?
    fn matches(&self, value: &toml::Value) -> bool {
        match (self, value) {
            (Self::Str(want), toml::Value::String(got)) => got == want,
            (Self::Bool(want), toml::Value::Boolean(got)) => got == want,
            _ => false,
        }
    }
}

/// Devuelve `text` con `key = value` dentro de `[section]`: reemplaza la clave
/// si ya está, la inserta debajo del encabezado si la sección existe, o agrega
/// la sección al final con la clave adentro. Conserva CRLF, la indentación y
/// el comentario de la línea reemplazada.
fn edit_section_key(text: &str, section: &str, key: &str, value: &str) -> String {
    let crlf = text.contains("\r\n");
    let eol = if crlf { "\r\n" } else { "\n" };

    // Encabezado de sección: `[sección]` (+ comentario opcional). Un
    // comentario que menciona `[sección]` no abre sección.
    fn header_section(line: &str) -> Option<&str> {
        let t = line.trim_start();
        let rest = t.strip_prefix('[')?;
        let end = rest.find(']')?;
        let after = rest[end + 1..].trim();
        if after.is_empty() || after.starts_with('#') {
            Some(rest[..end].trim())
        } else {
            None
        }
    }

    // Cola de comentario de una línea `clave = valor …` respetando comillas.
    fn line_comment(after_equals: &str) -> &str {
        let mut quote: Option<char> = None;
        for (i, c) in after_equals.char_indices() {
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None => match c {
                    '"' | '\'' => quote = Some(c),
                    '#' => return after_equals[i..].trim_end(),
                    _ => {}
                },
            }
        }
        ""
    }

    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();

    let Some(start) = lines
        .iter()
        .position(|l| header_section(l) == Some(section))
    else {
        // Sin sección: agregarla al final, sin tocar lo existente.
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(&format!("[{section}]{eol}{key} = {value}{eol}"));
        return out;
    };

    // Fin de la sección: la próxima cabecera (de cualquier sección).
    let end = lines
        .iter()
        .skip(start + 1)
        .position(|l| header_section(l).is_some())
        .map(|offset| start + 1 + offset)
        .unwrap_or(lines.len());

    // ¿Ya está la clave dentro de la sección?
    for i in (start + 1)..end {
        let line = &lines[i];
        let t = line.trim_start();
        if t.starts_with('#') {
            continue;
        }
        let Some(rest) = t.strip_prefix(key) else {
            continue;
        };
        let Some(after) = rest.trim_start().strip_prefix('=') else {
            // «names», «name2», etc.: no es la clave.
            continue;
        };
        let indent = &line[..line.len() - t.len()];
        let comment = line_comment(after);
        let mut new_line = format!("{indent}{key} = {value}");
        if !comment.is_empty() {
            new_line.push(' ');
            new_line.push_str(comment);
        }
        if line.ends_with('\r') {
            new_line.push('\r');
        }
        lines[i] = new_line;
        return lines.join("\n");
    }

    // Clave ausente: insertarla justo debajo del encabezado.
    let mut inserted = format!("{key} = {value}");
    if crlf {
        inserted.push('\r');
    }
    lines.insert(start + 1, inserted);
    lines.join("\n")
}

/// Escribe `key = value` en `[section]` de `path` de forma atómica (archivo
/// temporal + rename). Crea el archivo (y su directorio) si no existe. Nunca
/// toca la config real: las pruebas usan rutas temporales vía esta función.
///
/// `label` es cómo se nombra el valor en los mensajes de error ("el tema",
/// "el mouse"). El resultado se reparsea antes de escribir: el editor por
/// líneas solo entiende `[section]` + `key =`, y ante otras formas válidas
/// (tabla inline, claves con punto o entre comillas) podría duplicar claves y
/// dejar la config ilegible, así que si no queda TOML válido con el valor
/// pedido no se toca el archivo.
fn persist_key_at(
    path: &Path,
    section: &str,
    key: &str,
    label: &str,
    value: ConfigValue<'_>,
) -> Result<(), String> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(format!("no se pudo leer '{}': {err}", path.display()));
        }
    };
    let updated = edit_section_key(&existing, section, key, &value.render());

    let persisted = updated
        .parse::<toml::Table>()
        .ok()
        .and_then(|table| {
            table
                .get(section)?
                .get(key)
                .map(|saved| value.matches(saved))
        })
        .unwrap_or(false);
    if !persisted {
        return Err(format!(
            "no se pudo guardar {label} sin romper '{}'; agregá `{key} = {}` en [{section}] a mano",
            path.display(),
            value.render()
        ));
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(err) = fs::create_dir_all(parent)
    {
        return Err(format!("no se pudo guardar {label}: {err}"));
    }

    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    if let Err(err) = fs::write(&tmp, updated.as_bytes()) {
        return Err(format!("no se pudo guardar {label}: {err}"));
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(format!("no se pudo guardar {label}: {err}"))
        }
    }
}

// Persistencia del tema
// ---------------------------------------------------------------------------

/// Actualiza (o inserta) solo la clave `name` dentro de `[theme]` de un texto
/// TOML, preservando el resto — comentarios, otras claves, otras secciones y
/// los finales de línea CRLF. Sin sección `[theme]`, la agrega al final.
/// Solo para tests: fachada tipada sobre [`edit_section_key`].
#[cfg(test)]
pub fn update_theme_name(text: &str, name: &str) -> String {
    edit_section_key(text, "theme", "name", &ConfigValue::Str(name).render())
}

/// Escribe el `name` del tema en `path` de forma atómica (archivo temporal +
/// rename). Crea el archivo (y su directorio) si no existe. Nunca toca la
/// config real: las pruebas usan rutas temporales vía esta función.
pub(crate) fn persist_theme_name_at(path: &Path, name: &str) -> Result<(), String> {
    persist_key_at(path, "theme", "name", "el tema", ConfigValue::Str(name))
}

/// Persiste el id del tema en la config del usuario (ruta por defecto o
/// `MP3TUI_CONFIG`).
pub fn persist_theme_name(name: &str) -> Result<(), String> {
    let Some((path, _)) = resolve_path() else {
        return Err("no se pudo guardar el tema: sin ruta de config".to_string());
    };
    persist_theme_name_at(&path, name)
}

// ---------------------------------------------------------------------------
// Keybindings
// ---------------------------------------------------------------------------

/// Atajos por acción. Cada acción acepta una o varias teclas (se comparan
/// sin distinguir mayúsculas para las letras).
#[derive(Debug, Clone, PartialEq)]
pub struct Keys {
    /// Salir de la app. En el explorador, Esc llega primero por `back` y
    /// cierra el panel (o sale) antes de que llegue acá.
    pub quit: Vec<KeyCode>,
    /// Espacio en ambos modos.
    pub play_pause: Vec<KeyCode>,
    pub volume_up: Vec<KeyCode>,
    pub volume_down: Vec<KeyCode>,
    pub seek_back: Vec<KeyCode>,
    pub seek_fwd: Vec<KeyCode>,
    pub prev_track: Vec<KeyCode>,
    pub next_track: Vec<KeyCode>,
    pub shuffle: Vec<KeyCode>,
    pub repeat: Vec<KeyCode>,
    /// Abrir la búsqueda incremental.
    pub search: Vec<KeyCode>,
    /// Abrir el buscador (Spotify, URLs y rutas locales). Por defecto 'u'.
    pub spotify_search: Vec<KeyCode>,
    /// Enfocar (y abrir) el árbol de playlists de Spotify en la vista de
    /// lista. Por defecto 'l'. Conserva el nombre `spotify_library` por
    /// compatibilidad con configs existentes.
    pub spotify_library: Vec<KeyCode>,
    /// Alternar entre la vista de lista (Locales │ Playlists │ Cola) y la
    /// vista Reproductor a pantalla completa. Por defecto 'v'.
    pub player_view: Vec<KeyCode>,
    /// Abrir el historial de playlists y álbumes de Spotify. Por defecto 'h'.
    pub playlist_history: Vec<KeyCode>,
    pub speed_up: Vec<KeyCode>,
    pub speed_down: Vec<KeyCode>,
    /// Abrir/cerrar el panel lateral del explorador: `b` / `Tab` /
    /// `Ctrl+Tab` (el dispatch ignora modificadores, así que las tres variantes
    /// caen en la misma acción).
    pub explorer: Vec<KeyCode>,
    pub move_up: Vec<KeyCode>,
    pub move_down: Vec<KeyCode>,
    pub page_up: Vec<KeyCode>,
    pub page_down: Vec<KeyCode>,
    pub home: Vec<KeyCode>,
    pub end: Vec<KeyCode>,
    /// Reproducir la pista seleccionada en el explorador.
    pub select: Vec<KeyCode>,
    /// Cerrar el panel lateral del explorador / volver al player; sin
    /// player en pantalla completa, sale de la app. Solo `Esc` por defecto
    /// `Tab` vive en `explorer` y acá salía de la app sin player.
    pub back: Vec<KeyCode>,
    /// Prompt de URL legado. Sin tecla por defecto: 'u' abre el buscador, que
    /// ya acepta URLs y rutas. Sigue funcionando si el usuario lo configura.
    pub open_url: Vec<KeyCode>,
    /// Encolar el ítem seleccionado de la Biblioteca (pista local o URL del
    /// historial) al final de la cola. Por defecto 'a'.
    pub enqueue: Vec<KeyCode>,
    /// Rotar el foco de la vista de lista entre Locales, Playlists y Cola.
    /// Por defecto 'c'.
    pub queue_panel: Vec<KeyCode>,
    /// Eliminar el ítem seleccionado de la cola. Por defecto 'd'.
    pub queue_delete: Vec<KeyCode>,
    /// Vaciar toda la cola sin cortar la reproducción en curso. Por defecto 'x'.
    pub clear_queue: Vec<KeyCode>,
    /// Abrir el panel de configuración (shell con filas Temas/Mouse/Bandas).
    /// Se dispara con Ctrl+tecla (por defecto `Ctrl+P`): a diferencia de
    /// `explorer` (donde Tab y Ctrl+Tab valen por igual porque el dispatch
    /// ignora modificadores), acá se exige Ctrl para no robarle la letra
    /// suelta a otra acción (`p` sigue siendo pista anterior).
    pub settings: Vec<KeyCode>,
}

impl Default for Keys {
    fn default() -> Self {
        fn keys(codes: &[KeyCode]) -> Vec<KeyCode> {
            codes.to_vec()
        }
        Self {
            quit: keys(&[KeyCode::Esc]),
            play_pause: keys(&[KeyCode::Char(' ')]),
            volume_up: keys(&[KeyCode::Up]),
            volume_down: keys(&[KeyCode::Down]),
            seek_back: keys(&[KeyCode::Left]),
            seek_fwd: keys(&[KeyCode::Right]),
            prev_track: keys(&[KeyCode::Char('p')]),
            next_track: keys(&[KeyCode::Char('n')]),
            shuffle: keys(&[KeyCode::Char('s')]),
            repeat: keys(&[KeyCode::Char('r')]),
            search: keys(&[KeyCode::Char('b')]),
            spotify_search: keys(&[KeyCode::Char('u')]),
            spotify_library: keys(&[KeyCode::Char('l')]),
            player_view: keys(&[KeyCode::Char('v')]),
            playlist_history: keys(&[KeyCode::Char('h')]),
            speed_up: keys(&[KeyCode::Char(']')]),
            speed_down: keys(&[KeyCode::Char('[')]),
            explorer: keys(&[KeyCode::Tab]),
            move_up: keys(&[KeyCode::Up]),
            move_down: keys(&[KeyCode::Down]),
            page_up: keys(&[KeyCode::PageUp]),
            page_down: keys(&[KeyCode::PageDown]),
            home: keys(&[KeyCode::Home]),
            end: keys(&[KeyCode::End]),
            select: keys(&[KeyCode::Enter]),
            back: keys(&[KeyCode::Esc]),
            open_url: keys(&[]),
            enqueue: keys(&[KeyCode::Char('a')]),
            queue_panel: keys(&[KeyCode::Char('c')]),
            queue_delete: keys(&[KeyCode::Char('d')]),
            clear_queue: keys(&[KeyCode::Char('x')]),
            settings: keys(&[KeyCode::Char('p')]),
        }
    }
}

fn parse_keybindings(section: &toml::Table, keys: &mut Keys, warnings: &mut Vec<String>) {
    for (key, value) in section {
        let field = match key.as_str() {
            "quit" => &mut keys.quit,
            "play_pause" => &mut keys.play_pause,
            "volume_up" => &mut keys.volume_up,
            "volume_down" => &mut keys.volume_down,
            "seek_back" => &mut keys.seek_back,
            "seek_fwd" => &mut keys.seek_fwd,
            "prev_track" => &mut keys.prev_track,
            "next_track" => &mut keys.next_track,
            "shuffle" => &mut keys.shuffle,
            "repeat" => &mut keys.repeat,
            "search" => &mut keys.search,
            "spotify_search" => &mut keys.spotify_search,
            "spotify_library" => &mut keys.spotify_library,
            "player_view" => &mut keys.player_view,
            "playlist_history" => &mut keys.playlist_history,
            "open_url" => &mut keys.open_url,
            "enqueue" => &mut keys.enqueue,
            "queue_panel" => &mut keys.queue_panel,
            "queue_delete" => &mut keys.queue_delete,
            "clear_queue" => &mut keys.clear_queue,
            "settings" => &mut keys.settings,
            "speed_up" => &mut keys.speed_up,
            "speed_down" => &mut keys.speed_down,
            "explorer" => &mut keys.explorer,
            "move_up" => &mut keys.move_up,
            "move_down" => &mut keys.move_down,
            "page_up" => &mut keys.page_up,
            "page_down" => &mut keys.page_down,
            "home" => &mut keys.home,
            "end" => &mut keys.end,
            "select" => &mut keys.select,
            "back" => &mut keys.back,
            other => {
                warnings.push(format!("[keybindings] acción desconocida «{other}»"));
                continue;
            }
        };
        let Some(raw) = value.as_str() else {
            warnings.push(format!(
                "[keybindings] {key}: se espera una tecla entre comillas, \
                 p. ej. \"space\" o \"q,esc\"; se mantiene el valor por defecto"
            ));
            continue;
        };
        match parse_key_list(raw) {
            Some(list) if !list.is_empty() => *field = list,
            Some(_) => warnings.push(format!(
                "[keybindings] {key}: lista vacía; se mantiene el valor por defecto"
            )),
            None => warnings.push(format!(
                "[keybindings] {key}: tecla «{raw}» no reconocida (válidas: \
                 un carácter, o enter/esc/tab/up/down/left/right/pageup/\
                 pagedown/home/end/backspace/space); \
                 se mantiene el valor por defecto"
            )),
        }
    }
}

/// Las letras comparan igual que en `pressed()`: sin distinguir mayúsculas.
fn normalize_key(key: KeyCode) -> KeyCode {
    match key {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        other => other,
    }
}

/// Resuelve colisiones de teclas entre acciones distintas y avisa con un
/// warning: el dispatch (por orden fijo) dejaría la acción perdedora
/// sombreada en silencio.
///
/// Se recorren las acciones en orden de prioridad del dispatch; gana la
/// primera y la perdedora pierde esa tecla (si queda sin teclas, vuelve a
/// sus defaults que estén libres). Las colisiones heredadas de los defaults
/// se dejan como están.
fn resolve_key_collisions(keys: &mut Keys, warnings: &mut Vec<String>) {
    let defaults = Keys::default();

    // (acción, teclas actuales, teclas por defecto) en orden de prioridad.
    let mut entries: Vec<(&'static str, &mut Vec<KeyCode>, &[KeyCode])> = vec![
        ("back", &mut keys.back, &defaults.back),
        ("quit", &mut keys.quit, &defaults.quit),
        ("select", &mut keys.select, &defaults.select),
        ("play_pause", &mut keys.play_pause, &defaults.play_pause),
        ("volume_up", &mut keys.volume_up, &defaults.volume_up),
        ("volume_down", &mut keys.volume_down, &defaults.volume_down),
        ("seek_back", &mut keys.seek_back, &defaults.seek_back),
        ("seek_fwd", &mut keys.seek_fwd, &defaults.seek_fwd),
        ("speed_down", &mut keys.speed_down, &defaults.speed_down),
        ("speed_up", &mut keys.speed_up, &defaults.speed_up),
        ("explorer", &mut keys.explorer, &defaults.explorer),
        ("next_track", &mut keys.next_track, &defaults.next_track),
        ("prev_track", &mut keys.prev_track, &defaults.prev_track),
        ("shuffle", &mut keys.shuffle, &defaults.shuffle),
        ("repeat", &mut keys.repeat, &defaults.repeat),
        ("search", &mut keys.search, &defaults.search),
        (
            "spotify_search",
            &mut keys.spotify_search,
            &defaults.spotify_search,
        ),
        (
            "spotify_library",
            &mut keys.spotify_library,
            &defaults.spotify_library,
        ),
        (
            "playlist_history",
            &mut keys.playlist_history,
            &defaults.playlist_history,
        ),
        ("player_view", &mut keys.player_view, &defaults.player_view),
        ("open_url", &mut keys.open_url, &defaults.open_url),
        ("enqueue", &mut keys.enqueue, &defaults.enqueue),
        ("queue_panel", &mut keys.queue_panel, &defaults.queue_panel),
        (
            "queue_delete",
            &mut keys.queue_delete,
            &defaults.queue_delete,
        ),
        (
            "clear_queue",
            &mut keys.clear_queue,
            &defaults.clear_queue,
        ),
        ("settings", &mut keys.settings, &defaults.settings),
        ("move_up", &mut keys.move_up, &defaults.move_up),
        ("move_down", &mut keys.move_down, &defaults.move_down),
        ("page_up", &mut keys.page_up, &defaults.page_up),
        ("page_down", &mut keys.page_down, &defaults.page_down),
        ("home", &mut keys.home, &defaults.home),
        ("end", &mut keys.end, &defaults.end),
    ];

    // Tecla (normalizada) -> (acción que la tomó, sus defaults).
    let mut owned: Vec<(KeyCode, &'static str, &[KeyCode])> = Vec::new();
    // (perdedora, tecla perdida, ganador) en orden de aparición.
    let mut lost: Vec<(&'static str, KeyCode, &'static str)> = Vec::new();
    // Acciones que la colisión dejó sin teclas (candidatas a restauración).
    let mut emptied_actions: Vec<&'static str> = Vec::new();

    for (name, list, default) in entries.iter_mut() {
        let name = *name;
        let mut kept = Vec::new();
        for key in list.iter().copied() {
            let norm = normalize_key(key);
            let clash = owned.iter().find(|(k, _, _)| *k == norm);
            match clash {
                Some((_, winner, _)) if *winner == name => {
                    // Misma tecla repetida dentro de la misma acción.
                }
                Some((_, winner, winner_default)) => {
                    let inherited = winner_default.contains(&key) && default.contains(&key);
                    if inherited {
                        // Colisión heredada de los defaults: el orden del
                        // dispatch ya la resuelve; se conserva tal cual.
                        kept.push(key);
                    } else {
                        lost.push((name, key, winner));
                    }
                }
                None => {
                    owned.push((norm, name, default));
                    kept.push(key);
                }
            }
        }
        let emptied = kept.is_empty();
        list.clear();
        list.extend(kept.iter().copied());
        if emptied {
            emptied_actions.push(name);
        }
    }

    // Las que quedaron sin teclas vuelven a sus defaults que estén libres.
    for (name, list, default) in entries.iter_mut() {
        if !emptied_actions.contains(name) {
            continue;
        }
        let restored: Vec<KeyCode> = default
            .iter()
            .copied()
            .filter(|key| !owned.iter().any(|(k, _, _)| *k == normalize_key(*key)))
            .collect();
        if restored.is_empty() {
            continue;
        }
        for key in &restored {
            owned.push((normalize_key(*key), *name, default));
        }
        list.extend(restored.iter().copied());
    }

    // Un warning por acción perdedora, agrupando sus teclas perdidas.
    let mut order: Vec<&'static str> = Vec::new();
    let mut groups: Vec<(&'static str, Vec<(KeyCode, &'static str)>)> = Vec::new();
    for (loser, key, winner) in &lost {
        if !order.contains(loser) {
            order.push(loser);
            groups.push((loser, Vec::new()));
        }
        let group = groups
            .iter_mut()
            .find(|(name, _)| name == loser)
            .expect("grupo recién creado");
        group.1.push((*key, *winner));
    }

    for loser in order {
        let pairs = groups
            .iter()
            .find(|(name, _)| *name == loser)
            .expect("grupo registrado")
            .1
            .clone();
        let teclas = pairs
            .iter()
            .map(|(key, _)| format!("«{}»", key_labels(&[*key])))
            .collect::<Vec<_>>()
            .join(" y ");
        let mut ganadores: Vec<&str> = Vec::new();
        for (_, winner) in &pairs {
            if !ganadores.contains(winner) {
                ganadores.push(winner);
            }
        }
        let (final_keys, emptied) = match entries.iter().find(|(name, _, _)| *name == loser) {
            Some((name, list, _)) => ((**list).clone(), emptied_actions.contains(name)),
            None => (Vec::new(), false),
        };
        let destino = if final_keys.is_empty() {
            "queda sin ninguna tecla asignada".to_string()
        } else if emptied {
            format!(
                "vuelve a sus teclas por defecto ({})",
                key_labels(&final_keys)
            )
        } else {
            format!("conserva sus otras teclas ({})", key_labels(&final_keys))
        };
        warnings.push(format!(
            "[keybindings] {loser} pierde {teclas} frente a {} \
             (prioridad de dispatch); {destino}",
            ganadores.join(" y ")
        ));
    }
}

/// «q,esc» → `['q', Esc]`. `None` si alguna entrada no se reconoce.
fn parse_key_list(raw: &str) -> Option<Vec<KeyCode>> {
    raw.split(',').map(parse_key).collect()
}

fn parse_key(raw: &str) -> Option<KeyCode> {
    let value = raw.trim();
    let code = match value.to_ascii_lowercase().as_str() {
        "space" | "espacio" => KeyCode::Char(' '),
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "up" | "arriba" => KeyCode::Up,
        "down" | "abajo" => KeyCode::Down,
        "left" | "izquierda" => KeyCode::Left,
        "right" | "derecha" => KeyCode::Right,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "backspace" => KeyCode::Backspace,
        "delete" | "supr" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "comma" => KeyCode::Char(','),
        "minus" => KeyCode::Char('-'),
        "plus" => KeyCode::Char('+'),
        _ => {
            let mut chars = value.chars();
            let single = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(single)
        }
    };
    Some(code)
}

/// ¿`code` coincide con alguna de las teclas de la acción? Las letras
/// comparan sin distinguir mayúsculas (hoy 'q' y 'Q' salen igual).
pub fn pressed(bindings: &[KeyCode], code: KeyCode) -> bool {
    bindings.iter().any(|binding| match (*binding, code) {
        (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(&b),
        (binding, code) => binding == code,
    })
}

/// ¿`key` dispara un atajo `Ctrl+...` (`settings`, por defecto Ctrl+P)? A
/// diferencia de `pressed` —que ignora modificadores, como el Ctrl+Tab que cae
/// en `explorer`—, exige Ctrl: la letra suelta sigue siendo de su acción
/// (`p` = pista anterior).
pub fn pressed_ctrl(bindings: &[KeyCode], key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && pressed(bindings, key.code)
}

// ---------------------------------------------------------------------------
// Biblioteca
// ---------------------------------------------------------------------------

/// Carpetas a escanear cuando la CLI no recibe ningún path. El
/// set final ([`LibraryConfig::scan_roots`]) une las carpetas conocidas del
/// sistema con las configuradas a mano; los tracks quedan agrupados por
/// carpeta de origen en la lista del explorador.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LibraryConfig {
    /// Raíces escritas por el usuario en `[library] roots`: se AGREGAN a las
    /// carpetas conocidas, no las reemplazan. Vacío = solo conocidas.
    pub roots: Vec<PathBuf>,
}

/// Sección `[spotify]` del config.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SpotifyConfig {
    /// `client_id` de la app registrada por el usuario en el dashboard de
    /// Spotify (PKCE no usa secret). `MP3TUI_SPOTIFY_CLIENT_ID` tiene prioridad.
    pub client_id: Option<String>,
}

/// Carpetas conocidas típicas bajo el home: la biblioteca por defecto
/// cubre las cuatro sin que el usuario configure nada.
const KNOWN_DIR_NAMES: [&str; 4] = ["Music", "Downloads", "Desktop", "Documents"];

/// Home del usuario: `%USERPROFILE%` en Windows, `$HOME` en el resto.
fn home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .filter(|v| !v.is_empty())
        .or_else(|| env::var_os("HOME").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

/// Carpeta que XDG usa para la config (`$XDG_CONFIG_HOME` o `~/.config`):
/// ahí vive `user-dirs.dirs`.
fn xdg_config_home(home: &Path) -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
}

/// Solo las carpetas conocidas que existen bajo `home`: una faltante se
/// saltea en silencio, porque es un default automático y no una ruta
/// configurada a mano (esas sí avisan en `parse_library`). Respeta los
/// nombres localizados que declare `user-dirs.dirs`.
fn known_dirs(home: &Path) -> Vec<PathBuf> {
    known_dirs_from(home, &xdg_config_home(home))
}

/// [`known_dirs`] con la carpeta de config XDG ya resuelta por el caller
/// (las pruebas la apuntan a un home temporal, sin depender del entorno).
fn known_dirs_from(home: &Path, config_home: &Path) -> Vec<PathBuf> {
    let xdg = xdg_user_dirs(config_home, home);
    KNOWN_DIR_NAMES
        .iter()
        .map(|name| xdg.get(*name).cloned().unwrap_or_else(|| home.join(name)))
        .filter(|path| path.is_dir())
        .collect()
}

/// Lee `user-dirs.dirs` (XDG) y devuelve las carpetas conocidas que define,
/// con `$HOME`/`${HOME}` ya expandidos. Vacío si el archivo no existe o no
/// define nada: el caller cae entonces a los nombres en inglés.
fn xdg_user_dirs(
    config_home: &Path,
    home: &Path,
) -> std::collections::HashMap<&'static str, PathBuf> {
    use std::collections::HashMap;

    // Clave de `user-dirs.dirs` → nombre de KNOWN_DIR_NAMES.
    const KEYS: [(&str, &str); 4] = [
        ("XDG_MUSIC_DIR", "Music"),
        ("XDG_DOWNLOAD_DIR", "Downloads"),
        ("XDG_DESKTOP_DIR", "Desktop"),
        ("XDG_DOCUMENTS_DIR", "Documents"),
    ];

    let Ok(text) = fs::read_to_string(config_home.join("user-dirs.dirs")) else {
        return HashMap::new();
    };

    let home_str = home.to_string_lossy();
    let mut dirs = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let Some((_, name)) = KEYS.iter().find(|(k, _)| *k == key.trim()) else {
            continue;
        };
        let raw = raw.trim().trim_matches('"');
        if raw.is_empty() {
            continue;
        }
        let expanded = raw
            .replace("${HOME}", &home_str)
            .replace("$HOME", &home_str);
        let path = PathBuf::from(expanded);
        let path = if path.is_absolute() {
            path
        } else {
            home.join(path)
        };
        dirs.insert(*name, path);
    }
    dirs
}

impl LibraryConfig {
    /// Set final a escanear sin argumento de CLI: las carpetas
    /// conocidas que existen en el sistema, unidas con las configuradas,
    /// sin duplicados y en ese orden (las conocidas primero). Si queda vacío
    /// (sin home y sin roots) se usa el `cwd`.
    pub fn scan_roots(&self) -> Vec<PathBuf> {
        let home = home_dir();
        self.scan_roots_with(home.as_deref())
    }

    /// [`Self::scan_roots`] con el home resuelto por el caller: las pruebas
    /// pasan un home temporal para no depender del perfil de la máquina.
    fn scan_roots_with(&self, home: Option<&Path>) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        let known = home.map(known_dirs).unwrap_or_default();
        for path in known.into_iter().chain(self.roots.iter().cloned()) {
            if !roots.contains(&path) {
                roots.push(path);
            }
        }
        if roots.is_empty() {
            roots.push(env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        }
        roots
    }
}

/// Estilo de bandas del visualizador (`[visualizer] style`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VisualizerStyle {
    /// Según la terminal: con `WT_SESSION` = unicode, sin ella = ascii.
    #[default]
    Auto,
    Ascii,
    Unicode,
}

impl VisualizerStyle {
    /// Valor que `[visualizer] style` acepta en config.toml.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Ascii => "ascii",
            Self::Unicode => "unicode",
        }
    }

    /// Los 3 valores en orden estable de menú (sub-selector de Bandas).
    pub fn options() -> [Self; 3] {
        [Self::Auto, Self::Ascii, Self::Unicode]
    }
}

/// Resuelve el estilo efectivo de forma pura (testeable): `ascii`/`unicode`
/// fuerzan; `auto` mira `WT_SESSION` (inyectada, NO leída acá).
pub fn resolve_visualizer_style(
    configured: VisualizerStyle,
    wt_session: Option<&str>,
) -> VisualizerStyle {
    match configured {
        VisualizerStyle::Auto => match wt_session {
            Some(session) if !session.is_empty() => VisualizerStyle::Unicode,
            _ => VisualizerStyle::Ascii,
        },
        forced => forced,
    }
}

// ---------------------------------------------------------------------------
// Persistencia del estilo de bandas
// ---------------------------------------------------------------------------

/// Actualiza (o inserta) solo la clave `style` dentro de `[visualizer]` de un
/// texto TOML, preservando el resto — comentarios, otras claves, otras
/// secciones y los finales de línea CRLF. Sin sección `[visualizer]`, la
/// agrega al final.
/// Solo para tests: fachada tipada sobre [`edit_section_key`].
#[cfg(test)]
pub fn update_visualizer_style(text: &str, style: &str) -> String {
    edit_section_key(
        text,
        "visualizer",
        "style",
        &ConfigValue::Str(style).render(),
    )
}

/// Escribe el `style` en `path` de forma atómica (archivo temporal +
/// rename). Crea el archivo (y su directorio) si no existe. Nunca toca la
/// config real: las pruebas usan rutas temporales vía esta función.
pub(crate) fn persist_visualizer_style_at(path: &Path, style: &str) -> Result<(), String> {
    persist_key_at(
        path,
        "visualizer",
        "style",
        "el estilo",
        ConfigValue::Str(style),
    )
}

/// Persiste el estilo en la config del usuario (ruta por defecto o
/// `MP3TUI_CONFIG`).
pub fn persist_visualizer_style(style: &str) -> Result<(), String> {
    let Some((path, _)) = resolve_path() else {
        return Err("no se pudo guardar el estilo: sin ruta de config".to_string());
    };
    persist_visualizer_style_at(&path, style)
}

// ---------------------------------------------------------------------------
// Modo del visualizador
// ---------------------------------------------------------------------------

/// Modo de dibujo del visualizador (`[visualizer] mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VisualizerMode {
    /// Las 32 barras estilo cava (comportamiento actual).
    #[default]
    Barras,
    /// Osciloscopio con muestras crudas del tap.
    Onda,
    /// 32 bandas arriba+abajo desde el centro.
    Espejo,
    /// Medidor RMS, mono como el tap actual.
    Vu,
    /// Plot polar grueso de las 32 bandas.
    Circular,
    /// Bailarina ASCII (tema miku): poses guiadas por la energía de graves.
    Miku,
}

impl VisualizerMode {
    /// Valor que `[visualizer] mode` acepta en config.toml.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Barras => "barras",
            Self::Onda => "onda",
            Self::Espejo => "espejo",
            Self::Vu => "vu",
            Self::Circular => "circular",
            Self::Miku => "miku",
        }
    }

    /// Los 6 valores en orden estable de menú (sub-selector de Modo).
    pub fn options() -> [Self; 6] {
        [
            Self::Barras,
            Self::Onda,
            Self::Espejo,
            Self::Vu,
            Self::Circular,
            Self::Miku,
        ]
    }
}

// ---------------------------------------------------------------------------
// Persistencia del modo visualizador
// ---------------------------------------------------------------------------

/// Actualiza (o inserta) solo la clave `mode` dentro de `[visualizer]` de un
/// texto TOML, preservando el resto — comentarios, otras claves, otras
/// secciones y los finales de línea CRLF. Sin sección `[visualizer]`, la
/// agrega al final.
/// Solo para tests: fachada tipada sobre [`edit_section_key`].
#[cfg(test)]
pub fn update_visualizer_mode(text: &str, mode: &str) -> String {
    edit_section_key(text, "visualizer", "mode", &ConfigValue::Str(mode).render())
}

/// Escribe el `mode` en `path` de forma atómica (archivo temporal +
/// rename). Crea el archivo (y su directorio) si no existe. Nunca toca la
/// config real: las pruebas usan rutas temporales vía esta función.
pub(crate) fn persist_visualizer_mode_at(path: &Path, mode: &str) -> Result<(), String> {
    persist_key_at(
        path,
        "visualizer",
        "mode",
        "el modo",
        ConfigValue::Str(mode),
    )
}

/// Persiste el modo en la config del usuario (ruta por defecto o
/// `MP3TUI_CONFIG`).
pub fn persist_visualizer_mode(mode: &str) -> Result<(), String> {
    let Some((path, _)) = resolve_path() else {
        return Err("no se pudo guardar el modo: sin ruta de config".to_string());
    };
    persist_visualizer_mode_at(&path, mode)
}

/// Autocorrige `path` cuando `[visualizer] mode` traía el valor eliminado
/// `caida`: reescribe `mode` a `barras` con la persistencia línea-a-línea
/// existente (`persist_visualizer_mode_at`), la misma que usa el sub-selector
/// del panel. Solo lo llama `load()`, nunca `parse()` (puro, sin tocar
/// archivos). Best-effort: un fallo al escribir se agrega a `warnings` en
/// vez de entrar en pánico; sin reintentos.
fn migrate_removed_visualizer_mode_at(path: &Path, warnings: &mut Vec<String>) {
    if let Err(err) = persist_visualizer_mode_at(path, VisualizerMode::Barras.as_str()) {
        warnings.push(format!(
            "no se pudo corregir automáticamente [visualizer] mode \
             («caida» → «barras») en '{}': {err}",
            path.display()
        ));
    }
}

// ---------------------------------------------------------------------------
// Persistencia del toggle de mouse
// ---------------------------------------------------------------------------

/// Actualiza (o inserta) solo la clave `mouse` dentro de `[ui]` de un texto
/// TOML, preservando el resto — comentarios, otras claves, otras secciones y
/// los finales de línea CRLF. Sin sección `[ui]`, la agrega al final. Misma
/// receta que `update_visualizer_style` pero con valor bool sin comillas.
/// Solo para tests: fachada tipada sobre [`edit_section_key`].
#[cfg(test)]
pub fn update_ui_mouse(text: &str, enabled: bool) -> String {
    edit_section_key(text, "ui", "mouse", &ConfigValue::Bool(enabled).render())
}

/// Escribe el estado del mouse en `path` de forma atómica (archivo temporal +
/// rename). Crea el archivo (y su directorio) si no existe. Nunca toca la
/// config real: las pruebas usan rutas temporales vía esta función.
pub(crate) fn persist_mouse_enabled_at(path: &Path, enabled: bool) -> Result<(), String> {
    persist_key_at(path, "ui", "mouse", "el mouse", ConfigValue::Bool(enabled))
}

/// Persiste el estado del mouse en la config del usuario (ruta por defecto o
/// `MP3TUI_CONFIG`).
pub fn persist_mouse_enabled(enabled: bool) -> Result<(), String> {
    let Some((path, _)) = resolve_path() else {
        return Err("no se pudo guardar el mouse: sin ruta de config".to_string());
    };
    persist_mouse_enabled_at(&path, enabled)
}

/// `[visualizer]`: `style` (auto/ascii/unicode) y `mode`, derivado de
/// `VisualizerMode::options()` (sin lista de valores hardcodeada en el
/// match ni en el aviso). El valor eliminado `caida` avisa específicamente
/// y cae a `barras` (`load()` migra la config real, ver
/// `migrate_removed_visualizer_mode_at`). Merge campo a campo como el
/// resto: lo inválido avisa y cae al default sin tocar lo demás.
fn parse_visualizer(section: &toml::Table, config: &mut Config, warnings: &mut Vec<String>) {
    for (key, value) in section {
        match key.as_str() {
            "style" => match value.as_str() {
                Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                    "auto" => config.visualizer_style = VisualizerStyle::Auto,
                    "ascii" => config.visualizer_style = VisualizerStyle::Ascii,
                    "unicode" => config.visualizer_style = VisualizerStyle::Unicode,
                    _ => warnings.push(format!(
                        "[visualizer] style «{raw}» no reconocido \
                             (válidos: auto, ascii, unicode); se usa «auto»"
                    )),
                },
                None => warnings.push(
                    "[visualizer] style debe ser un texto entre comillas, \
                     p. ej. style = \"ascii\"; se usa «auto»"
                        .to_string(),
                ),
            },
            "mode" => match value.as_str() {
                Some(raw) => {
                    let normalized = raw.trim().to_ascii_lowercase();
                    // La clave existe (aunque el valor sea inválido): un modo
                    // explícito gana sobre el default del tema.
                    config.visualizer_mode_explicit = true;
                    match VisualizerMode::options()
                        .into_iter()
                        .find(|mode| mode.as_str() == normalized)
                    {
                        Some(mode) => config.visualizer_mode = mode,
                        None if normalized == "caida" => {
                            config.visualizer_mode = VisualizerMode::Barras;
                            config.removed_visualizer_mode = true;
                            warnings.push(format!(
                                "[visualizer] mode «{raw}»: el modo «caida» \
                                 fue eliminado; se usa «barras»"
                            ));
                        }
                        None => {
                            let valid = VisualizerMode::options()
                                .iter()
                                .map(|mode| mode.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            warnings.push(format!(
                                "[visualizer] mode «{raw}» no reconocido \
                                 (válidos: {valid}); se usa «barras»"
                            ));
                        }
                    }
                }
                None => {
                    // La clave existe: cuenta como modo explícito.
                    config.visualizer_mode_explicit = true;
                    warnings.push(
                        "[visualizer] mode debe ser un texto entre comillas, \
                         p. ej. mode = \"barras\"; se usa «barras»"
                            .to_string(),
                    );
                }
            },
            other => warnings.push(format!(
                "[visualizer] clave desconocida «{other}» (se ignora)"
            )),
        }
    }
}

/// `[ui]`: opciones de la interfaz. Hoy solo `mouse` (bool).
fn parse_ui(section: &toml::Table, config: &mut Config, warnings: &mut Vec<String>) {
    for (key, value) in section {
        match key.as_str() {
            "mouse" => match value.as_bool() {
                Some(enabled) => config.mouse = enabled,
                None => {
                    warnings.push("[ui] mouse debe ser true o false; se deja activado".to_string())
                }
            },
            other => warnings.push(format!("[ui] clave desconocida «{other}» (se ignora)")),
        }
    }
}

fn parse_library(section: &toml::Table, library: &mut LibraryConfig, warnings: &mut Vec<String>) {
    for (key, value) in section {
        match key.as_str() {
            "roots" => match value.as_array() {
                Some(items) => {
                    let mut roots = Vec::new();
                    for item in items {
                        let Some(text) = item.as_str() else {
                            warnings.push(
                                "[library] roots: cada entrada debe ser una \
                                 ruta entre comillas; se ignora"
                                    .to_string(),
                            );
                            continue;
                        };
                        if text.trim().is_empty() {
                            warnings.push("[library] roots: ruta vacía; se ignora".to_string());
                            continue;
                        }
                        let path = PathBuf::from(text);
                        if !path.is_dir() {
                            warnings.push(format!(
                                "[library] «{}» no es una carpeta existente \
                                 (se listará vacía)",
                                path.display()
                            ));
                        }
                        roots.push(path);
                    }
                    // Lista vacía o todo inválido: no hay roots propios y
                    // la biblioteca queda en las carpetas conocidas.
                    if !roots.is_empty() {
                        library.roots = roots;
                    }
                }
                None => warnings.push(
                    "[library] roots debe ser una lista de rutas, p. ej. \
                     roots = [\"C:/Musica\"]"
                        .to_string(),
                ),
            },
            other => warnings.push(format!("[library] clave desconocida «{other}»")),
        }
    }
}

fn parse_spotify(section: &toml::Table, spotify: &mut SpotifyConfig, warnings: &mut Vec<String>) {
    for (key, value) in section {
        match key.as_str() {
            "client_id" => match value.as_str().map(str::trim) {
                Some(id) if !id.is_empty() => spotify.client_id = Some(id.to_string()),
                Some(_) => warnings.push("[spotify] client_id está vacío; se ignora".to_string()),
                None => warnings.push(
                    "[spotify] client_id debe ser un texto entre comillas".to_string(),
                ),
            },
            other => warnings.push(format!("[spotify] clave desconocida «{other}»")),
        }
    }
}

// ---------------------------------------------------------------------------
// Etiquetas para el pie de atajos
// ---------------------------------------------------------------------------

/// Nombre de la tecla para la UI (castellano, como el resto de la interfaz).
pub fn key_label(code: KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "espacio".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "Shift+Tab".to_string(),
        KeyCode::Up => "↑".to_string(),
        KeyCode::Down => "↓".to_string(),
        KeyCode::Left => "←".to_string(),
        KeyCode::Right => "→".to_string(),
        KeyCode::PageUp => "PgUp".to_string(),
        KeyCode::PageDown => "PgDn".to_string(),
        KeyCode::Home => "Inicio".to_string(),
        KeyCode::End => "Fin".to_string(),
        KeyCode::Backspace => "Retroceso".to_string(),
        KeyCode::Delete => "Supr".to_string(),
        KeyCode::Insert => "Ins".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Null => "?".to_string(),
        other => format!("{other:?}"),
    }
}

/// Varias teclas unidas con «/» («q/Esc», «b/Tab», «n/p»…).
pub fn key_labels(codes: &[KeyCode]) -> String {
    codes
        .iter()
        .map(|&code| key_label(code))
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_mouse_defaults_on_and_can_be_disabled() {
        assert!(parse("").mouse, "mouse activado por defecto");
        let off = parse(
            "[ui]
mouse = false",
        );
        assert!(!off.mouse);
        assert!(off.warnings.is_empty(), "{:?}", off.warnings);
    }

    #[test]
    fn ui_mouse_invalid_values_warn_and_keep_default() {
        let bad = parse(
            "[ui]
mouse = \"no\"",
        );
        assert!(bad.mouse, "un valor inválido no desactiva el mouse");
        assert_eq!(bad.warnings.len(), 1, "{:?}", bad.warnings);
        assert!(bad.warnings[0].contains("mouse"), "{:?}", bad.warnings);

        let unknown = parse(
            "[ui]
zoom = 2",
        );
        assert_eq!(unknown.warnings.len(), 1, "{:?}", unknown.warnings);
        assert!(
            unknown.warnings[0].contains("zoom"),
            "{:?}",
            unknown.warnings
        );
    }

    #[test]
    fn visualizer_style_defaults_to_auto_and_parses_all_values() {
        assert_eq!(parse("").visualizer_style, VisualizerStyle::Auto);
        for (raw, expected) in [
            ("auto", VisualizerStyle::Auto),
            ("ascii", VisualizerStyle::Ascii),
            ("unicode", VisualizerStyle::Unicode),
        ] {
            let config = parse(&format!("[visualizer]\nstyle = \"{raw}\"\n"));
            assert_eq!(config.visualizer_style, expected, "style={raw}");
            assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        }
    }

    #[test]
    fn visualizer_style_invalid_or_unknown_warns_and_keeps_default() {
        let bad = parse("[visualizer]\nstyle = \"bloques\"");
        assert_eq!(bad.visualizer_style, VisualizerStyle::Auto);
        assert_eq!(bad.warnings.len(), 1, "{:?}", bad.warnings);
        assert!(bad.warnings[0].contains("style"), "{:?}", bad.warnings);

        let not_a_string = parse("[visualizer]\nstyle = 42");
        assert_eq!(not_a_string.visualizer_style, VisualizerStyle::Auto);
        assert_eq!(
            not_a_string.warnings.len(),
            1,
            "{:?}",
            not_a_string.warnings
        );

        let unknown = parse("[visualizer]\nzoom = 2");
        assert_eq!(unknown.visualizer_style, VisualizerStyle::Auto);
        assert_eq!(unknown.warnings.len(), 1, "{:?}", unknown.warnings);
        assert!(
            unknown.warnings[0].contains("zoom"),
            "{:?}",
            unknown.warnings
        );
    }

    #[test]
    fn visualizer_style_does_not_drag_other_fields() {
        let config = parse("[visualizer]\nstyle = \"ascii\"\n[ui]\nmouse = false\n");
        assert_eq!(config.visualizer_style, VisualizerStyle::Ascii);
        assert!(!config.mouse);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn visualizer_auto_resolves_by_wt_session_and_forced_modes_win() {
        use VisualizerStyle::{Ascii, Auto, Unicode};
        assert_eq!(resolve_visualizer_style(Auto, None), Ascii);
        assert_eq!(resolve_visualizer_style(Auto, Some(""),), Ascii);
        assert_eq!(resolve_visualizer_style(Auto, Some("6e5b..."),), Unicode);
        assert_eq!(resolve_visualizer_style(Ascii, Some("6e5b..."),), Ascii);
        assert_eq!(resolve_visualizer_style(Unicode, None), Unicode);
    }

    #[test]
    fn default_keys_use_b_search_tab_explorer_and_esc_only_to_quit() {
        let keys = Keys::default();
        assert_eq!(keys.search, vec![KeyCode::Char('b')], "b busca");
        assert_eq!(
            keys.explorer,
            vec![KeyCode::Tab],
            "Tab abre/cierra el explorador"
        );
        assert_eq!(
            keys.quit,
            vec![KeyCode::Esc],
            "solo Esc sale (doble pulsación)"
        );
    }

    #[test]
    fn default_spotify_search_key_is_u_and_can_be_remapped() {
        assert_eq!(Keys::default().spotify_search, vec![KeyCode::Char('u')]);
        let config = parse(
            r#"
            [keybindings]
            spotify_search = "f"
            "#,
        );
        assert_eq!(config.keys.spotify_search, vec![KeyCode::Char('f')]);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn default_keys_have_no_collisions_warning() {
        // Los defaults nuevos no generan avisos de colisión (Esc compartido
        // entre «back» y «quit» es la colisión heredada permitida).
        let config = parse("");
        assert!(
            config.warnings.is_empty(),
            "sin avisos con la config vacía: {:?}",
            config.warnings
        );
    }

    #[test]
    fn rose_pine_uses_main_variant_palette() {
        // rose-pine/palette, variante principal (no Moon ni Dawn).
        let theme = builtin_theme("rose-pine").expect("rose-pine existe");
        assert_eq!(
            theme.select_bg,
            Color::Rgb(0x40, 0x3D, 0x52),
            "highlightMed"
        );
        assert_eq!(theme.spectrum_high, Color::Rgb(0xEB, 0xBC, 0xBA), "rose");
    }

    #[test]
    fn empty_text_gives_defaults() {
        let config = parse("");
        assert_eq!(config, Config::default());
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn full_file_parses_colors_and_keys() {
        let config = parse(
            r##"
            [theme]
            accent = "#FF00FF"
            spectrum_low = "azul"
            text = "#101010"

            [keybindings]
            quit = "q"
            play_pause = "space,plus"
            seek_fwd = "right"
            speed_up = "]"
            "##,
        );
        assert_eq!(config.theme.accent, Color::Rgb(255, 0, 255));
        assert_eq!(config.theme.spectrum_low, Color::Blue);
        assert_eq!(config.theme.text, Color::Rgb(16, 16, 16));
        assert_eq!(config.keys.quit, vec![KeyCode::Char('q')]);
        assert_eq!(
            config.keys.play_pause,
            vec![KeyCode::Char(' '), KeyCode::Char('+')]
        );
        assert_eq!(config.keys.seek_fwd, vec![KeyCode::Right]);
        assert_eq!(config.keys.speed_up, vec![KeyCode::Char(']')]);
        // Los campos no mencionados quedan en default.
        assert_eq!(config.theme.dim, Theme::default().dim);
        assert_eq!(config.keys.shuffle, Keys::default().shuffle);
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn partial_file_merges_field_by_field() {
        let config = parse(
            r##"
            [theme]
            accent = "#AABBCC"
            "##,
        );
        assert_eq!(config.theme.accent, Color::Rgb(0xAA, 0xBB, 0xCC));
        assert_eq!(
            config.theme,
            Theme {
                accent: Color::Rgb(0xAA, 0xBB, 0xCC),
                ..Theme::default()
            }
        );
        assert_eq!(config.keys, Keys::default());
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn invalid_syntax_falls_back_to_all_defaults() {
        let config = parse("[theme\naccent = ");
        assert_eq!(config.theme, Theme::default());
        assert_eq!(config.keys, Keys::default());
        assert_eq!(config.warnings.len(), 1);
        assert!(config.warnings[0].contains("sintaxis inválida"));
    }

    #[test]
    fn invalid_fields_fall_back_individually() {
        let config = parse(
            r##"
            [theme]
            accent = "púrpura"
            spectrum_low = 123
            dim = "#GGG"          # hex inválido

            [keybindings]
            quit = "voltear"
            shuffle = "s"
            "##,
        );
        // Cada campo inválido volvió a su default, sin arrastrar a los demás.
        assert_eq!(config.theme.accent, Theme::default().accent);
        assert_eq!(config.theme.spectrum_low, Theme::default().spectrum_low);
        assert_eq!(config.theme.dim, Theme::default().dim);
        // El campo válido de la misma sección sí se aplicó.
        assert_eq!(config.keys.shuffle, vec![KeyCode::Char('s')]);
        assert_eq!(config.keys.quit, Keys::default().quit);
        assert_eq!(config.warnings.len(), 4, "warnings: {:?}", config.warnings);
    }

    #[test]
    fn unknown_sections_and_keys_warn() {
        let config = parse(
            r##"
            [teme]
            accent = "#FFFFFF"

            [theme]
            acento = "#FFFFFF"

            [keybindings]
            salir = "x"
            "##,
        );
        assert_eq!(config.warnings.len(), 3, "warnings: {:?}", config.warnings);
        assert_eq!(config.theme, Theme::default());
    }

    #[test]
    fn section_of_wrong_type_warns() {
        let config = parse("theme = 3");
        assert_eq!(config.warnings.len(), 1);
        assert_eq!(config.theme, Theme::default());
    }

    #[test]
    fn parse_color_accepts_hex_and_names() {
        assert_eq!(parse_color("#22D3EE"), Some(Color::Rgb(0x22, 0xD3, 0xEE)));
        assert_eq!(parse_color(" #ffffff "), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(parse_color("cyan"), Some(Color::Cyan));
        assert_eq!(parse_color("Gris Oscuro"), Some(Color::DarkGray));
        assert_eq!(parse_color("#GGG"), None);
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("púrpura"), None);
    }

    #[test]
    fn parse_key_accepts_names_and_chars() {
        assert_eq!(parse_key("q"), Some(KeyCode::Char('q')));
        assert_eq!(parse_key("]"), Some(KeyCode::Char(']')));
        assert_eq!(parse_key("space"), Some(KeyCode::Char(' ')));
        assert_eq!(parse_key("Esc"), Some(KeyCode::Esc));
        assert_eq!(parse_key("pageup"), Some(KeyCode::PageUp));
        assert_eq!(parse_key("voltear"), None);
        assert_eq!(parse_key("dos letras"), None);
        assert_eq!(parse_key("q,esc"), None, "una lista no es una tecla");
    }

    #[test]
    fn pressed_is_case_insensitive_for_letters() {
        let quit = vec![KeyCode::Char('q'), KeyCode::Esc];
        assert!(pressed(&quit, KeyCode::Char('Q')));
        assert!(pressed(&quit, KeyCode::Char('q')));
        assert!(pressed(&quit, KeyCode::Esc));
        assert!(!pressed(&quit, KeyCode::Char('x')));
        let up = vec![KeyCode::Up];
        assert!(pressed(&up, KeyCode::Up));
        assert!(!pressed(&up, KeyCode::Down));
    }

    #[test]
    fn key_collision_warns_and_resolves_predictably() {
        let config = parse(
            r##"
            [keybindings]
            quit = "q"
            play_pause = "q"
            "##,
        );
        // Gana quit (prioridad en el dispatch); play_pause pierde «q» y
        // vuelve a su tecla por defecto, con warning que lo explicita.
        assert_eq!(config.keys.quit, vec![KeyCode::Char('q')]);
        assert_eq!(config.keys.play_pause, vec![KeyCode::Char(' ')]);
        assert_eq!(config.warnings.len(), 1, "warnings: {:?}", config.warnings);
        let warning = &config.warnings[0];
        assert!(warning.contains("play_pause"), "{warning}");
        assert!(warning.contains("quit"), "{warning}");
        assert!(warning.contains("«q»"), "{warning}");
        assert!(warning.contains("espacio"), "{warning}");
    }

    #[test]
    fn key_collision_without_free_default_leaves_action_unbound() {
        let config = parse(
            r##"
            [keybindings]
            play_pause = "enter"
            search = "space"
            "##,
        );
        // Enter la gana select (va antes en el orden de resolución); el
        // default de play_pause (space) quedó en manos de search, así que
        // play_pause termina sin bindear — y lo dice.
        assert_eq!(config.keys.select, Keys::default().select);
        assert_eq!(config.keys.search, vec![KeyCode::Char(' ')]);
        assert!(config.keys.play_pause.is_empty());
        assert_eq!(config.warnings.len(), 1, "warnings: {:?}", config.warnings);
        let warning = &config.warnings[0];
        assert!(warning.contains("queda sin ninguna tecla"), "{warning}");
        assert!(warning.contains("select"), "{warning}");
    }

    #[test]
    fn inherited_default_collisions_do_not_warn() {
        // Esc y ↑/↓ están en defaults de dos acciones por diseño (la
        // resuelve el orden del dispatch): una config default no avisa.
        let config = parse("");
        assert_eq!(config, Config::default());
        assert!(config.warnings.is_empty());
        assert!(config.keys.back.contains(&KeyCode::Esc));
        assert!(config.keys.quit.contains(&KeyCode::Esc));
        assert_eq!(config.keys.volume_up, Keys::default().volume_up);
        assert_eq!(config.keys.move_up, Keys::default().move_up);
    }

    #[test]
    fn back_is_esc_only_and_tab_stays_in_explorer() {
        // Tab en `back` hacía que Tab saliera de la app sin player
        // (back se chequea antes que explorer). Ahora Tab solo abre/cierra
        // el panel — que sin player no-opea.
        let config = parse("");
        assert_eq!(config.keys.back, vec![KeyCode::Esc]);
        assert_eq!(config.keys.explorer, vec![KeyCode::Tab]);

        // El remapeo explícito del usuario se respeta; explorer pierde Tab,
        // su única tecla por default, y queda sin bindear con warning.
        let config = parse("[keybindings]\nback = \"esc,tab\"");
        assert_eq!(config.keys.back, vec![KeyCode::Esc, KeyCode::Tab]);
        assert!(config.keys.explorer.is_empty());
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(
            config.warnings[0].contains("explorer"),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    fn library_has_no_roots_by_default_and_scans_the_known_dirs() {
        let config = parse("");
        assert_eq!(config.library, LibraryConfig::default());
        assert!(config.library.roots.is_empty());
        if let Some(home) = home_dir() {
            // Las carpetas conocidas que existan (con nombre localizado, si
            // `user-dirs.dirs` las define) quedan en el set por defecto.
            let roots = config.library.scan_roots();
            for dir in known_dirs(&home) {
                assert!(
                    roots.contains(&dir),
                    "la biblioteca por defecto incluye {}",
                    dir.display()
                );
            }
        }
    }

    #[test]
    fn known_dirs_skip_missing_folders_without_complaining() {
        let home = env::temp_dir().join("mp3tui_t15_home_only_two");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("Music")).unwrap();
        fs::create_dir_all(home.join("Documents")).unwrap();
        // Downloads y Desktop no existen: se saltean, en silencio y en el
        // orden de KNOWN_DIR_NAMES.
        assert_eq!(
            known_dirs(&home),
            vec![home.join("Music"), home.join("Documents")]
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn known_dirs_follow_localized_xdg_names() {
        let home = env::temp_dir().join("mp3tui_xdg_localized");
        let _ = fs::remove_dir_all(&home);
        let config_home = home.join(".config");
        fs::create_dir_all(&config_home).unwrap();
        // Carpetas reales con nombres localizados (locale es_* típico).
        fs::create_dir_all(home.join("Música")).unwrap();
        fs::create_dir_all(home.join("Descargas")).unwrap();
        fs::write(
            config_home.join("user-dirs.dirs"),
            "# comentario\nXDG_MUSIC_DIR=\"$HOME/Música\"\nXDG_DOWNLOAD_DIR=\"${HOME}/Descargas\"\nXDG_DESKTOP_DIR=\"$HOME/Escritorio\"\n",
        )
        .unwrap();

        // Solo las que existen y en orden de KNOWN_DIR_NAMES (Escritorio
        // está declarada pero no existe: se saltea en silencio).
        assert_eq!(
            known_dirs_from(&home, &config_home),
            vec![home.join("Música"), home.join("Descargas")]
        );

        // El parser también expande `${HOME}` y no inventa claves.
        let dirs = xdg_user_dirs(&config_home, &home);
        assert_eq!(dirs.get("Music").unwrap(), &home.join("Música"));
        assert_eq!(dirs.get("Downloads").unwrap(), &home.join("Descargas"));
        assert!(!dirs.contains_key("Documents"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn scan_roots_unions_known_and_configured_without_duplicates() {
        let home = env::temp_dir().join("mp3tui_t15_home_union");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("Music")).unwrap();
        fs::create_dir_all(home.join("Downloads")).unwrap();
        // Downloads duplicada (también conocida) y una carpeta custom que
        // ni siquiera existe: ambas se agregan sin repetir lo conocido.
        let custom = home.join("MiMusica");
        let config = LibraryConfig {
            roots: vec![home.join("Downloads"), custom.clone()],
        };
        assert_eq!(
            config.scan_roots_with(Some(&home)),
            vec![home.join("Music"), home.join("Downloads"), custom]
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn scan_roots_without_known_or_configured_falls_back_to_cwd() {
        let empty = env::temp_dir().join("mp3tui_t15_home_empty");
        let _ = fs::remove_dir_all(&empty);
        fs::create_dir_all(&empty).unwrap();

        // Home sin ninguna carpeta conocida y sin roots: va al cwd.
        let config = LibraryConfig::default();
        assert_eq!(
            config.scan_roots_with(Some(&empty)),
            vec![env::current_dir().unwrap()]
        );

        // Con roots configuradas no se mete el cwd: manda lo del usuario.
        let custom = empty.join("custom");
        let config = LibraryConfig {
            roots: vec![custom.clone()],
        };
        assert_eq!(config.scan_roots_with(Some(&empty)), vec![custom.clone()]);
        // Sin USERPROFILE (None) tampoco.
        assert_eq!(config.scan_roots_with(None), vec![custom]);

        let _ = fs::remove_dir_all(&empty);
    }

    #[test]
    fn library_section_parses_roots_and_warns_for_missing_dirs() {
        let present = env::temp_dir();
        let missing = env::temp_dir().join("mp3tui_no_existe_t14");
        // Strings literales de TOML: sin escapes, las barras de Windows
        // entran tal cual.
        let config = parse(&format!(
            "[library]\nroots = ['{}', '{}']",
            present.display(),
            missing.display()
        ));
        assert_eq!(config.library.roots, vec![present, missing]);
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(
            config.warnings[0].contains("no es una carpeta"),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    fn spotify_section_parses_client_id() {
        assert_eq!(parse("").spotify, SpotifyConfig::default());

        let config = parse("[spotify]\nclient_id = \"  abc123  \"");
        assert_eq!(config.spotify.client_id.as_deref(), Some("abc123"));
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn invalid_spotify_section_warns_and_keeps_the_default() {
        for text in [
            "[spotify]\nclient_id = \"\"",
            "[spotify]\nclient_id = 3",
            "[spotify]\nsecret = \"x\"",
            "spotify = 1",
        ] {
            let config = parse(text);
            assert_eq!(config.spotify, SpotifyConfig::default(), "{text}");
            assert_eq!(config.warnings.len(), 1, "{text}: {:?}", config.warnings);
        }
    }

    #[test]
    fn empty_or_invalid_roots_fall_back_to_the_default() {
        let config = parse("[library]\nroots = []");
        assert_eq!(config.library, LibraryConfig::default());
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);

        let config = parse("[library]\nroots = [3]");
        assert_eq!(config.library, LibraryConfig::default());
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);

        let config = parse("[library]\nbiblioteca = \"x\"");
        assert_eq!(config.library, LibraryConfig::default());
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
    }

    #[test]
    fn key_labels_match_the_footer_style() {
        assert_eq!(key_labels(&[KeyCode::Char('q'), KeyCode::Esc]), "q/Esc");
        assert_eq!(key_labels(&[KeyCode::Char(' ')]), "espacio");
        assert_eq!(key_labels(&[KeyCode::Char('['), KeyCode::Char(']')]), "[/]");
        assert_eq!(key_labels(&[KeyCode::Up]), "↑");
        assert_eq!(key_labels(&[KeyCode::Char('b'), KeyCode::Tab]), "b/Tab");
    }

    #[test]
    fn test_open_url_default_and_parse() {
        // Sin default: 'u' abre el buscador, que ya acepta URLs. Se puede
        // seguir configurando a mano.
        assert!(Keys::default().open_url.is_empty());

        // Parseo desde TOML
        let config = parse(
            r#"
            [keybindings]
            open_url = "O"
            "#,
        );
        assert_eq!(config.keys.open_url, vec![KeyCode::Char('O')]);
        assert!(
            config.warnings.is_empty(),
            "warnings: {:?}",
            config.warnings
        );
    }

    #[test]
    fn test_open_url_collision_resolves_predictably() {
        let config = parse(
            r#"
            [keybindings]
            shuffle = "o"
            open_url = "o"
            "#,
        );
        // shuffle va antes en dispatch de prioridad; open_url pierde 'o'.
        assert_eq!(config.warnings.len(), 1, "warnings: {:?}", config.warnings);
        assert!(
            config.warnings[0].contains("open_url"),
            "{:?}",
            config.warnings
        );
        assert!(
            config.warnings[0].contains("shuffle"),
            "{:?}",
            config.warnings
        );
    }

    /// Quien tenía `open_url = "u"` en su config no rompe: el buscador gana
    /// 'u' y se avisa; `open_url` queda sin teclas (el prompt viejo no se abre).
    #[test]
    fn test_legacy_open_url_u_loses_to_spotify_search_with_a_warning() {
        let config = parse("[keybindings]\nopen_url = \"u\"");
        assert_eq!(config.keys.spotify_search, vec![KeyCode::Char('u')]);
        assert!(config.keys.open_url.is_empty(), "{:?}", config.keys.open_url);
        assert_eq!(config.warnings.len(), 1, "warnings: {:?}", config.warnings);
        assert!(config.warnings[0].contains("open_url"), "{:?}", config.warnings);
        assert!(config.warnings[0].contains("spotify_search"), "{:?}", config.warnings);
    }

    #[test]
    fn spotify_library_default_is_l_and_can_be_remapped() {
        assert_eq!(Keys::default().spotify_library, vec![KeyCode::Char('l')]);
        let config = parse("[keybindings]\nspotify_library = \"m\"");
        assert_eq!(config.keys.spotify_library, vec![KeyCode::Char('m')]);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn spotify_library_collision_warns_and_the_earlier_action_wins() {
        let config = parse("[keybindings]\nspotify_library = \"u\"");
        // spotify_search va antes: spotify_library pierde 'u' y vuelve a 'l'.
        assert_eq!(config.keys.spotify_search, vec![KeyCode::Char('u')]);
        assert_eq!(config.keys.spotify_library, vec![KeyCode::Char('l')]);
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(config.warnings[0].contains("spotify_library"), "{:?}", config.warnings);
    }

    #[test]
    fn player_view_default_is_v_and_can_be_remapped() {
        assert_eq!(Keys::default().player_view, vec![KeyCode::Char('v')]);
        let config = parse("[keybindings]\nplayer_view = \"m\"");
        assert_eq!(config.keys.player_view, vec![KeyCode::Char('m')]);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn player_view_collision_warns_and_the_earlier_action_wins() {
        let config = parse("[keybindings]\nplayer_view = \"h\"");
        // playlist_history va antes: player_view pierde 'h' y vuelve a 'v'.
        assert_eq!(config.keys.playlist_history, vec![KeyCode::Char('h')]);
        assert_eq!(config.keys.player_view, vec![KeyCode::Char('v')]);
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(config.warnings[0].contains("player_view"), "{:?}", config.warnings);
    }

    #[test]
    fn playlist_history_default_is_h_and_can_be_remapped() {
        assert_eq!(Keys::default().playlist_history, vec![KeyCode::Char('h')]);
        let config = parse("[keybindings]\nplaylist_history = \"j\"");
        assert_eq!(config.keys.playlist_history, vec![KeyCode::Char('j')]);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn playlist_history_collision_warns_and_the_earlier_action_wins() {
        let config = parse("[keybindings]\nplaylist_history = \"l\"");
        // spotify_library va antes: playlist_history pierde 'l' y vuelve a 'h'.
        assert_eq!(config.keys.spotify_library, vec![KeyCode::Char('l')]);
        assert_eq!(config.keys.playlist_history, vec![KeyCode::Char('h')]);
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(config.warnings[0].contains("playlist_history"), "{:?}", config.warnings);
    }

    #[test]
    fn clear_queue_default_is_x_and_can_be_remapped() {
        assert_eq!(Keys::default().clear_queue, vec![KeyCode::Char('x')]);
        let config = parse("[keybindings]\nclear_queue = \"z\"");
        assert_eq!(config.keys.clear_queue, vec![KeyCode::Char('z')]);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn clear_queue_collision_warns_and_the_earlier_action_wins() {
        let config = parse("[keybindings]\nclear_queue = \"d\"");
        // queue_delete va antes: clear_queue pierde 'd' y vuelve a su default.
        assert_eq!(config.keys.queue_delete, vec![KeyCode::Char('d')]);
        assert_eq!(config.keys.clear_queue, vec![KeyCode::Char('x')]);
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(config.warnings[0].contains("clear_queue"), "{:?}", config.warnings);
    }

    #[test]
    fn enqueue_queue_panel_and_queue_delete_defaults() {
        let keys = Keys::default();
        assert_eq!(keys.enqueue, vec![KeyCode::Char('a')], "a encola");
        assert_eq!(
            keys.queue_panel,
            vec![KeyCode::Char('c')],
            "c rota el foco entre las columnas"
        );
        assert_eq!(
            keys.queue_delete,
            vec![KeyCode::Char('d')],
            "d borra el ítem de la cola"
        );
    }

    #[test]
    fn new_queue_keys_parse_from_toml() {
        let config = parse(
            r#"
            [keybindings]
            enqueue = "w"
            queue_panel = "C"
            queue_delete = "delete"
            "#,
        );
        assert_eq!(config.keys.enqueue, vec![KeyCode::Char('w')]);
        assert_eq!(config.keys.queue_panel, vec![KeyCode::Char('C')]);
        assert_eq!(config.keys.queue_delete, vec![KeyCode::Delete]);
        assert!(
            config.warnings.is_empty(),
            "warnings: {:?}",
            config.warnings
        );
    }

    #[test]
    fn new_queue_keys_keep_working_when_remapped() {
        // Un valor inválido deja el default y avisa, como cualquier atajo.
        let config = parse("[keybindings]\nenqueue = \"voltear\"\n");
        assert_eq!(config.keys.enqueue, Keys::default().enqueue);
        assert_eq!(config.warnings.len(), 1, "warnings: {:?}", config.warnings);
        assert!(
            config.warnings[0].contains("enqueue"),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    fn test_theme_buffered_color_default_and_custom_parse() {
        assert_eq!(Theme::default().buffered, Color::Rgb(0xA6, 0xA6, 0x9C));

        let config = parse(
            r##"
            [theme]
            buffered = "#123456"
            "##,
        );
        assert_eq!(config.theme.buffered, Color::Rgb(0x12, 0x34, 0x56));
        assert!(
            config.warnings.is_empty(),
            "warnings: {:?}",
            config.warnings
        );
    }

    // -------------------------------------------------------------------------
    // Paletas builtin, [theme] name, tecla del menú y persistencia
    // -------------------------------------------------------------------------

    #[test]
    fn theme_palette_table_lists_eight_ids_and_kanagawa_matches_default() {
        let ids: Vec<&str> = builtin_themes().iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            vec![
                "kanagawa-dragon",
                "tokyonight",
                "catppuccin-mocha",
                "dracula",
                "gruvbox-dark",
                "nord",
                "rose-pine",
                "miku",
            ],
            "los 8 ids oficiales, en orden estable de menú"
        );
        // kanagawa-dragon es exactamente el tema actual: la look & feel de hoy.
        assert_eq!(
            builtin_theme("kanagawa-dragon"),
            Some(&Theme::default()),
            "kanagawa-dragon debe ser el default actual"
        );
        // Cada paleta llena los 11 slots (struct sin Option) y todas son
        // distintas entre sí: sin copy-paste de una paleta a otra.
        let themes: Vec<&Theme> = builtin_themes().iter().map(|(_, t)| t).collect();
        for (i, a) in themes.iter().enumerate() {
            for b in themes.iter().skip(i + 1) {
                assert_ne!(a, b, "paletas duplicadas en la tabla builtin");
            }
        }
        assert!(builtin_theme("no-existe").is_none(), "id desconocido");
    }

    #[test]
    fn theme_name_resolves_palette_and_color_overrides_stay_on_top() {
        let config = parse(
            r##"
            [theme]
            name = "dracula"
            accent = "#123456"
            "##,
        );
        assert_eq!(config.theme_name, "dracula");
        assert!(
            config.warnings.is_empty(),
            "warnings: {:?}",
            config.warnings
        );
        // El override de color pisa la paleta elegida…
        assert_eq!(config.theme.accent, Color::Rgb(0x12, 0x34, 0x56));
        // …y el resto de los slots viene de la paleta dracula, no del default.
        let base = builtin_theme("dracula").expect("dracula es builtin");
        assert_eq!(config.theme.text, base.text, "slots sin override = paleta");
        assert_eq!(config.theme.select_bg, base.select_bg);
        assert_eq!(config.theme.buffered, base.buffered);
        assert_eq!(config.theme.spectrum_low, base.spectrum_low);
        assert_eq!(config.theme.spectrum_high, base.spectrum_high);
        assert_ne!(config.theme, Theme::default(), "dracula ≠ default");
        // Los overrides quedan registrados para re-aplicarlos al previsualizar.
        assert_eq!(
            config.theme_overrides,
            vec![("accent".to_string(), Color::Rgb(0x12, 0x34, 0x56))]
        );
    }

    #[test]
    fn theme_name_unknown_or_invalid_warns_and_falls_back() {
        let unknown = parse(
            r#"
            [theme]
            name = "no-existe"
            "#,
        );
        assert_eq!(unknown.theme_name, DEFAULT_THEME_ID);
        assert_eq!(unknown.theme, Theme::default(), "fallback al default");
        assert!(
            unknown
                .warnings
                .iter()
                .any(|w| w.contains("no-existe") && w.contains("desconocido")),
            "warnings: {:?}",
            unknown.warnings
        );

        let invalid = parse(
            r#"
            [theme]
            name = 42
            "#,
        );
        assert_eq!(invalid.theme_name, DEFAULT_THEME_ID);
        assert_eq!(invalid.theme, Theme::default());
        assert!(
            invalid.warnings.iter().any(|w| w.contains("name")),
            "warnings: {:?}",
            invalid.warnings
        );

        // Sin `name`: default, sin ningún aviso.
        let plain = parse("[theme]\naccent = \"#112233\"\n");
        assert_eq!(plain.theme_name, DEFAULT_THEME_ID);
        assert!(plain.warnings.is_empty(), "warnings: {:?}", plain.warnings);
    }

    // -------------------------------------------------------------------------
    // Acción `settings` (panel de configuración, Ctrl+P)
    // -------------------------------------------------------------------------

    #[test]
    fn settings_defaults_to_ctrl_p_and_parses() {
        assert_eq!(
            Keys::default().settings,
            vec![KeyCode::Char('p')],
            "default: la letra de Ctrl+P"
        );

        let config = parse("[keybindings]\nsettings = \"o\"\n");
        assert_eq!(config.keys.settings, vec![KeyCode::Char('o')]);
        assert!(
            config.warnings.is_empty(),
            "warnings: {:?}",
            config.warnings
        );
    }

    #[test]
    fn settings_default_does_not_collide_with_prev_track() {
        // `settings` (Ctrl+P) y `prev_track` (p) comparten la letra pero no la
        // combinación: los defaults no generan avisos y ambos conservan su tecla.
        let config = parse("");
        assert!(
            config.warnings.is_empty(),
            "warnings: {:?}",
            config.warnings
        );
        assert_eq!(config.keys.settings, vec![KeyCode::Char('p')]);
        assert_eq!(config.keys.prev_track, vec![KeyCode::Char('p')]);
    }

    #[test]
    fn pressed_ctrl_requires_the_control_modifier() {
        use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
        let bindings = vec![KeyCode::Char('p')];
        let plain = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(
            !pressed_ctrl(&bindings, plain),
            "la letra suelta no dispara settings"
        );
        let ctrl = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(pressed_ctrl(&bindings, ctrl), "Ctrl+P sí");
        let other = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(
            !pressed_ctrl(&bindings, other),
            "otra tecla con Ctrl tampoco"
        );
        let upper = KeyEvent::new(KeyCode::Char('P'), KeyModifiers::CONTROL);
        assert!(pressed_ctrl(&bindings, upper), "sin distinguir mayúsculas");
    }

    #[test]
    fn theme_menu_action_removed_warns_as_unknown() {
        // La acción `theme_menu` ya no existe; la tecla `t` queda libre y
        // una config vieja que la mencione avisa como acción desconocida.
        let config = parse("[keybindings]\ntheme_menu = \"t\"\n");
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("theme_menu") && w.contains("desconocida")),
            "warnings: {:?}",
            config.warnings
        );
    }

    #[test]
    fn update_theme_name_creates_section_when_missing() {
        assert_eq!(
            update_theme_name("", "nord"),
            "[theme]\nname = \"nord\"\n",
            "archivo vacío: crea la sección"
        );

        let existing = "[library]\nroots = []\n";
        let out = update_theme_name(existing, "nord");
        assert!(
            out.starts_with(existing),
            "el contenido existente se preserva: {out:?}"
        );
        assert!(
            out.ends_with("[theme]\nname = \"nord\"\n"),
            "se agrega [theme] al final: {out:?}"
        );
    }

    #[test]
    fn update_theme_name_inserts_into_existing_section() {
        let out = update_theme_name("[theme]\naccent = \"#111111\"\n", "nord");
        assert_eq!(
            out, "[theme]\nname = \"nord\"\naccent = \"#111111\"\n",
            "inserta name justo después del encabezado"
        );

        let out = update_theme_name(
            "[keybindings]\nname = \"otra\"\n\n[theme]\nok = \"#00FF00\"\n",
            "dracula",
        );
        assert!(
            out.contains("[keybindings]\nname = \"otra\"\n"),
            "otra sección intacta: {out:?}"
        );
        assert!(
            out.contains("[theme]\nname = \"dracula\"\nok = \"#00FF00\"\n"),
            "insertada solo dentro de [theme]: {out:?}"
        );
    }

    #[test]
    fn update_theme_name_replaces_value_and_keeps_comments() {
        let out = update_theme_name(
            "[theme]\nname = \"dracula\"  # viejo\nok = \"#00FF00\"\n",
            "nord",
        );
        assert_eq!(
            out, "[theme]\nname = \"nord\" # viejo\nok = \"#00FF00\"\n",
            "reemplaza solo el valor y conserva el comentario"
        );

        let out = update_theme_name("[theme]\nname = \"nord\"\n", "gruvbox-dark");
        assert_eq!(out, "[theme]\nname = \"gruvbox-dark\"\n");

        // Un comentario que menciona [theme] no abre una sección.
        let out = update_theme_name("# [theme]\n[library]\nroots = []\n", "nord");
        assert!(
            out.starts_with("# [theme]\n") && out.ends_with("[theme]\nname = \"nord\"\n"),
            "{out:?}"
        );
    }

    #[test]
    fn update_theme_name_preserves_crlf_and_unrelated_content() {
        let crlf = "[theme]\r\nname = \"nord\"\r\naccent = \"#111111\"\r\n";
        let out = update_theme_name(crlf, "tokyonight");
        assert_eq!(
            out, "[theme]\r\nname = \"tokyonight\"\r\naccent = \"#111111\"\r\n",
            "reemplazo con CRLF: solo cambia el valor"
        );

        let out = update_theme_name("[theme]\r\naccent = \"#111111\"\r\n", "nord");
        assert_eq!(
            out, "[theme]\r\nname = \"nord\"\r\naccent = \"#111111\"\r\n",
            "inserción con CRLF"
        );

        let out = update_theme_name("[library]\r\nroots = []", "nord");
        assert_eq!(
            out, "[library]\r\nroots = []\r\n[theme]\r\nname = \"nord\"\r\n",
            "creación con CRLF y sin salto final previo"
        );
    }

    #[test]
    fn persist_theme_name_never_writes_invalid_toml() {
        // Formas válidas de TOML que el editor por líneas no reconoce: el
        // resultado tiene que seguir siendo TOML válido con el nombre pedido, o
        // el archivo tiene que quedar intacto (nunca una config rota).
        let cases = [
            ("inline", "theme = { accent = \"#112233\" }\n"),
            ("dotted", "theme.accent = \"#112233\"\n"),
            ("quoted", "[theme]\n\"name\" = \"nord\"\n"),
        ];
        for (label, original) in cases {
            let path = std::env::temp_dir().join(format!(
                "mp3tui_theme_invalid_{label}_{}.toml",
                std::process::id()
            ));
            std::fs::write(&path, original).expect("escribe el original");

            let result = persist_theme_name_at(&path, "dracula");
            let text = std::fs::read_to_string(&path).expect("sigue existiendo");
            let parsed = text.parse::<toml::Table>();
            let _ = std::fs::remove_file(&path);

            match result {
                Ok(()) => {
                    let table = parsed.unwrap_or_else(|e| {
                        panic!("{label}: Ok pero dejó TOML inválido: {e}\n{text}")
                    });
                    let name = table
                        .get("theme")
                        .and_then(|t| t.get("name"))
                        .and_then(|n| n.as_str());
                    assert_eq!(name, Some("dracula"), "{label}: nombre guardado");
                }
                Err(_) => assert_eq!(text, original, "{label}: archivo intacto si falla"),
            }
        }
    }

    #[test]
    fn persist_theme_name_at_updates_or_creates_and_reports_errors() {
        let path =
            std::env::temp_dir().join(format!("mp3tui_theme_cfg_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // 1. No existe: lo crea con la sección mínima.
        persist_theme_name_at(&path, "nord").expect("crea el archivo");
        let text = std::fs::read_to_string(&path).expect("recién creado");
        assert_eq!(text, "[theme]\nname = \"nord\"\n");

        // 2. Existe: solo cambia name; comentarios, otras claves y secciones
        //    quedan intactos y no queda archivo temporal.
        let original = "# mío\n[theme]\nname = \"nord\"\nbuffered = \"#123456\"\n";
        std::fs::write(&path, original).unwrap();
        persist_theme_name_at(&path, "dracula").expect("actualiza");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# mío\n[theme]\nname = \"dracula\"\nbuffered = \"#123456\"\n"
        );
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        assert!(
            !std::path::PathBuf::from(&tmp).exists(),
            "sin archivo temporal colgando"
        );

        // 3. Destino no escribible (directorio): error corto, sin pánico.
        let dir = std::env::temp_dir().join(format!("mp3tui_theme_dir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = persist_theme_name_at(&dir, "nord").expect_err("un directorio no es escribible");
        assert!(!err.is_empty(), "mensaje de error no vacío");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&path);
    }

    // -------------------------------------------------------------------------
    // Selector de Bandas — opciones, reescritura y persistencia de style
    // -------------------------------------------------------------------------

    #[test]
    fn visualizer_style_options_list_three_values_in_stable_menu_order() {
        assert_eq!(
            VisualizerStyle::options(),
            [
                VisualizerStyle::Auto,
                VisualizerStyle::Ascii,
                VisualizerStyle::Unicode
            ],
            "orden estable del sub-modal: auto, ascii, unicode"
        );
        assert_eq!(VisualizerStyle::Auto.as_str(), "auto");
        assert_eq!(VisualizerStyle::Ascii.as_str(), "ascii");
        assert_eq!(VisualizerStyle::Unicode.as_str(), "unicode");
    }

    #[test]
    fn update_visualizer_style_creates_section_when_missing() {
        assert_eq!(
            update_visualizer_style("", "ascii"),
            "[visualizer]\nstyle = \"ascii\"\n",
            "archivo vacío: crea la sección"
        );

        let existing = "[library]\nroots = []\n";
        let out = update_visualizer_style(existing, "unicode");
        assert!(
            out.starts_with(existing),
            "el contenido existente se preserva: {out:?}"
        );
        assert!(
            out.ends_with("[visualizer]\nstyle = \"unicode\"\n"),
            "se agrega [visualizer] al final: {out:?}"
        );
    }

    #[test]
    fn update_visualizer_style_replaces_and_inserts_preserving_comments() {
        let out = update_visualizer_style("[visualizer]\nstyle = \"unicode\"  # viejo\n", "ascii");
        assert_eq!(
            out, "[visualizer]\nstyle = \"ascii\" # viejo\n",
            "reemplaza solo el valor y conserva el comentario"
        );

        let out = update_visualizer_style("[visualizer]\nmode = 3\n", "auto");
        assert_eq!(
            out, "[visualizer]\nstyle = \"auto\"\nmode = 3\n",
            "inserta style justo después del encabezado"
        );

        // Un comentario que menciona [visualizer] no abre una sección.
        let out = update_visualizer_style("# [visualizer]\n[library]\nroots = []\n", "ascii");
        assert!(
            out.starts_with("# [visualizer]\n")
                && out.ends_with("[visualizer]\nstyle = \"ascii\"\n"),
            "{out:?}"
        );

        let crlf = "[visualizer]\r\nstyle = \"auto\"\r\n";
        assert_eq!(
            update_visualizer_style(crlf, "unicode"),
            "[visualizer]\r\nstyle = \"unicode\"\r\n",
            "reemplazo con CRLF: solo cambia el valor"
        );
    }

    #[test]
    fn persist_visualizer_style_never_writes_invalid_toml() {
        // Igual que el tema: formas válidas de TOML que el editor por líneas
        // no reconoce → el archivo queda intacto, nunca roto.
        let cases = [
            ("inline", "visualizer = { style = \"auto\" }\n"),
            ("dotted", "visualizer.style = \"auto\"\n"),
            ("quoted", "[visualizer]\n\"style\" = \"auto\"\n"),
        ];
        for (label, original) in cases {
            let path = std::env::temp_dir().join(format!(
                "mp3tui_bands_invalid_{label}_{}.toml",
                std::process::id()
            ));
            std::fs::write(&path, original).expect("escribe el original");

            let result = persist_visualizer_style_at(&path, "ascii");
            let text = std::fs::read_to_string(&path).expect("sigue existiendo");
            let parsed = text.parse::<toml::Table>();
            let _ = std::fs::remove_file(&path);

            match result {
                Ok(()) => {
                    let table = parsed.unwrap_or_else(|e| {
                        panic!("{label}: Ok pero dejó TOML inválido: {e}\n{text}")
                    });
                    let style = table
                        .get("visualizer")
                        .and_then(|t| t.get("style"))
                        .and_then(|n| n.as_str());
                    assert_eq!(style, Some("ascii"), "{label}: estilo guardado");
                }
                Err(_) => assert_eq!(text, original, "{label}: archivo intacto si falla"),
            }
        }
    }

    #[test]
    fn persist_visualizer_style_at_updates_or_creates_and_reports_errors() {
        let path =
            std::env::temp_dir().join(format!("mp3tui_bands_cfg_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // 1. No existe: lo crea con la sección mínima.
        persist_visualizer_style_at(&path, "ascii").expect("crea el archivo");
        let text = std::fs::read_to_string(&path).expect("recién creado");
        assert_eq!(text, "[visualizer]\nstyle = \"ascii\"\n");

        // 2. Existe: solo cambia style; comentarios y otras secciones intactos,
        //    sin archivo temporal colgando.
        let original = "# mío\n[visualizer]\nstyle = \"ascii\"\n[ui]\nmouse = false\n";
        std::fs::write(&path, original).unwrap();
        persist_visualizer_style_at(&path, "unicode").expect("actualiza");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# mío\n[visualizer]\nstyle = \"unicode\"\n[ui]\nmouse = false\n"
        );
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        assert!(
            !std::path::PathBuf::from(&tmp).exists(),
            "sin archivo temporal colgando"
        );

        // 3. Destino no escribible (directorio): error corto, sin pánico.
        let dir = std::env::temp_dir().join(format!("mp3tui_bands_dir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err =
            persist_visualizer_style_at(&dir, "ascii").expect_err("un directorio no es escribible");
        assert!(!err.is_empty(), "mensaje de error no vacío");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&path);
    }

    // Toggle de Mouse — reescritura y persistencia de `[ui] mouse`
    // -------------------------------------------------------------------------

    #[test]
    fn update_ui_mouse_creates_section_when_missing() {
        assert_eq!(
            update_ui_mouse("", false),
            "[ui]\nmouse = false\n",
            "archivo vacío: crea la sección"
        );

        let existing = "[library]\nroots = []\n";
        let out = update_ui_mouse(existing, true);
        assert!(
            out.starts_with(existing),
            "el contenido existente se preserva: {out:?}"
        );
        assert!(
            out.ends_with("[ui]\nmouse = true\n"),
            "se agrega [ui] al final: {out:?}"
        );
    }

    #[test]
    fn update_ui_mouse_replaces_and_inserts_preserving_comments() {
        let out = update_ui_mouse("[ui]\nmouse = true  # viejo\n", false);
        assert_eq!(
            out, "[ui]\nmouse = false # viejo\n",
            "reemplaza solo el valor y conserva el comentario"
        );

        let out = update_ui_mouse("[ui]\nvolume = 50\n", true);
        assert_eq!(
            out, "[ui]\nmouse = true\nvolume = 50\n",
            "inserta mouse justo después del encabezado"
        );

        // Un comentario que menciona [ui] no abre una sección.
        let out = update_ui_mouse("# [ui]\n[library]\nroots = []\n", false);
        assert!(
            out.starts_with("# [ui]\n") && out.ends_with("[ui]\nmouse = false\n"),
            "{out:?}"
        );

        let crlf = "[ui]\r\nmouse = true\r\n";
        assert_eq!(
            update_ui_mouse(crlf, false),
            "[ui]\r\nmouse = false\r\n",
            "reemplazo con CRLF: solo cambia el valor"
        );
    }

    #[test]
    fn persist_mouse_enabled_never_writes_invalid_toml() {
        // Igual que tema y bandas: formas válidas de TOML que el editor por
        // líneas no reconoce → el archivo queda intacto, nunca roto.
        let cases = [
            ("inline", "ui = { mouse = true }\n"),
            ("dotted", "ui.mouse = true\n"),
            ("quoted", "[ui]\n\"mouse\" = true\n"),
        ];
        for (label, original) in cases {
            let path = std::env::temp_dir().join(format!(
                "mp3tui_mouse_invalid_{label}_{}.toml",
                std::process::id()
            ));
            std::fs::write(&path, original).expect("escribe el original");

            let result = persist_mouse_enabled_at(&path, false);
            let text = std::fs::read_to_string(&path).expect("sigue existiendo");
            let parsed = text.parse::<toml::Table>();
            let _ = std::fs::remove_file(&path);

            match result {
                Ok(()) => {
                    let table = parsed.unwrap_or_else(|e| {
                        panic!("{label}: Ok pero dejó TOML inválido: {e}\n{text}")
                    });
                    let mouse = table
                        .get("ui")
                        .and_then(|t| t.get("mouse"))
                        .and_then(|n| n.as_bool());
                    assert_eq!(mouse, Some(false), "{label}: valor guardado");
                }
                Err(_) => assert_eq!(text, original, "{label}: archivo intacto si falla"),
            }
        }
    }

    #[test]
    fn persist_mouse_enabled_at_updates_or_creates_and_reports_errors() {
        let path =
            std::env::temp_dir().join(format!("mp3tui_mouse_cfg_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // 1. No existe: lo crea con la sección mínima.
        persist_mouse_enabled_at(&path, false).expect("crea el archivo");
        let text = std::fs::read_to_string(&path).expect("recién creado");
        assert_eq!(text, "[ui]\nmouse = false\n");

        // 2. Existe: solo cambia mouse; comentarios y otras secciones
        //    intactos, sin archivo temporal colgando.
        let original = "# mío\n[ui]\nmouse = true\n[visualizer]\nstyle = \"ascii\"\n";
        std::fs::write(&path, original).unwrap();
        persist_mouse_enabled_at(&path, false).expect("actualiza");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# mío\n[ui]\nmouse = false\n[visualizer]\nstyle = \"ascii\"\n"
        );
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        assert!(
            !std::path::PathBuf::from(&tmp).exists(),
            "sin archivo temporal colgando"
        );

        // 3. Destino no escribible (directorio): error corto, sin pánico.
        let dir = std::env::temp_dir().join(format!("mp3tui_mouse_dir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = persist_mouse_enabled_at(&dir, true).expect_err("un directorio no es escribible");
        assert!(!err.is_empty(), "mensaje de error no vacío");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&path);
    }

    // -------------------------------------------------------------------------
    // `[visualizer] mode` — opciones, parseo,
    // reescritura y persistencia
    // -------------------------------------------------------------------------

    #[test]
    fn visualizer_mode_defaults_to_barras_and_parses_all_values() {
        assert_eq!(parse("").visualizer_mode, VisualizerMode::Barras);
        for (raw, expected) in [
            ("barras", VisualizerMode::Barras),
            ("onda", VisualizerMode::Onda),
            ("espejo", VisualizerMode::Espejo),
            ("vu", VisualizerMode::Vu),
            ("circular", VisualizerMode::Circular),
            ("miku", VisualizerMode::Miku),
        ] {
            let config = parse(&format!("[visualizer]\nmode = \"{raw}\"\n"));
            assert_eq!(config.visualizer_mode, expected, "mode={raw}");
            assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        }
    }

    #[test]
    fn visualizer_mode_invalid_or_unknown_warns_and_keeps_default() {
        let bad = parse("[visualizer]\nmode = \"cascada\"");
        assert_eq!(bad.visualizer_mode, VisualizerMode::Barras);
        assert_eq!(bad.warnings.len(), 1, "{:?}", bad.warnings);
        assert!(bad.warnings[0].contains("mode"), "{:?}", bad.warnings);
        // La lista de válidos sale de `VisualizerMode::options()`, no de un
        // string duplicado a mano.
        assert!(
            bad.warnings[0].contains("barras, onda, espejo, vu, circular, miku"),
            "lista de válidos derivada de options(): {:?}",
            bad.warnings
        );

        // `caida` ya no existe: cae al aviso específico + default (no el
        // genérico de «no reconocido»), y marca la config para que `load()`
        // la migre.
        let removed = parse("[visualizer]\nmode = \"caida\"");
        assert_eq!(
            removed.visualizer_mode,
            VisualizerMode::Barras,
            "caida ya no existe: cae a barras"
        );
        assert!(
            removed.removed_visualizer_mode,
            "marca para que load() migre el archivo real"
        );
        assert_eq!(removed.warnings.len(), 1, "{:?}", removed.warnings);
        assert!(
            removed.warnings[0].contains("eliminado"),
            "aviso específico de modo eliminado, no el genérico: {:?}",
            removed.warnings
        );

        // Un valor inválido cualquiera no dispara la migración.
        assert!(!bad.removed_visualizer_mode);

        let not_a_string = parse("[visualizer]\nmode = 42");
        assert_eq!(not_a_string.visualizer_mode, VisualizerMode::Barras);
        assert_eq!(
            not_a_string.warnings.len(),
            1,
            "{:?}",
            not_a_string.warnings
        );

        let unknown = parse("[visualizer]\ntempo = 2");
        assert_eq!(unknown.visualizer_mode, VisualizerMode::Barras);
        assert_eq!(unknown.warnings.len(), 1, "{:?}", unknown.warnings);
        assert!(
            unknown.warnings[0].contains("tempo"),
            "{:?}",
            unknown.warnings
        );
    }

    /// El modo «miku» parsea como los demás, y el tema Miku arranca en
    /// modo Miku cuando `[visualizer] mode` NO está explicitado; un modo
    /// explícito gana sobre el default del tema.
    #[test]
    fn visualizer_mode_miku_parses_and_miku_theme_defaults_the_mode() {
        let explicit = parse("[visualizer]\nmode = \"miku\"\n");
        assert_eq!(explicit.visualizer_mode, VisualizerMode::Miku);
        assert!(explicit.warnings.is_empty(), "{:?}", explicit.warnings);

        let from_theme = parse("[theme]\nname = \"miku\"\n");
        assert_eq!(from_theme.theme_name, "miku");
        assert_eq!(
            from_theme.visualizer_mode,
            VisualizerMode::Miku,
            "sin modo explícito el tema Miku activa la bailarina"
        );

        let explicit_wins = parse("[theme]\nname = \"miku\"\n[visualizer]\nmode = \"barras\"\n");
        assert_eq!(
            explicit_wins.visualizer_mode,
            VisualizerMode::Barras,
            "un modo explícito gana sobre el default del tema"
        );
    }

    /// El tema Miku existe con paleta completa (los 11 slots coloreados) y en
    /// el orden estable del menú.
    #[test]
    fn builtin_themes_include_miku_with_a_full_palette() {
        let themes = builtin_themes();
        let (id, theme) = themes
            .iter()
            .find(|(id, _)| *id == "miku")
            .expect("el tema miku existe");
        assert_eq!(*id, "miku");
        assert_ne!(theme.accent, Color::Reset);
        assert_ne!(theme.text, Color::Reset);
        assert_ne!(theme.spectrum_high, Color::Reset);
        assert_ne!(theme.buffered, Color::Reset);
        // Último del menú estable, detrás de los 7 previos.
        assert_eq!(themes.last().map(|(id, _)| *id), Some("miku"));
    }

    #[test]
    fn visualizer_mode_does_not_drag_other_fields() {
        let config =
            parse("[visualizer]\nstyle = \"ascii\"\nmode = \"espejo\"\n[ui]\nmouse = false\n");
        assert_eq!(config.visualizer_style, VisualizerStyle::Ascii);
        assert_eq!(config.visualizer_mode, VisualizerMode::Espejo);
        assert!(!config.mouse);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn visualizer_mode_options_list_six_values_in_stable_menu_order() {
        assert_eq!(
            VisualizerMode::options(),
            [
                VisualizerMode::Barras,
                VisualizerMode::Onda,
                VisualizerMode::Espejo,
                VisualizerMode::Vu,
                VisualizerMode::Circular,
                VisualizerMode::Miku,
            ],
            "orden estable del sub-modal: barras, onda, espejo, vu, circular, miku"
        );
        assert_eq!(VisualizerMode::Barras.as_str(), "barras");
        assert_eq!(VisualizerMode::Onda.as_str(), "onda");
        assert_eq!(VisualizerMode::Espejo.as_str(), "espejo");
        assert_eq!(VisualizerMode::Vu.as_str(), "vu");
        assert_eq!(VisualizerMode::Circular.as_str(), "circular");
        assert_eq!(VisualizerMode::Miku.as_str(), "miku");
    }

    #[test]
    fn update_visualizer_mode_creates_section_when_missing() {
        assert_eq!(
            update_visualizer_mode("", "onda"),
            "[visualizer]\nmode = \"onda\"\n",
            "archivo vacío: crea la sección"
        );

        let existing = "[library]\nroots = []\n";
        let out = update_visualizer_mode(existing, "vu");
        assert!(
            out.starts_with(existing),
            "el contenido existente se preserva: {out:?}"
        );
        assert!(
            out.ends_with("[visualizer]\nmode = \"vu\"\n"),
            "se agrega [visualizer] al final: {out:?}"
        );
    }

    #[test]
    fn update_visualizer_mode_replaces_and_inserts_preserving_comments() {
        let out = update_visualizer_mode("[visualizer]\nmode = \"barras\"  # viejo\n", "onda");
        assert_eq!(
            out, "[visualizer]\nmode = \"onda\" # viejo\n",
            "reemplaza solo el valor y conserva el comentario"
        );

        // La otra clave de la sección queda intacta: mode se inserta justo
        // después del encabezado.
        let out = update_visualizer_mode("[visualizer]\nstyle = \"ascii\"\n", "espejo");
        assert_eq!(
            out, "[visualizer]\nmode = \"espejo\"\nstyle = \"ascii\"\n",
            "inserta mode justo después del encabezado"
        );

        // Un comentario que menciona [visualizer] no abre una sección.
        let out = update_visualizer_mode("# [visualizer]\n[library]\nroots = []\n", "vu");
        assert!(
            out.starts_with("# [visualizer]\n") && out.ends_with("[visualizer]\nmode = \"vu\"\n"),
            "{out:?}"
        );

        let crlf = "[visualizer]\r\nmode = \"barras\"\r\n";
        assert_eq!(
            update_visualizer_mode(crlf, "circular"),
            "[visualizer]\r\nmode = \"circular\"\r\n",
            "reemplazo con CRLF: solo cambia el valor"
        );
    }

    #[test]
    fn persist_visualizer_mode_never_writes_invalid_toml() {
        // Igual que style: formas válidas de TOML que el editor por líneas
        // no reconoce → el archivo queda intacto, nunca roto.
        let cases = [
            ("inline", "visualizer = { mode = \"barras\" }\n"),
            ("dotted", "visualizer.mode = \"barras\"\n"),
            ("quoted", "[visualizer]\n\"mode\" = \"barras\"\n"),
        ];
        for (label, original) in cases {
            let path = std::env::temp_dir().join(format!(
                "mp3tui_mode_invalid_{label}_{}.toml",
                std::process::id()
            ));
            std::fs::write(&path, original).expect("escribe el original");

            let result = persist_visualizer_mode_at(&path, "onda");
            let text = std::fs::read_to_string(&path).expect("sigue existiendo");
            let parsed = text.parse::<toml::Table>();
            let _ = std::fs::remove_file(&path);

            match result {
                Ok(()) => {
                    let table = parsed.unwrap_or_else(|e| {
                        panic!("{label}: Ok pero dejó TOML inválido: {e}\n{text}")
                    });
                    let mode = table
                        .get("visualizer")
                        .and_then(|t| t.get("mode"))
                        .and_then(|n| n.as_str());
                    assert_eq!(mode, Some("onda"), "{label}: modo guardado");
                }
                Err(_) => assert_eq!(text, original, "{label}: archivo intacto si falla"),
            }
        }
    }

    #[test]
    fn persist_visualizer_mode_at_updates_or_creates_and_reports_errors() {
        let path =
            std::env::temp_dir().join(format!("mp3tui_mode_cfg_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // 1. No existe: lo crea con la sección mínima.
        persist_visualizer_mode_at(&path, "onda").expect("crea el archivo");
        let text = std::fs::read_to_string(&path).expect("recién creado");
        assert_eq!(text, "[visualizer]\nmode = \"onda\"\n");

        // 2. Existe: solo cambia mode; comentarios, style y otras secciones
        //    intactos, sin archivo temporal colgando.
        let original =
            "# mío\n[visualizer]\nstyle = \"ascii\"\nmode = \"barras\"\n[ui]\nmouse = false\n";
        std::fs::write(&path, original).unwrap();
        persist_visualizer_mode_at(&path, "espejo").expect("actualiza");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# mío\n[visualizer]\nstyle = \"ascii\"\nmode = \"espejo\"\n[ui]\nmouse = false\n"
        );
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        assert!(
            !std::path::PathBuf::from(&tmp).exists(),
            "sin archivo temporal colgando"
        );

        // 3. Destino no escribible (directorio): error corto, sin pánico.
        let dir = std::env::temp_dir().join(format!("mp3tui_mode_dir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err =
            persist_visualizer_mode_at(&dir, "onda").expect_err("un directorio no es escribible");
        assert!(!err.is_empty(), "mensaje de error no vacío");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migrate_removed_visualizer_mode_at_rewrites_caida_to_barras_once() {
        let path =
            std::env::temp_dir().join(format!("mp3tui_mode_migrate_{}.toml", std::process::id()));
        std::fs::write(&path, "# mío\n[visualizer]\nmode = \"caida\"\n").unwrap();

        let mut warnings = Vec::new();
        migrate_removed_visualizer_mode_at(&path, &mut warnings);

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "# mío\n[visualizer]\nmode = \"barras\"\n");
        assert!(warnings.is_empty(), "{:?}", warnings);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migrate_removed_visualizer_mode_at_reports_write_failure_as_warning() {
        // Destino no escribible (directorio): el error se agrega a
        // `warnings`, nunca entra en pánico ni se reintenta.
        let dir =
            std::env::temp_dir().join(format!("mp3tui_mode_migrate_dir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut warnings = Vec::new();
        migrate_removed_visualizer_mode_at(&dir, &mut warnings);

        assert_eq!(warnings.len(), 1, "{:?}", warnings);
        assert!(!warnings[0].is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
