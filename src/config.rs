//! Configuración (T9): tema visual y atajos remapeables.
//!
//! El archivo `config.toml` es opcional: sin él (o con partes inválidas) la
//! app corre con los mismos valores por defecto que antes de existirlo.
//! Cada campo inválido cae **solo** a su default y deja un aviso en
//! `Config::warnings` (se muestra en stderr al arrancar y en la primera
//! pantalla de la TUI).
//!
//! Ruta por defecto (convención de Windows, sin dependencias extra):
//! `%APPDATA%\mp3tui\config.toml`. La variable `MP3TUI_CONFIG` permite
//! apuntar a otro archivo (útil para pruebas).
//!
//! Formato soportado — un subconjunto razonable de TOML con claves planas:
//!
//! ```toml
//! [theme]
//! accent = "#957FB8"        # «#RRGGBB» o nombre: cyan, blanco, gris, ...
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

use ratatui::crossterm::event::KeyCode;
use ratatui::style::Color;

/// Override de la ruta del archivo (pruebas / instalaciones portables).
pub const CONFIG_ENV_OVERRIDE: &str = "MP3TUI_CONFIG";

/// Configuración efectiva de la app (se carga una vez al arrancar).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub theme: Theme,
    pub keys: Keys,
    pub library: LibraryConfig,
    /// Avisos de la carga: campos inválidos, claves desconocidas o archivo
    /// ilegible. Vacío cuando todo (o el archivo mismo) está bien.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Ruta y ciclo de vida
// ---------------------------------------------------------------------------

/// `%APPDATA%\mp3tui\config.toml`; `None` si la variable no existe.
pub fn default_path() -> Option<PathBuf> {
    let appdata = env::var_os("APPDATA")?;
    let mut path = PathBuf::from(appdata);
    path.push("mp3tui");
    path.push("config.toml");
    Some(path)
}

fn resolve_path() -> Option<(PathBuf, bool)> {
    match env::var(CONFIG_ENV_OVERRIDE) {
        Ok(custom) if !custom.trim().is_empty() => {
            Some((PathBuf::from(custom), true))
        }
        _ => default_path().map(|path| (path, false)),
    }
}

/// Lee el archivo (si existe) y lo parsea. Sin archivo: default silencioso.
pub fn load() -> Config {
    let Some((path, from_override)) = resolve_path() else {
        return Config::default();
    };
    match fs::read_to_string(&path) {
        Ok(text) => parse(&text),
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
            warnings: vec![format!(
                "no se pudo leer '{}': {err}",
                path.display()
            )],
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
            "theme" | "keybindings" | "library" => {}
            other => warnings.push(format!(
                "sección desconocida «[{other}]» (se ignora)"
            )),
        }
    }

    match table.get("theme") {
        None => {}
        Some(value) => match value.as_table() {
            Some(section) => parse_theme(section, &mut config.theme, &mut warnings),
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
            Some(section) => {
                parse_keybindings(section, &mut config.keys, &mut warnings)
            }
            None => warnings.push(
                "[keybindings] debe ser una tabla con claves de la forma \
                 clave = \"valor\""
                    .to_string(),
            ),
        },
    }

    match table.get("library") {
        None => {}
        Some(value) => match value.as_table() {
            Some(section) => {
                parse_library(section, &mut config.library, &mut warnings)
            }
            None => warnings.push(
                "[library] debe ser una tabla con claves de la forma \
                 roots = [\"ruta\", ...]"
                    .to_string(),
            ),
        },
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
/// kanagawa.nvim (T11): tonos oscuros y cálidos, acentos desaturados y
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
            spectrum_low: Color::Rgb(0x89, 0x92, 0xA7),  // dragonViolet
            spectrum_mid: Color::Rgb(0x7F, 0xB4, 0xCA),  // springBlue
            spectrum_high: Color::Rgb(0xC4, 0x74, 0x6E), // dragonRed
        }
    }
}

fn parse_theme(
    section: &toml::Table,
    theme: &mut Theme,
    warnings: &mut Vec<String>,
) {
    for (key, value) in section {
        let field = match key.as_str() {
            "accent" => &mut theme.accent,
            "text" => &mut theme.text,
            "dim" => &mut theme.dim,
            "select_bg" => &mut theme.select_bg,
            "error" => &mut theme.error,
            "ok" => &mut theme.ok,
            "warn" => &mut theme.warn,
            "spectrum_low" => &mut theme.spectrum_low,
            "spectrum_mid" => &mut theme.spectrum_mid,
            "spectrum_high" => &mut theme.spectrum_high,
            other => {
                warnings.push(format!("[theme] clave desconocida «{other}»"));
                continue;
            }
        };
        let Some(raw) = value.as_str() else {
            warnings.push(format!(
                "[theme] {key}: se espera un color entre comillas, \
                 p. ej. \"#22D3EE\"; se mantiene el valor por defecto"
            ));
            continue;
        };
        match parse_color(raw) {
            Some(color) => *field = color,
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
    pub speed_up: Vec<KeyCode>,
    pub speed_down: Vec<KeyCode>,
    /// Abrir/cerrar el panel lateral del explorador (T13): `b` / `Tab` /
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
    /// Cerrar el panel lateral del explorador / volver al player (T13); sin
    /// player en pantalla completa, sale de la app. Solo `Esc` por defecto
    /// (T14): `Tab` vive en `explorer` y acá salía de la app sin player.
    pub back: Vec<KeyCode>,
    /// Abrir prompt para reproducir URL (T6): por defecto 'u'.
    pub open_url: Vec<KeyCode>,
}

impl Default for Keys {
    fn default() -> Self {
        fn keys(codes: &[KeyCode]) -> Vec<KeyCode> {
            codes.to_vec()
        }
        Self {
            quit: keys(&[KeyCode::Char('q'), KeyCode::Esc]),
            play_pause: keys(&[KeyCode::Char(' ')]),
            volume_up: keys(&[KeyCode::Up]),
            volume_down: keys(&[KeyCode::Down]),
            seek_back: keys(&[KeyCode::Left]),
            seek_fwd: keys(&[KeyCode::Right]),
            prev_track: keys(&[KeyCode::Char('p')]),
            next_track: keys(&[KeyCode::Char('n')]),
            shuffle: keys(&[KeyCode::Char('s')]),
            repeat: keys(&[KeyCode::Char('r')]),
            search: keys(&[KeyCode::Char('/')]),
            speed_up: keys(&[KeyCode::Char(']')]),
            speed_down: keys(&[KeyCode::Char('[')]),
            explorer: keys(&[KeyCode::Char('b'), KeyCode::Tab]),
            move_up: keys(&[KeyCode::Up]),
            move_down: keys(&[KeyCode::Down]),
            page_up: keys(&[KeyCode::PageUp]),
            page_down: keys(&[KeyCode::PageDown]),
            home: keys(&[KeyCode::Home]),
            end: keys(&[KeyCode::End]),
            select: keys(&[KeyCode::Enter]),
            back: keys(&[KeyCode::Esc]),
            open_url: keys(&[KeyCode::Char('u')]),
        }
    }
}

fn parse_keybindings(
    section: &toml::Table,
    keys: &mut Keys,
    warnings: &mut Vec<String>,
) {
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
            "open_url" => &mut keys.open_url,
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
                warnings.push(format!(
                    "[keybindings] acción desconocida «{other}»"
                ));
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

/// Resuelve colisiones de teclas entre acciones distintas (T9). El dispatch
/// (if/else if con orden fijo) haría que la acción posterior quede sombreada
/// en silencio: el usuario remapea, "no le funciona" y no hay ningún aviso —
/// a diferencia del resto de los casos inválidos de la config, que sí avisan.
///
/// Reglas:
/// 1. Se recorren las acciones en orden de prioridad del dispatch: primero
///    las del explorador que se chequean antes (`back`, `select`), después el
///    orden del player y por último la navegación del explorador. Si un par
///    se resuelve distinto en cada modo (seek/speed van arriba en el player y
///    abajo en el explorador) manda el orden del player: es el modo de
///    arranque y el par coexiste ahí.
/// 2. Colisión heredada de los defaults (Esc en `back`/`quit`, ↑/↓ en
///    volumen/mover): no se avisa ni se modifica nada, el dispatch la
///    resuelve por orden como siempre.
/// 3. Si no, gana la primera en el orden; la perdedora pierde esa tecla y,
///    si queda sin teclas, vuelve a sus defaults que estén libres (o queda
///    sin bindear si ninguno está libre). Cada pérdida genera un warning que
///    dice quién gana, quién pierde y cómo quedó la perdedora.
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
        ("open_url", &mut keys.open_url, &defaults.open_url),
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
                    let inherited = winner_default.contains(&key)
                        && default.contains(&key);
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
            .filter(|key| {
                !owned
                    .iter()
                    .any(|(k, _, _)| *k == normalize_key(*key))
            })
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
    let mut groups: Vec<(&'static str, Vec<(KeyCode, &'static str)>)> =
        Vec::new();
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
        let (final_keys, emptied) = match entries
            .iter()
            .find(|(name, _, _)| *name == loser)
        {
            Some((name, list, _)) => (
                (**list).clone(),
                emptied_actions.contains(name),
            ),
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
            format!(
                "conserva sus otras teclas ({})",
                key_labels(&final_keys)
            )
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

// ---------------------------------------------------------------------------
// Biblioteca (T14)
// ---------------------------------------------------------------------------

/// Carpetas a escanear cuando la CLI no recibe ningún path (T14/T15). El
/// set final ([`LibraryConfig::scan_roots`]) une las carpetas conocidas del
/// sistema con las configuradas a mano; los tracks quedan agrupados por
/// carpeta de origen en la lista del explorador.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LibraryConfig {
    /// Raíces escritas por el usuario en `[library] roots`: se AGREGAN a las
    /// carpetas conocidas (T15), no las reemplazan. Vacío = solo conocidas.
    pub roots: Vec<PathBuf>,
}

/// Carpetas conocidas típicas bajo `%USERPROFILE%` (T15): la biblioteca
/// por defecto cubre las cuatro sin que el usuario configure nada.
const KNOWN_DIR_NAMES: [&str; 4] = ["Music", "Downloads", "Desktop", "Documents"];

/// Solo las carpetas conocidas que existen bajo `home`: una faltante se
/// saltea en silencio, porque es un default automático y no una ruta
/// configurada a mano (esas sí avisan en `parse_library`).
fn known_dirs(home: &Path) -> Vec<PathBuf> {
    KNOWN_DIR_NAMES
        .iter()
        .map(|name| home.join(name))
        .filter(|path| path.is_dir())
        .collect()
}

impl LibraryConfig {
    /// Set final a escanear sin argumento de CLI (T15): las carpetas
    /// conocidas que existen en el sistema, unidas con las configuradas,
    /// sin duplicados y en ese orden (las conocidas primero). Si queda vacío
    /// (sin `USERPROFILE` y sin roots) se usa el `cwd`, como en T14.
    pub fn scan_roots(&self) -> Vec<PathBuf> {
        let home = env::var_os("USERPROFILE").map(PathBuf::from);
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

fn parse_library(
    section: &toml::Table,
    library: &mut LibraryConfig,
    warnings: &mut Vec<String>,
) {
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
                            warnings.push(
                                "[library] roots: ruta vacía; se ignora"
                                    .to_string(),
                            );
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
                    // la biblioteca queda en las carpetas conocidas (T15).
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
            other => warnings.push(format!(
                "[library] clave desconocida «{other}»"
            )),
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
            quit = "x"
            play_pause = "space,plus"
            seek_fwd = "right"
            speed_up = "]"
            "##,
        );
        assert_eq!(config.theme.accent, Color::Rgb(255, 0, 255));
        assert_eq!(config.theme.spectrum_low, Color::Blue);
        assert_eq!(config.theme.text, Color::Rgb(16, 16, 16));
        assert_eq!(config.keys.quit, vec![KeyCode::Char('x')]);
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
        assert_eq!(config.theme, Theme {
            accent: Color::Rgb(0xAA, 0xBB, 0xCC),
            ..Theme::default()
        });
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
            play_pause = "q"
            search = "space"
            "##,
        );
        // «q» la gana quit; el default de play_pause (space) quedó en manos
        // de search, así que play_pause termina sin bindear — y lo dice.
        assert_eq!(config.keys.quit, Keys::default().quit);
        assert_eq!(config.keys.search, vec![KeyCode::Char(' ')]);
        assert!(config.keys.play_pause.is_empty());
        assert_eq!(config.warnings.len(), 1, "warnings: {:?}", config.warnings);
        let warning = &config.warnings[0];
        assert!(warning.contains("queda sin ninguna tecla"), "{warning}");
        assert!(warning.contains("quit"), "{warning}");
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
        // T14: Tab en `back` hacía que Tab saliera de la app sin player
        // (back se chequea antes que explorer). Ahora Tab solo abre/cierra
        // el panel — que sin player no-opea.
        let config = parse("");
        assert_eq!(config.keys.back, vec![KeyCode::Esc]);
        assert_eq!(
            config.keys.explorer,
            vec![KeyCode::Char('b'), KeyCode::Tab]
        );

        // El remapeo explícito del usuario se respeta; el choque con
        // explorer (que por default tenía Tab) se resuelve con warning.
        let config = parse("[keybindings]\nback = \"esc,tab\"");
        assert_eq!(config.keys.back, vec![KeyCode::Esc, KeyCode::Tab]);
        assert_eq!(config.keys.explorer, vec![KeyCode::Char('b')]);
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
        if let Some(home) = env::var_os("USERPROFILE") {
            let music = PathBuf::from(home).join("Music");
            if music.is_dir() {
                assert!(
                    config.library.scan_roots().contains(&music),
                    "si Music existe, la biblioteca por defecto la incluye"
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
        // Default debe ser 'u'
        assert_eq!(Keys::default().open_url, vec![KeyCode::Char('u')]);

        // Parseo desde TOML
        let config = parse(
            r#"
            [keybindings]
            open_url = "U"
            "#,
        );
        assert_eq!(config.keys.open_url, vec![KeyCode::Char('U')]);
        assert!(config.warnings.is_empty(), "warnings: {:?}", config.warnings);
    }

    #[test]
    fn test_open_url_collision_resolves_predictably() {
        let config = parse(
            r#"
            [keybindings]
            shuffle = "u"
            open_url = "u"
            "#,
        );
        // shuffle va antes en dispatch de prioridad; open_url pierde 'u' y vuelve al default si está libre
        assert_eq!(config.warnings.len(), 1, "warnings: {:?}", config.warnings);
        assert!(config.warnings[0].contains("open_url"), "{:?}", config.warnings);
        assert!(config.warnings[0].contains("shuffle"), "{:?}", config.warnings);
    }
}
