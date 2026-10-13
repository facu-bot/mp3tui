//! Interfaz de terminal: layout con ratatui y loop de eventos no bloqueante.
//! Reproducción y explorador de biblioteca. Con sesión de audio
//! el reproductor (info + visualizador) es el fondo permanente y el
//! explorador se superpone como panel lateral con Ctrl+Tab/b/Tab; a
//! pantalla completa solo cuando no hay player (arranque por directorio).

use std::io;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::audio::{AudioEngine, SPEED_STEP};
use crate::config::{self, key_labels, Theme};
use crate::playback::{
    filter_tracks, natural_end_action, next_track, seek_target, shuffle_order, NextAction,
    QueueItem, RepeatMode, URL_REPLAY_TOO_SOON_MSG,
};
use crate::playlist::{move_selection, scroll_offset, Playlist, Track};
use crate::runtime;
use crate::visualizer::{SpectrumAnalyzer, NUM_BANDS};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const VOLUME_STEP: f32 = 0.05;
const PAGE_STEP: isize = 10;

/// Bloques de octavo de celda para el visualizador sub-celda (mejora 1).
const EIGHTH_BLOCKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// Rampa ASCII de 10 niveles para el modo ascii (índice 0 = vacío … 9 = lleno).
const ASCII_RAMP: [&str; 10] =
    [" ", ".", ":", "-", "=", ";", "+", "*", "#", "@"];

// Paleta compartida por todo el programa: la celda runtime del
// módulo `runtime`, que cambia en caliente al previsualizar/aplicar temas desde el menú
// (default: la del `[theme] name` cargado al arrancar). Copia de 11 `Color`
// por llamada; barato por frame.
fn theme() -> Theme {
    runtime::active_theme()
}

const CURSOR: &str = "\u{258c}"; // ▌ fila seleccionada
const PLAYING: &str = "\u{25b6}"; // ▶ pista en reproducción
const SEARCH_CARET: &str = "\u{258f}"; // ▏ cursor de texto en modo búsqueda
/// Ancho fijo (en columnas) de la caja del campo de búsqueda.
const SEARCH_BOX_WIDTH: usize = 24;
/// Texto guía de la caja vacía.
const SEARCH_PLACEHOLDER: &str = "escribí para buscar";
/// Aviso cuando una lista de playlist sale vacía (S12).
const PLAYLIST_EMPTY_MSG: &str = "La playlist no tiene canciones";
/// Aviso cuando un ítem de la cola no arranca y se saltea (S13).
const QUEUE_ITEM_FAILED_MSG: &str = "No se pudo reproducir el ítem de la cola; se saltea";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Playing,
    Paused,
    Ended,
    /// No suena y además falló (apertura, decode o device): distinto de
    /// `Ended`, que es la pista que llegó a su fin natural.
    Stopped,
    /// Resolviendo URL de página con yt-dlp en segundo plano.
    Resolving,
    /// Conectando a URL de audio remota en segundo plano.
    Connecting,
}

/// Cómo arranca la TUI según el argumento de CLI.
pub enum StartMode {
    /// Archivo por CLI: reproducción directa (comportamiento original).
    PlayFile(PathBuf),
    /// URL por CLI: conexión/resolución directa en background con biblioteca scan_roots.
    PlayUrl {
        url: String,
        kind: crate::source::UrlKind,
        roots: Vec<PathBuf>,
    },
    /// Sin argumento (carpetas conocidas + `[library]` roots) o con
    /// carpeta: explorador de la biblioteca, agrupado por carpeta de origen.
    Browse(Vec<PathBuf>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitAction {
    Local(PathBuf),
    DirectAudio(String),
    Page(String),
    /// URL de playlist: se lista con `--flat-playlist` y se encola el resto (S12).
    Playlist(String),
    Unknown(String),
    Empty,
}

pub fn classify_submit_input(input: &str) -> SubmitAction {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return SubmitAction::Empty;
    }
    match crate::source::classify(trimmed) {
        crate::source::Input::Local(path) => SubmitAction::Local(path),
        crate::source::Input::Url { url, kind } => match kind {
            crate::source::UrlKind::DirectAudio => SubmitAction::DirectAudio(url),
            crate::source::UrlKind::Page => SubmitAction::Page(url),
            crate::source::UrlKind::Playlist => SubmitAction::Playlist(url),
            crate::source::UrlKind::Unknown => SubmitAction::Unknown(url),
        },
    }
}

pub fn url_display_label(url: &str) -> String {
    let trimmed = url.trim();
    let without_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else {
        trimmed
    };

    let before_fragment = without_scheme.split('#').next().unwrap_or(without_scheme);
    let before_query = before_fragment.split('?').next().unwrap_or(before_fragment);

    let mut parts: Vec<&str> = before_query
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if parts.len() > 1 {
        // Has host and path segments: return the last non-empty path segment
        parts.pop().unwrap().to_string()
    } else if let Some(host) = parts.first() {
        host.to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn handle_prompt_char(prompt: &mut String, c: char) {
    if !c.is_control() {
        prompt.push(c);
    }
}

pub fn handle_prompt_backspace(prompt: &mut String) {
    prompt.pop();
}

pub fn handle_prompt_paste(prompt: &mut String, pasted: &str) {
    for c in pasted.chars() {
        if c == '\r' || c == '\n' {
            continue;
        }
        if !c.is_control() {
            prompt.push(c);
        }
    }
}

fn handle_ytdlp_result(
    app: &mut App,
    request_id: u64,
    result: Result<crate::source::ytdlp::Resolved, crate::source::ytdlp::YtDlpError>,
) -> Option<(String, String)> {
    if request_id != app.ytdlp_request_id {
        return None;
    }
    match result {
        Ok(resolved) => {
            app.status = Status::Connecting;
            app.track_label = resolved.title.clone();
            app.error = None;
            app.ytdlp_failed = false;
            Some((resolved.audio_url, resolved.title))
        }
        Err(err) => {
            app.status = Status::Stopped;
            app.error = Some(err.to_string());
            app.ytdlp_failed = true;
            None
        }
    }
}

/// Resultado del listado `--flat-playlist` de una playlist (S12). La primera
/// entrada se resuelve enseguida (su URL de audio vence) y el resto entra a
/// la cola **al final** de la existente, sin tocar el cursor. Una lista vacía
/// o un listado fallido → aviso visible y la cola queda intacta.
fn handle_playlist_result(
    app: &mut App,
    request_id: u64,
    result: Result<Vec<crate::source::ytdlp::PlaylistEntry>, crate::source::ytdlp::YtDlpError>,
    resolve_page: &mut impl FnMut(String, u64) -> Receiver<YtDlpMessage>,
) {
    if request_id != app.ytdlp_request_id {
        // Resultado tarde de un listado ya cancelado: se descarta (S12).
        return;
    }
    match result {
        Ok(entries) if entries.is_empty() => {
            app.status = Status::Stopped;
            app.error = Some(PLAYLIST_EMPTY_MSG.to_string());
            app.ytdlp_failed = true;
            app.pending_history = None;
        }
        Ok(entries) => {
            // Las siguientes van al final de la cola existente (D7), sin
            // tocar el cursor de la cola (S12). Cada una se resuelve recién
            // cuando le toque sonar (S3).
            for entry in entries.iter().skip(1) {
                // Un título vacío (video privado/eliminado en flat mode) se
                // muestra como su URL de página, igual que en el historial.
                let title = if entry.title.is_empty() {
                    entry.page_url.clone()
                } else {
                    entry.title.clone()
                };
                app.queue.push(QueueItem::Url {
                    url: entry.page_url.clone(),
                    title,
                });
            }
            // La primera suena directo: se resuelve ahora.
            let first = &entries[0];
            app.status = Status::Resolving;
            app.error = None;
            let req_id = app.ytdlp_request_id;
            app.ytdlp_rx = Some(resolve_page(first.page_url.clone(), req_id));
        }
        Err(err) => {
            app.status = Status::Stopped;
            app.error = Some(err.to_string());
            app.ytdlp_failed = true;
            app.pending_history = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Pantalla de fondo. Con sesión de audio siempre es `Player`; el explorador
/// se superpone como panel lateral (`App::explorer_open`) sin
/// reemplazarlo. `Browser` a pantalla completa es el caso sin player
/// (arranque por directorio): no hay fondo que mostrar detrás de la lista.
enum Mode {
    Player,
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewItem {
    History(usize),
    Track(usize),
}

impl From<usize> for ViewItem {
    fn from(index: usize) -> Self {
        ViewItem::Track(index)
    }
}

impl PartialEq<usize> for ViewItem {
    fn eq(&self, other: &usize) -> bool {
        match self {
            ViewItem::Track(idx) => idx == other,
            ViewItem::History(_) => false,
        }
    }
}

impl PartialEq<ViewItem> for usize {
    fn eq(&self, other: &ViewItem) -> bool {
        other == self
    }
}

type YtDlpMessage = (
    u64,
    String,
    Result<crate::source::ytdlp::Resolved, crate::source::ytdlp::YtDlpError>,
);

/// Mensaje del listado `--flat-playlist`: id de pedido, URL original de la
/// playlist y la lista de entradas (título + URL de página).
type YtDlpListMessage = (
    u64,
    String,
    Result<Vec<crate::source::ytdlp::PlaylistEntry>, crate::source::ytdlp::YtDlpError>,
);

/// Disparador que inició el avance por la cola. Al agotarse la cola tras
/// saltear un ítem fallido (S13), decide por dónde cae el fallback al pool
/// de la biblioteca/historial: el fin natural o el sentido del salto manual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueAdvance {
    /// Fin natural de pista: avanza el pool hacia adelante.
    NaturalEnd,
    /// Salto manual `n` (+1) o `p` (-1).
    Step(isize),
}

struct App {
    mode: Mode,
    /// Panel lateral del explorador superpuesto sobre el reproductor.
    /// Solo tiene efecto en `Mode::Player`.
    explorer_open: bool,
    /// Raíces escaneadas: CLI, o bien las carpetas conocidas del
    /// sistema unidas a las `[library]` roots de la config.
    roots: Vec<PathBuf>,
    /// Última biblioteca conocida; `None` mientras no terminó el primer escaneo.
    library: Option<Playlist>,
    /// Resultado del escaneo en curso (thread de background). `Some` = escaneando.
    scan_rx: Option<Receiver<Playlist>>,
    /// Índices de la biblioteca y del historial que se muestran (filtrados por la búsqueda).
    view: Vec<ViewItem>,
    /// Selección dentro de `view` (nunca un índice directo de la biblioteca).
    selected: usize,
    /// Filtro de búsqueda confirmado (Enter); la lista visible lo respeta.
    filter: Option<String>,
    /// Texto de la búsqueda en curso; `Some` = modo búsqueda activo.
    search: Option<String>,
    /// Shuffle activo: next/previous recorren `shuffle_order` en vez del
    /// orden de archivo.
    shuffle: bool,
    repeat: RepeatMode,
    /// Permutación de `0..view.len()`; válida mientras `shuffle` esté activo
    /// y `shuffle_order.len() == view.len()`.
    shuffle_order: Vec<usize>,
    /// Cola de reproducción: lo que aparece en la solapa Cola del panel.
    /// Mientras queden ítems después de `queue_cursor` manda sobre `repeat`
    /// y `shuffle`.
    queue: Vec<QueueItem>,
    /// Posición de la cola donde está parado el cursor: el ítem que suena (o
    /// el último que sonó desde la cola). `None` mientras no haya sonado
    /// ningún ítem de la cola. Las posiciones por debajo se muestran
    /// atenuadas (ya sonaron) y las de arriba, como próximas.
    queue_cursor: Option<usize>,
    /// Índice de la cola cuya URL se está resolviendo (S13). Si la resolución
    /// falla, el event loop llama a [`skip_failed_queue_item`]: se saltea con
    /// aviso y se intenta el próximo ítem (o el pool si la cola se agota).
    queue_resolving: Option<usize>,
    /// Disparador que inició el recorrido de la cola, para el fallback (S13).
    queue_advance: QueueAdvance,
    /// Solapa activa del panel lateral: `false` = Biblioteca, `true` = Cola.
    /// Solo tiene efecto con el panel abierto.
    queue_panel: bool,
    /// Selección dentro de la Cola (independiente de `selected`, la de la
    /// Biblioteca): cada lista conserva la suya al alternar.
    queue_selected: usize,
    /// «Próximo» a tocar para la franja del panel (S3): el ítem que sigue al
    /// cursor de la cola, y con la cola agotada el siguiente del pool
    /// (biblioteca/historial con shuffle); `None` la oculta.
    next_up: Option<String>,
    /// Reloj de frames del render: alimenta el marquee de la franja.
    ui_frame: u64,
    current_path: Option<PathBuf>,
    has_player: bool,
    track_label: String,
    status: Status,
    error: Option<String>,
    should_quit: bool,
    analyzer: SpectrumAnalyzer,
    url_input: Option<String>,
    ytdlp_rx: Option<Receiver<YtDlpMessage>>,
    /// Receiver del listado `--flat-playlist` en curso (S12).
    playlist_rx: Option<Receiver<YtDlpListMessage>>,
    ytdlp_request_id: u64,
    is_url: bool,
    /// URL original (nunca la resuelta) + etiqueta de la pista cuando la
    /// reproducción actual viene de una URL; `None` en reproducción local.
    /// Guarda el original para repetir (RepeatMode::One) vía `submit_url_prompt`:
    /// las URLs que resuelve yt-dlp vencen, así que una página se re-resuelve.
    current_url: Option<(String, String)>,
    ytdlp_failed: bool,
    pub history: crate::history::History,
    pub pending_history: Option<(String, String)>,
    pub history_path: Option<PathBuf>,
    /// Menú de temas abierto: `Some` mientras el modal está activo.
    theme_menu: Option<ThemeMenuState>,
    /// Selector de bandas abierto: `Some` mientras el sub-modal está
    /// activo (el panel sigue abierto debajo).
    bands_menu: Option<BandsMenuState>,
    /// Selector de modo abierto: `Some` mientras el
    /// sub-modal está activo (el panel sigue abierto debajo).
    mode_menu: Option<ModeMenuState>,
    /// Panel de configuración abierto: `Some` mientras el modal está activo.
    settings_panel: Option<SettingsPanelState>,
    /// Captura del mouse en vivo: arranca con el valor de `[ui] mouse`
    /// y el toggle del panel la cambia en el momento (+ persiste).
    /// `handle_mouse` la respeta por encima del chequeo de modales.
    mouse_enabled: bool,
    /// Doble Esc para salir: el primer Esc arma, el segundo sale.
    quit_guard: QuitGuard,
    /// Zonas clickeables del último frame (las anota el render).
    hit: RefCell<HitMap>,
    /// Arrastre con el mouse en curso.
    drag: Option<Drag>,
}

/// Barra clickeable registrada al dibujar: columna inicial, fila y ancho del
/// tramo interno (sin etiqueta ni corchetes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BarHit {
    x: u16,
    y: u16,
    width: u16,
}

impl BarHit {
    /// Fracción 0..=1 de la columna dentro de la barra (afuera se recorta).
    fn ratio(&self, column: u16) -> f64 {
        if self.width <= 1 {
            return 0.0;
        }
        let last = self.width - 1;
        let offset = column.saturating_sub(self.x).min(last);
        f64::from(offset) / f64::from(last)
    }

    fn contains(&self, column: u16, row: u16) -> bool {
        row == self.y && column >= self.x && column < self.x.saturating_add(self.width)
    }
}

/// Zonas clickeables del último frame (las anota el render).
#[derive(Debug, Clone, Default)]
struct HitMap {
    progress: Option<BarHit>,
    volume: Option<BarHit>,
    /// Área del reproductor (info + visualizador): la rueda cambia el volumen.
    player: Option<Rect>,
    /// Botones de transporte dibujados (área y botón).
    buttons: Vec<(Rect, Button)>,
    /// Chips del footer: área y tecla que simulan al clickearlos.
    chips: Vec<(Rect, KeyCode)>,
}

/// Arrastre en curso: sigue a la barra donde empezó aunque el puntero salga.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    /// Arrastre sobre el progreso: guarda la última fracción para el seek al
    /// soltar.
    Progress(Option<f64>),
    Volume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    Prev,
    PlayPause,
    Next,
    Shuffle,
    Repeat,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MouseAction {
    None,
    SeekRatio(f64),
    SetVolume(f64),
    VolumeStep(i8),
    Press(Button),
    /// Clic en un chip del footer: simula la tecla.
    Key(KeyCode),
}

/// Una línea del footer («tecla: acción   tecla: acción») como chips: la tecla
/// resaltada como keycap y la acción tenue. Devuelve los spans (mismo texto)
/// y, por entrada, columna inicial, ancho y el texto de las teclas.
fn footer_chip_spans(text: &str) -> (Vec<Span<'static>>, Vec<(u16, u16, String)>) {
    const SEPARATOR: &str = "   ";
    let keycap = Style::default()
        .fg(theme().accent)
        .bg(theme().select_bg)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme().dim);

    let mut spans = Vec::new();
    let mut entries = Vec::new();
    let mut rest = text;
    let mut column: u16 = 0;
    let push_plain = |spans: &mut Vec<Span<'static>>, piece: &str, column: &mut u16| {
        if !piece.is_empty() {
            spans.push(Span::styled(piece.to_string(), label));
            *column += piece.chars().count() as u16;
        }
    };

    let leading = rest.len() - rest.trim_start().len();
    push_plain(&mut spans, &rest[..leading], &mut column);
    rest = &rest[leading..];

    while !rest.is_empty() {
        let (segment, separator, tail) = match rest.find(SEPARATOR) {
            Some(at) => {
                let after = &rest[at..];
                let spaces = after.len() - after.trim_start().len();
                (&rest[..at], &after[..spaces], &after[spaces..])
            }
            None => (rest, "", ""),
        };
        match segment.split_once(": ") {
            Some((keys, action)) => {
                let width = segment.chars().count() as u16;
                entries.push((column, width, keys.to_string()));
                spans.push(Span::styled(keys.to_string(), keycap));
                spans.push(Span::styled(format!(": {action}"), label));
                column += width;
            }
            None => push_plain(&mut spans, segment, &mut column),
        }
        push_plain(&mut spans, separator, &mut column);
        rest = tail;
    }
    (spans, entries)
}

/// Tecla configurada cuya etiqueta abre el texto de un chip («s», «←/→»,
/// «Esc×2»): la primera de la lista. `None` si no es un atajo.
fn key_for_chip(keys_text: &str) -> Option<KeyCode> {
    let first = keys_text.split(['/', ' ']).next()?.trim_end_matches("×2");
    // «Ctrl+P: config»: la acción `settings` exige Ctrl, así que el
    // prefijo se resuelve solo contra sus teclas (sin distinguir mayúsculas:
    // el chip muestra «P» y el binding es «p»).
    if let Some(rest) = first.strip_prefix("Ctrl+") {
        return config::config()
            .keys
            .settings
            .iter()
            .copied()
            .find(|code| config::key_label(*code).eq_ignore_ascii_case(rest));
    }
    let keys = &config::config().keys;
    [
        &keys.quit, &keys.play_pause, &keys.volume_up, &keys.volume_down,
        &keys.seek_back, &keys.seek_fwd, &keys.prev_track, &keys.next_track,
        &keys.shuffle, &keys.repeat, &keys.search, &keys.speed_up,
        &keys.speed_down, &keys.explorer, &keys.move_up, &keys.move_down,
        &keys.page_up, &keys.page_down, &keys.home, &keys.end, &keys.select,
        &keys.back, &keys.open_url, &keys.enqueue, &keys.queue_panel,
        &keys.queue_delete, &keys.settings,
    ]
    .into_iter()
    .flatten()
    .copied()
    .find(|code| config::key_label(*code) == first)
}

/// Etiqueta de la tecla de `settings` para el chip («P» en «Ctrl+P: config»):
/// una letra suelta se muestra en mayúscula (se aprieta con Ctrl); los
/// nombres especiales quedan tal cual («Enter», «Tab»…).
fn settings_key_display() -> String {
    let label = config::key_labels(&config::config().keys.settings);
    if label.chars().count() == 1 {
        label.to_uppercase()
    } else {
        label
    }
}

/// Fila de controles clickeables: la línea a dibujar y, por botón, la columna
/// inicial (relativa a la fila) y su ancho. Los chips se ajustan al ancho
/// disponible (`max_width`): los que no entran completos se omiten enteros,
/// así nunca queda un botón recortado a la mitad ni una hit-zone fuera de la
/// fila (con el panel abierto el reproductor se queda con el 64% del ancho).
fn transport_row(
    playing: bool,
    shuffle: bool,
    repeat: RepeatMode,
    max_width: u16,
) -> (Line<'static>, Vec<(Button, u16, u16)>) {
    let transport = Style::default()
        .fg(theme().text)
        .bg(theme().select_bg)
        .add_modifier(Modifier::BOLD);
    let toggle = |active: bool| {
        Style::default()
            .fg(if active { theme().accent } else { theme().dim })
            .bg(theme().select_bg)
    };
    let repeat_label = match repeat.label() {
        Some(label) => format!("repetir: {label}"),
        None => "repetir".to_string(),
    };
    // (botón, texto, estilo, separación antes). Glifos de ancho simple para
    // que el área clickeable coincida con lo que se ve.
    let chips: [(Button, String, Style, &str); 5] = [
        (Button::Prev, "◀◀".to_string(), transport, "  "),
        (
            Button::PlayPause,
            if playing { "▮▮" } else { "▶" }.to_string(),
            transport,
            "  ",
        ),
        (Button::Next, "▶▶".to_string(), transport, "  "),
        (Button::Shuffle, "aleatorio".to_string(), toggle(shuffle), "     "),
        (Button::Repeat, repeat_label, toggle(repeat.label().is_some()), "  "),
    ];

    let mut spans = Vec::new();
    let mut buttons = Vec::new();
    let mut column: u16 = 0;
    for (button, label, style, gap) in chips {
        let gap_w = gap.chars().count() as u16;
        let chip = format!(" {label} ");
        let chip_w = chip.chars().count() as u16;
        let next_column = column.saturating_add(gap_w).saturating_add(chip_w);
        if next_column > max_width {
            // El chip no entra completo: se omite entero (y lo que sigue).
            break;
        }
        spans.push(Span::raw(gap));
        column += gap_w;
        buttons.push((button, column, chip_w));
        spans.push(Span::styled(chip, style));
        column += chip_w;
    }
    (Line::from(spans), buttons)
}

/// Columna inicial (relativa) y ancho del tramo interno de una barra hecha
/// con `build_bar`, o `None` si no entra.
fn bar_geometry(label: &str, suffix: &str, total_width: usize) -> Option<(u16, u16)> {
    let label_len = label.chars().count();
    // Etiqueta + " [" + "] " + sufijo; la barra necesita al menos 3 celdas.
    let overhead = label_len + 2 + 2 + suffix.chars().count();
    if total_width <= overhead + 2 {
        return None;
    }
    let offset = u16::try_from(label_len + 2).ok()?;
    let width = u16::try_from(total_width - overhead).ok()?;
    Some((offset, width))
}

/// Traduce un evento de mouse a una acción según las zonas del frame y el
/// arrastre en curso; devuelve también el nuevo estado de arrastre.
fn mouse_action(hit: &HitMap, drag: Option<Drag>, event: MouseEvent) -> (MouseAction, Option<Drag>) {
    let (column, row) = (event.column, event.row);
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let point = ratatui::layout::Position::new(column, row);
            if let Some((_, button)) = hit.buttons.iter().find(|(area, _)| area.contains(point)) {
                (MouseAction::Press(*button), None)
            } else if let Some((_, key)) = hit.chips.iter().find(|(area, _)| area.contains(point)) {
                (MouseAction::Key(*key), None)
            } else if let Some(bar) = hit.progress.filter(|bar| bar.contains(column, row)) {
                (MouseAction::SeekRatio(bar.ratio(column)), Some(Drag::Progress(None)))
            } else if let Some(bar) = hit.volume.filter(|bar| bar.contains(column, row)) {
                (MouseAction::SetVolume(bar.ratio(column)), Some(Drag::Volume))
            } else {
                (MouseAction::None, None)
            }
        }
        // El progreso solo marca la posición mientras se arrastra (el seek va
        // al soltar); el volumen sí sigue en vivo.
        MouseEventKind::Drag(MouseButton::Left) => match (drag, hit.progress, hit.volume) {
            (Some(Drag::Progress(_)), Some(bar), _) => {
                (MouseAction::None, Some(Drag::Progress(Some(bar.ratio(column)))))
            }
            (Some(Drag::Volume), _, Some(bar)) => (MouseAction::SetVolume(bar.ratio(column)), drag),
            _ => (MouseAction::None, drag),
        },
        MouseEventKind::Up(MouseButton::Left) => match drag {
            Some(Drag::Progress(Some(ratio))) => (MouseAction::SeekRatio(ratio), None),
            _ => (MouseAction::None, None),
        },
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let over_player = hit.player.is_some_and(|area| {
                area.contains(ratatui::layout::Position::new(column, row))
            });
            if !over_player {
                return (MouseAction::None, drag);
            }
            let step = if event.kind == MouseEventKind::ScrollUp { 1 } else { -1 };
            (MouseAction::VolumeStep(step), drag)
        }
        _ => (MouseAction::None, drag),
    }
}

/// Aplica un evento de mouse (clic/arrastre en barras, rueda) si el estado
/// vivo `mouse_enabled` (arranca con `[ui] mouse`, el toggle lo cambia)
/// lo permite.
fn handle_mouse(event: MouseEvent, app: &mut App, engine: &mut AudioEngine) {
    if !app.mouse_enabled {
        return;
    }
    // Con un modal abierto (prompt de URL, menú de temas, selector de
    // bandas, selector de modo, panel de configuración) el clic no atraviesa
    // hacia lo que quedó tapado.
    if app.url_input.is_some()
        || app.theme_menu.is_some()
        || app.bands_menu.is_some()
        || app.mode_menu.is_some()
        || app.settings_panel.is_some()
    {
        app.drag = None;
        return;
    }
    let (action, drag) = mouse_action(&app.hit.borrow(), app.drag, event);
    app.drag = drag;
    match action {
        MouseAction::None => {}
        MouseAction::SeekRatio(ratio) => seek_to_ratio(app, engine, ratio),
        MouseAction::SetVolume(volume) => engine.set_volume((volume as f32).clamp(0.0, 1.0)),
        MouseAction::VolumeStep(step) => engine.set_volume(
            (engine.volume() + f32::from(step) * VOLUME_STEP).clamp(0.0, 1.0),
        ),
        MouseAction::Press(Button::Prev) => play_step(app, engine, -1),
        MouseAction::Press(Button::PlayPause) => toggle_play(app, engine),
        MouseAction::Press(Button::Next) => play_step(app, engine, 1),
        MouseAction::Press(Button::Shuffle) => toggle_shuffle(app),
        MouseAction::Press(Button::Repeat) => app.repeat = app.repeat.cycle(),
        MouseAction::Key(code) => {
            // El chip «Ctrl+P: config» registra la tecla de `settings`: al
            // clickearlo se sintetiza con Ctrl (la acción lo exige); el resto
            // de los chips va sin modificador, como siempre.
            let modifiers = if config::pressed(&config::config().keys.settings, code) {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            };
            handle_key(KeyEvent::new(code, modifiers), app, engine)
        }
    }
}

/// Seek a una fracción de la duración (clic/arrastre en la barra de progreso).
fn seek_to_ratio(app: &mut App, engine: &mut AudioEngine, ratio: f64) {
    if !app.has_player {
        return;
    }
    let Some(total) = engine.duration().or_else(|| current_track_duration(app)) else {
        return;
    };
    match engine.seek(total.mul_f64(ratio.clamp(0.0, 1.0))) {
        Ok(()) => app.error = None,
        Err(err) => app.error = Some(err.to_string()),
    }
}

/// Ventana para confirmar la salida con un segundo Esc.
const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuitDecision {
    Arm,
    Quit,
}

/// Doble pulsación para salir: el primer «quit» arma la guarda y muestra el
/// aviso; un segundo dentro de `QUIT_CONFIRM_WINDOW` sale. Cualquier otra tecla
/// (o el vencimiento de la ventana) la desarma. El reloj llega por parámetro
/// para poder testear la ventana sin esperar.
#[derive(Debug, Default)]
struct QuitGuard {
    armed_at: Option<Instant>,
    /// Marca si la tecla que se está procesando pasó por la guarda; si no,
    /// `handle_key` la desarma al terminar.
    touched: bool,
}

impl QuitGuard {
    fn on_quit_key(&mut self, now: Instant) -> QuitDecision {
        self.touched = true;
        if self.is_armed(now) {
            self.armed_at = None;
            QuitDecision::Quit
        } else {
            self.armed_at = Some(now);
            QuitDecision::Arm
        }
    }

    fn disarm(&mut self) {
        self.armed_at = None;
    }

    fn is_armed(&self, now: Instant) -> bool {
        self.armed_at
            .is_some_and(|armed| now.saturating_duration_since(armed) <= QUIT_CONFIRM_WINDOW)
    }
}

/// Pedido de salida por teclado: pasa por la guarda de doble Esc. (Ctrl+C
/// sigue saliendo de inmediato.)
fn request_quit(app: &mut App) {
    if app.quit_guard.on_quit_key(Instant::now()) == QuitDecision::Quit {
        app.should_quit = true;
    }
}

/// Estado genérico del sub-modal de selección (Bandas y Modo; Temas usa el
/// mismo esquema salvo sus dos originales): selección con previsualización en
/// vivo y el valor con el que se abrió (para el marcador ● y para restaurar
/// con Esc). Agregar un selector nuevo es instanciar este estado y agregar
/// data al handler/render genéricos, no copiar el estado.
struct MenuState<T> {
    /// Índice en la lista de opciones.
    selected: usize,
    /// Valor activo al abrir (fila «actual» marcada con ●).
    original: T,
}

/// Estado del menú de temas: selección con previsualización en vivo y
/// el tema con el que se abrió (para el marcador y para restaurar con Esc).
/// Conserva su forma concreta (captura dos originales: id + paleta); el
/// handler, las filas y el render corren por los genéricos.
struct ThemeMenuState {
    /// Índice en `config::builtin_themes()`.
    selected: usize,
    /// Id del tema activo al abrir (fila «actual» marcada con ●).
    original_name: String,
    /// Colores activos al abrir: Esc los restaura.
    original_theme: Theme,
}

/// Selector de bandas: instancia del estado genérico sobre los 3 valores
/// fijos de estilo.
type BandsMenuState = MenuState<config::VisualizerStyle>;

/// Selector de modo: instancia del estado genérico sobre los 5 valores
/// fijos de modo.
type ModeMenuState = MenuState<config::VisualizerMode>;

/// Posición de un sub-modal de selección: el único acceso que el handler
/// genérico necesita del estado (además de la toma del `Option`). Lo
/// implementan `MenuState<T>` y `ThemeMenuState`, que conserva su forma
/// concreta para los tests.
trait MenuSelection {
    fn selected(&self) -> usize;

    fn set_selected(&mut self, selected: usize);
}

impl<T> MenuSelection for MenuState<T> {
    fn selected(&self) -> usize {
        self.selected
    }

    fn set_selected(&mut self, selected: usize) {
        self.selected = selected;
    }
}

impl MenuSelection for ThemeMenuState {
    fn selected(&self) -> usize {
        self.selected
    }

    fn set_selected(&mut self, selected: usize) {
        self.selected = selected;
    }
}

/// Filas fijas del panel de configuración: Temas abre su sub-modal,
/// Bandas el suyo, Mouse su toggle y Modo
/// el suyo. El orden es estable: la cuarta fila va AL FINAL, sin reordenar
/// las existentes.
const SETTINGS_ROWS: [&str; 4] = ["Temas", "Mouse", "Bandas", "Modo"];

/// Estado del panel de configuración (shell sin contenido): selección por
/// índice sobre `SETTINGS_ROWS`.
struct SettingsPanelState {
    /// Índice en `SETTINGS_ROWS`.
    selected: usize,
}

/// Guard de terminal: raw mode + alternate screen vía `ratatui::try_init`.
/// La restauración está garantizada por `Drop` (y por el panic hook que instala `try_init`).
struct Tui {
    terminal: ratatui::DefaultTerminal,
    restored: bool,
}

impl Tui {
    fn new() -> io::Result<Self> {
        match ratatui::try_init() {
            Ok(terminal) => {
                let _ = crossterm::execute!(io::stdout(), crossterm::event::EnableBracketedPaste);
                if config::config().mouse {
                    let _ = crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture);
                }
                Ok(Self {
                    terminal,
                    restored: false,
                })
            }
            Err(err) => {
                // try_init puede fallar con raw mode ya activo; revertir igualmente.
                let _ = ratatui::try_restore();
                Err(err)
            }
        }
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
        let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture);
        let _ = self.terminal.show_cursor();
        let _ = ratatui::try_restore();
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Loop principal de la TUI. Sale con la terminal restaurada y el audio detenido.
pub fn run(engine: &mut AudioEngine, start: StartMode) -> io::Result<()> {
    let is_url_start = matches!(start, StartMode::PlayUrl { .. });
    let (mode, roots, current_path, has_player, track_label, start_url_info) = match start {
        StartMode::PlayFile(path) => {
            let root = path
                .parent()
                .filter(|parent| parent.is_dir())
                .map(Path::to_path_buf)
                .unwrap_or_else(fallback_cwd);
            let label = path.to_string_lossy().into_owned();
            (Mode::Player, vec![root], Some(path), true, label, None)
        }
        StartMode::PlayUrl { url, kind, roots } => {
            let label = url_display_label(&url);
            (Mode::Player, roots, None, true, label, Some((url, kind)))
        }
        StartMode::Browse(roots) => (Mode::Browser, roots, None, false, String::new(), None),
    };

    let (history, history_warn) = crate::history::load();
    let mut initial_error = (!config::config().warnings.is_empty())
        .then(|| config::config().warnings.join(" · "));
    if let Some(warn) = history_warn {
        initial_error = Some(match initial_error {
            Some(prev) => format!("{prev} · {warn}"),
            None => warn,
        });
    }

    // La biblioteca se escanea en background: con miles de archivos el parseo
    // de tags (lofty, I/O por archivo) no puede congelar el hilo de la UI.
    let mut app = App {
        mode,
        explorer_open: false,
        roots,
        library: None,
        scan_rx: None,
        view: Vec::new(),
        selected: 0,
        filter: None,
        search: None,
        shuffle: false,
        repeat: RepeatMode::Off,
        shuffle_order: Vec::new(),
        queue: Vec::new(),
        queue_cursor: None,
        queue_resolving: None,
        queue_advance: QueueAdvance::NaturalEnd,
        queue_panel: false,
        queue_selected: 0,
        next_up: None,
        ui_frame: 0,
        current_path,
        has_player,
        track_label,
        status: status_of(engine),
        // Avisos de la config  e historial: en la primera pantalla, además del
        // stderr que ya imprimió main. Con config limpia queda `None`.
        error: initial_error,
        should_quit: false,
        analyzer: SpectrumAnalyzer::new(),
        url_input: None,
        ytdlp_rx: None,
        playlist_rx: None,
        ytdlp_request_id: 0,
        is_url: is_url_start,
        current_url: None,
        ytdlp_failed: false,
        history,
        pending_history: None,
        history_path: crate::history::history_path(),
        theme_menu: None,
        bands_menu: None,
        mode_menu: None,
        settings_panel: None,
        // El estado vivo arranca con lo configurado (el toggle lo cambia en
        // el momento y lo persiste para el próximo arranque).
        mouse_enabled: config::config().mouse,
        quit_guard: QuitGuard::default(),
        hit: RefCell::new(HitMap::default()),
        drag: None,
    };

    if let Some((url, kind)) = start_url_info {
        start_boot_url(&mut app, engine, url, kind);
    }
    start_scan(&mut app);
    let mut tui = Tui::new()?;

    let result = event_loop(&mut tui, &mut app, engine);

    engine.stop();
    tui.restore();
    result
}

fn fallback_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn event_loop(tui: &mut Tui, app: &mut App, engine: &mut AudioEngine) -> io::Result<()> {
    loop {
        // Error en curso del decoder o del device (archivo corrupto a
        // mitad, device desconectado): se muestra una vez y corta el
        // auto-advance de fin natural — la pista no terminó, falló. Se lee
        // ANTES de la transición de estado: el hilo de decode escribe el
        // error primero que el state, así que si el estado ya cambió, el
        // error está disponible.
        let playback_error = engine.take_error();
        if let Some(err) = &playback_error {
            app.error = Some(err.to_string());
        }

        // La transición Playing → Ended es el fin natural de la pista: ahí
        // decide repeat (uno la relanza, todo salta y envuelve).
        let previous_status = app.status;
        let playback_failed = engine.had_playback_error() || playback_error.is_some() || app.ytdlp_failed;
        let snapshot = EngineSnapshot::from_engine(engine, playback_failed);
        app.status = apply_engine_snapshot(app, snapshot);
        if playback_error.is_none()
            && previous_status == Status::Playing
            && app.status == Status::Ended
        {
            handle_natural_end(app, engine);
        }
        // Un seek fallido en el decodificador se toma una sola vez para
        // mostrarlo; no corta la reproducción.
        if let Some(err) = engine.take_seek_error() {
            app.error = Some(err.to_string());
        }
        poll_scan(app);

        // Listado de playlist (`--flat-playlist`): al llegar, encola el resto
        // de la lista y resuelve la primera canción (S12).
        if let Some(rx) = &app.playlist_rx {
            match rx.try_recv() {
                Ok((req_id, _orig_playlist_url, result)) => {
                    app.playlist_rx = None;
                    handle_playlist_result(app, req_id, result, &mut spawn_ytdlp_resolution);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    app.playlist_rx = None;
                }
            }
        }

        if let Some(rx) = &app.ytdlp_rx {
            match rx.try_recv() {
                Ok((req_id, orig_url, result)) => {
                    app.ytdlp_rx = None;
                    // Historia: la URL original que se guarda es la de la
                    // sesión actual (`current_url`), que para la primera de
                    // una playlist es la URL de la playlist (S12); para un
                    // replay el `current_url` ya quedó reh aplicado. El valor
                    // que devuelve el hilo solo se usa como respaldo si no
                    // hubiera sesión (no debería ocurrir en esta ruta).
                    let original = app
                        .current_url
                        .as_ref()
                        .map(|(original, _)| original.clone())
                        .unwrap_or(orig_url);
                    if let Some((audio_url, title)) = handle_ytdlp_result(app, req_id, result) {
                        // Resolución OK: ya no es resolución de ítem de cola.
                        app.queue_resolving = None;
                        start_url_playback(app, engine, audio_url, original, title);
                    } else if req_id == app.ytdlp_request_id {
                        // Una URL encolada falló al resolver su turno: se
                        // saltea con aviso y se sigue con el próximo (S13).
                        skip_failed_queue_item(app, engine, &mut dispatch_url_replay);
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    app.ytdlp_rx = None;
                }
            }
        }

        // Lock corto: solo copiar muestras del tap; la FFT corre después, sin lock.
        {
            let tap = engine.visualizer();
            let mut tap = tap.lock().unwrap();
            app.analyzer.ingest(&mut tap);
        }
        app.analyzer.analyze();
        // Bailarina (modo `miku`): la pose avanza por frame según la energía
        // de graves (o por timer en pausa).
        app.analyzer.tick_dance(app.status == Status::Playing);
        // Reloj del render y franja «Próxima canción» (T1): se actualizan por
        // frame para el marquee y para no calcular el próximo en el draw.
        app.ui_frame = app.ui_frame.wrapping_add(1);
        refresh_next_up(app);

        tui.terminal.draw(|frame| render(frame, app, engine))?;

        if !event::poll(FRAME_INTERVAL)? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                handle_key(key, app, engine);
                if app.should_quit {
                    break;
                }
            }
            Event::Mouse(mouse) => handle_mouse(mouse, app, engine),
            Event::Paste(pasted) if app.url_input.is_some() => {
                if let Some(buf) = app.url_input.as_mut() {
                    handle_prompt_paste(buf, &pasted);
                }
            }
            _ => {}
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineSnapshot {
    pub is_connecting: bool,
    pub is_playing: bool,
    pub is_paused: bool,
    pub had_playback_error: bool,
}

impl EngineSnapshot {
    pub fn from_engine(engine: &AudioEngine, app_error: bool) -> Self {
        Self {
            is_connecting: engine.is_connecting(),
            is_playing: engine.is_playing(),
            is_paused: engine.is_paused(),
            had_playback_error: engine.had_playback_error() || app_error,
        }
    }
}

pub fn compute_status(pending_resolution: bool, snapshot: EngineSnapshot) -> Status {
    if pending_resolution {
        Status::Resolving
    } else if snapshot.had_playback_error {
        Status::Stopped
    } else if snapshot.is_connecting {
        Status::Connecting
    } else if snapshot.is_paused {
        Status::Paused
    } else if snapshot.is_playing {
        Status::Playing
    } else {
        Status::Ended
    }
}

fn init_url_playback(app: &mut App, original_url: String, label: String) {
    cancel_pending_ytdlp(app);
    app.is_url = true;
    app.current_path = None;
    app.track_label = label.clone();
    app.has_player = true;
    app.status = Status::Connecting;
    app.error = None;
    app.current_url = Some((original_url.clone(), label.clone()));
    app.pending_history = Some((original_url, label));
}

fn apply_engine_snapshot(app: &mut App, snapshot: EngineSnapshot) -> Status {
    let pending_resolution = app.ytdlp_rx.is_some();
    let new_status = compute_status(pending_resolution, snapshot);
    let previous_status = app.status;
    app.status = new_status;

    if snapshot.had_playback_error || new_status == Status::Stopped {
        app.pending_history = None;
    } else if previous_status == Status::Connecting
        && new_status == Status::Playing
        && app.is_url
        && !snapshot.had_playback_error
        && let Some((original_url, label)) = app.pending_history.take()
    {
        record_history_entry(app, &original_url, &label);
    }
    new_status
}

fn status_of(engine: &AudioEngine) -> Status {
    compute_status(false, EngineSnapshot::from_engine(engine, false))
}

fn handle_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    app.quit_guard.touched = false;
    dispatch_key(key, app, engine);
    if !app.quit_guard.touched {
        app.quit_guard.disarm();
    }
}

fn dispatch_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        app.should_quit = true;
        return;
    }

    if app.url_input.is_some() {
        handle_url_prompt_key(key, app, engine);
        return;
    }

    // Menú de temas: con el menú abierto las teclas van al menú —vale igual
    // como sub-modal del panel, que sigue abierto debajo y retoma al
    // cerrar el sub-modal con Enter/Esc—.
    if app.theme_menu.is_some() {
        handle_theme_menu_key(key, app, config::persist_theme_name);
        return;
    }

    // Selector de bandas: sub-modal del panel con la misma prioridad
    // que el de temas (nunca están abiertos los dos a la vez).
    if app.bands_menu.is_some() {
        handle_bands_menu_key(key, app, config::persist_visualizer_style);
        return;
    }

    // Selector de modo: sub-modal del panel con la
    // misma prioridad que los otros dos (nunca hay dos abiertos a la vez).
    if app.mode_menu.is_some() {
        handle_mode_menu_key(key, app, config::persist_visualizer_mode);
        return;
    }

    // Panel de configuración: con el panel abierto las teclas van al
    // panel (los modales son excluyentes: el primero que está abierto gana).
    if app.settings_panel.is_some() {
        handle_settings_panel_key(key, app, config::persist_mouse_enabled);
        return;
    }

    // Con la búsqueda abierta todo lo demás se escribe en el query: ni 'q'
    // sale ni las flechas mueven la selección.
    if app.search.is_some() {
        handle_search_key(key, app);
        return;
    }

    match app.mode {
        // Panel lateral abierto: la lista manda las flechas, pero los
        // atajos de reproducción (pausa, seek, n/p…) siguen activos.
        Mode::Player if app.explorer_open => handle_list_key(key, app, engine),
        Mode::Player => handle_player_key(key, app, engine),
        Mode::Browser => handle_list_key(key, app, engine),
    }
}

fn handle_player_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    let keys = &config::config().keys;
    let code = key.code;
    // `settings` va primero: exige Ctrl (`pressed_ctrl`), así que la letra
    // suelta nunca cae acá y el resto del chain queda intacto (`p` = previa).
    if config::pressed_ctrl(&keys.settings, key) {
        open_settings_panel(app);
    } else if config::pressed(&keys.quit, code) {
        request_quit(app);
    } else if config::pressed(&keys.play_pause, code) {
        toggle_play(app, engine);
    } else if config::pressed(&keys.volume_up, code) {
        engine.set_volume((engine.volume() + VOLUME_STEP).clamp(0.0, 1.0));
    } else if config::pressed(&keys.volume_down, code) {
        engine.set_volume((engine.volume() - VOLUME_STEP).clamp(0.0, 1.0));
    } else if config::pressed(&keys.seek_back, code) {
        seek_by(app, engine, -1);
    } else if config::pressed(&keys.seek_fwd, code) {
        seek_by(app, engine, 1);
    } else if config::pressed(&keys.speed_down, code) {
        step_speed(app, engine, -1.0);
    } else if config::pressed(&keys.speed_up, code) {
        step_speed(app, engine, 1.0);
    } else if config::pressed(&keys.explorer, code) {
        toggle_explorer(app);
    } else if config::pressed(&keys.next_track, code) {
        play_step(app, engine, 1);
    } else if config::pressed(&keys.prev_track, code) {
        play_step(app, engine, -1);
    } else if config::pressed(&keys.shuffle, code) {
        toggle_shuffle(app);
    } else if config::pressed(&keys.repeat, code) {
        app.repeat = app.repeat.cycle();
    } else if config::pressed(&keys.open_url, code) {
        open_url_prompt(app);
    } else if config::pressed(&keys.search, code) {
        toggle_explorer(app);
        open_search(app);
    }
}

/// Teclas de la lista del explorador: navegar y reproducir. Con el panel
/// lateral abierto corre en `Mode::Player` y suma los atajos de
/// reproducción (pausa, seek, n/p, velocidad…) sobre la lista; con el
/// explorador a pantalla completa (sin player) es el modo sin sesión de audio.
fn handle_list_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    let keys = &config::config().keys;
    let view_len = app.view.len();
    let queue_len = app.queue.len();
    let code = key.code;
    // `settings` va primero (igual que en el player): exige Ctrl, la letra
    // suelta nunca cae acá.
    if config::pressed_ctrl(&keys.settings, key) {
        open_settings_panel(app);
    // El orden importa: «back» antes que «quit» — Esc cierra el panel (o
    // sale del explorador sin player) aunque Esc también esté en «quit».
    // Tab no está en «back»: cae en «explorer» y no-opea sin player.
    } else if config::pressed(&keys.back, code) {
        go_back(app);
    } else if config::pressed(&keys.explorer, code) {
        toggle_explorer(app);
    } else if config::pressed(&keys.quit, code) {
        request_quit(app);
    } else if config::pressed(&keys.queue_panel, code) {
        // Alterna la solapa del panel lateral (Biblioteca | Cola): cada lista
        // conserva su propia selección (`selected` / `queue_selected`).
        app.queue_panel = !app.queue_panel;
    } else if config::pressed(&keys.select, code) {
        if app.queue_panel {
            play_selected_queue_item(app, engine);
        } else {
            play_selected(app, engine);
        }
    } else if config::pressed(&keys.enqueue, code) {
        // Encolar solo tiene sentido sobre la lista Biblioteca.
        if !app.queue_panel {
            enqueue_selected(app);
        }
    } else if config::pressed(&keys.queue_delete, code) {
        if app.queue_panel {
            delete_selected_queue_item(app);
        }
    } else if config::pressed(&keys.play_pause, code) {
        toggle_play(app, engine);
    } else if config::pressed(&keys.next_track, code) {
        play_step(app, engine, 1);
    } else if config::pressed(&keys.prev_track, code) {
        play_step(app, engine, -1);
    } else if config::pressed(&keys.shuffle, code) {
        toggle_shuffle(app);
    } else if config::pressed(&keys.repeat, code) {
        app.repeat = app.repeat.cycle();
    } else if config::pressed(&keys.open_url, code) {
        open_url_prompt(app);
    } else if config::pressed(&keys.search, code) {
        open_search(app);
    } else if config::pressed(&keys.move_up, code) {
        if app.queue_panel {
            app.queue_selected = move_selection(app.queue_selected, -1, queue_len);
        } else {
            app.selected = move_selection(app.selected, -1, view_len);
        }
    } else if config::pressed(&keys.move_down, code) {
        if app.queue_panel {
            app.queue_selected = move_selection(app.queue_selected, 1, queue_len);
        } else {
            app.selected = move_selection(app.selected, 1, view_len);
        }
    } else if config::pressed(&keys.seek_back, code) {
        seek_by(app, engine, -1);
    } else if config::pressed(&keys.seek_fwd, code) {
        seek_by(app, engine, 1);
    } else if config::pressed(&keys.speed_down, code) {
        step_speed(app, engine, -1.0);
    } else if config::pressed(&keys.speed_up, code) {
        step_speed(app, engine, 1.0);
    } else if config::pressed(&keys.page_up, code) {
        if app.queue_panel {
            app.queue_selected = move_selection(app.queue_selected, -PAGE_STEP, queue_len);
        } else {
            app.selected = move_selection(app.selected, -PAGE_STEP, view_len);
        }
    } else if config::pressed(&keys.page_down, code) {
        if app.queue_panel {
            app.queue_selected = move_selection(app.queue_selected, PAGE_STEP, queue_len);
        } else {
            app.selected = move_selection(app.selected, PAGE_STEP, view_len);
        }
    } else if config::pressed(&keys.home, code) {
        if app.queue_panel {
            app.queue_selected = move_selection(app.queue_selected, isize::MIN, queue_len);
        } else {
            app.selected = move_selection(app.selected, isize::MIN, view_len);
        }
    } else if config::pressed(&keys.end, code) {
        if app.queue_panel {
            app.queue_selected = move_selection(app.queue_selected, isize::MAX, queue_len);
        } else {
            app.selected = move_selection(app.selected, isize::MAX, view_len);
        }
    }
}

/// Modo búsqueda incremental: '/' lo abre, y mientras está activo la lista se
/// filtra en vivo. Enter confirma el filtro, Esc cancela y restaura la lista
/// completa.
fn handle_search_key(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Esc => {
            app.search = None;
            app.filter = None;
        }
        KeyCode::Enter => {
            let text = app.search.take().unwrap_or_default();
            app.filter = (!text.trim().is_empty()).then_some(text);
        }
        KeyCode::Backspace => {
            if let Some(query) = app.search.as_mut() {
                query.pop();
            }
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(query) = app.search.as_mut() {
                query.push(c);
            }
        }
        _ => {}
    }
    refresh_view(app);
}

fn open_search(app: &mut App) {
    // El texto arranca del filtro actual para poder editarlo.
    app.search = Some(app.filter.clone().unwrap_or_default());
    refresh_view(app);
}

fn library_get(app: &App, index: usize) -> Option<&Track> {
    app.library.as_ref().and_then(|library| library.get(index))
}

fn view_get(app: &App, index: usize) -> Option<&Track> {
    match app.view.get(index)? {
        ViewItem::Track(library_index) => library_get(app, *library_index),
        ViewItem::History(_) => None,
    }
}

/// Posición de un path dentro de la lista visible.
fn position_in_view(app: &App, path: &Path) -> Option<usize> {
    let library_index = app.library.as_ref()?.index_of(path)?;
    app.view.iter().position(|item| match item {
        ViewItem::Track(index) => *index == library_index,
        ViewItem::History(_) => false,
    })
}

/// Posición de la pista sonando dentro de la lista visible (la que recorren
/// next/previous con shuffle y repeat).
fn current_view_position(app: &App) -> Option<usize> {
    let path = app.current_path.as_ref()?;
    position_in_view(app, path)
}

fn matches_history_filter(entry: &crate::history::HistoryEntry, needle: &str) -> bool {
    let trimmed = needle.trim().to_lowercase();
    if trimmed.is_empty() {
        return true;
    }
    let title = if entry.title.is_empty() {
        &entry.url
    } else {
        &entry.title
    };
    title.to_lowercase().contains(&trimmed)
}

/// Recalcula la lista visible (biblioteca filtrada por búsqueda/filtro) sin
/// perder la selección: se preserva la pista seleccionada por path, si ya no
/// existe se usa la sonando y si ninguna está en la vista se acota a rango.
/// Con shuffle activo regenera el orden mezclado para la nueva lista.
fn refresh_view(app: &mut App) {
    refresh_view_with_hint(app, None);
}

fn refresh_view_with_hint(app: &mut App, preserved_history_url: Option<&str>) {
    let previous_item = app.view.get(app.selected).copied();
    let previous_path = previous_item.and_then(|item| match item {
        ViewItem::Track(library_index) => library_get(app, library_index).map(|track| track.path.clone()),
        ViewItem::History(_) => None,
    });
    let previous_history_url = preserved_history_url
        .map(String::from)
        .or_else(|| {
            previous_item.and_then(|item| match item {
                ViewItem::History(hist_idx) => app.history.get(hist_idx).map(|entry| entry.url.clone()),
                ViewItem::Track(_) => None,
            })
        });

    let needle = app
        .search
        .as_ref()
        .or(app.filter.as_ref())
        .map(String::as_str);

    let mut new_view = Vec::new();
    for (idx, entry) in app.history.entries().iter().enumerate() {
        if matches_history_filter(entry, needle.unwrap_or_default()) {
            new_view.push(ViewItem::History(idx));
        }
    }
    if let Some(library) = &app.library {
        for track_idx in filter_tracks(library.tracks(), needle) {
            new_view.push(ViewItem::Track(track_idx));
        }
    }
    app.view = new_view;

    let new_selected = if let Some(url) = &previous_history_url {
        app.view.iter().position(|item| match item {
            ViewItem::History(idx) => app.history.get(*idx).map(|entry| &entry.url) == Some(url),
            _ => false,
        })
    } else if let Some(path) = &previous_path {
        position_in_view(app, path)
    } else {
        None
    }
    .or_else(|| current_view_position(app))
    .unwrap_or(0)
    .min(app.view.len().saturating_sub(1));
    app.selected = new_selected;

    if app.shuffle {
        // El orden vive sobre el pool MIXTO (pistas + historial), igual que en
        // `toggle_shuffle` y en `pool_positions_in_view`: si el refresco lo
        // dejara con solo las pistas, quedaría a medio armar hasta el próximo
        // salto (D2/S6).
        let pool_count = app
            .view
            .iter()
            .filter(|item| {
                matches!(item, ViewItem::Track(_)) || matches!(item, ViewItem::History(_))
            })
            .count();
        app.shuffle_order = shuffle_order(pool_count);
    }
}

/// Ubica la selección en la pista que está sonando (si está en la biblioteca).
fn resync_selection(app: &mut App) {
    if let Some(position) = current_view_position(app) {
        app.selected = position;
    }
}

fn open_url_prompt(app: &mut App) {
    app.url_input = Some(String::new());
}

fn handle_url_prompt_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
    {
        // El pegado del portapapeles llega vía Event::Paste o como secuencia de caracteres del terminal.
        return;
    }

    match key.code {
        KeyCode::Esc => {
            app.url_input = None;
        }
        KeyCode::Enter => {
            let input = app.url_input.take().unwrap_or_default();
            submit_url_prompt(input, app, engine);
        }
        KeyCode::Backspace => {
            if let Some(buf) = app.url_input.as_mut() {
                handle_prompt_backspace(buf);
            }
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(buf) = app.url_input.as_mut() {
                handle_prompt_char(buf, c);
            }
        }
        _ => {}
    }
}

fn cancel_pending_ytdlp(app: &mut App) {
    app.ytdlp_request_id = app.ytdlp_request_id.wrapping_add(1);
    app.ytdlp_rx = None;
    app.playlist_rx = None;
    app.queue_resolving = None;
    app.ytdlp_failed = false;
    app.pending_history = None;
}

// --- Sub-modal de selección (Temas, Bandas y Modo) --------------------

/// Handler genérico del sub-modal de selección: ↑/↓ mueve y previsualiza
/// en vivo sin persistir, Enter aplica y persiste a través del closure
/// recibido (un fallo de guardado solo deja el aviso) y Esc restaura el
/// valor capturado al abrir (`original`/preview-restore). `slot` localiza
/// el campo `Option` del estado en la `App`; `len` + `item` resuelven la
/// lista, `preview`/`apply` (que devuelve el argumento a persistir)/`restore`
/// son la data de cada selector: agregar uno nuevo es agregar un wrapper
/// con esa data, no otra copia del handler.
fn handle_menu_key<T: MenuSelection, I>(
    app: &mut App,
    key: KeyEvent,
    slot: impl Fn(&mut App) -> &mut Option<T>,
    len: usize,
    item: impl Fn(usize) -> I,
    preview: impl Fn(&I),
    apply: impl Fn(&I) -> String,
    restore: impl Fn(&T),
    persist: impl FnOnce(&str) -> Result<(), String>,
) {
    match key.code {
        KeyCode::Up | KeyCode::Down => {
            let Some(menu) = slot(app).as_mut() else {
                return;
            };
            let delta = if matches!(key.code, KeyCode::Up) { -1 } else { 1 };
            menu.set_selected(move_selection(menu.selected(), delta, len));
            let value = item(menu.selected());
            // Previsualización en vivo: cambia el valor sin persistir.
            preview(&value);
        }
        KeyCode::Enter => {
            let Some(menu) = slot(app).take() else {
                return;
            };
            let value = item(menu.selected());
            // Aplica primero (queda en la sesión aunque falle el guardado)
            // y recién después persiste; un fallo solo deja el aviso.
            let persist_arg = apply(&value);
            if let Err(err) = persist(&persist_arg) {
                app.error = Some(err);
            }
        }
        KeyCode::Esc => {
            let Some(menu) = slot(app).take() else {
                return;
            };
            // Restaura el valor con el que se abrió.
            restore(&menu);
        }
        _ => {}
    }
}

// --- Menú de temas ----------------------------------------------------

fn open_theme_menu(app: &mut App) {
    let name = runtime::active_theme_name();
    let selected = config::builtin_themes()
        .iter()
        .position(|(id, _)| *id == name)
        .unwrap_or(0);
    app.theme_menu = Some(ThemeMenuState {
        selected,
        original_name: name,
        original_theme: runtime::active_theme(),
    });
}

fn handle_theme_menu_key(
    key: KeyEvent,
    app: &mut App,
    persist: impl FnOnce(&str) -> Result<(), String>,
) {
    handle_menu_key(
        app,
        key,
        |app| &mut app.theme_menu,
        config::builtin_themes().len(),
        |i| config::builtin_themes()[i].0,
        |id| runtime::preview_theme(config::resolve_theme(id)),
        |id| {
            runtime::activate_theme(id, config::resolve_theme(id));
            // D1: aplicar el tema Miku también activa la bailarina (el default
            // de arranque lo cubre parse(); acá el cambio es en vivo).
            if *id == "miku" {
                runtime::activate_visualizer_mode(config::VisualizerMode::Miku);
            }
            (*id).to_string()
        },
        |menu| runtime::activate_theme(&menu.original_name, menu.original_theme.clone()),
        persist,
    );
}

// --- Selector de bandas (sub-modal del panel) ---------------------------

fn open_bands_menu(app: &mut App) {
    let current = runtime::active_visualizer_style();
    let selected = config::VisualizerStyle::options()
        .iter()
        .position(|style| *style == current)
        .unwrap_or(0);
    app.bands_menu = Some(BandsMenuState {
        selected,
        original: current,
    });
}

fn handle_bands_menu_key(
    key: KeyEvent,
    app: &mut App,
    persist: impl FnOnce(&str) -> Result<(), String>,
) {
    handle_menu_key(
        app,
        key,
        |app| &mut app.bands_menu,
        config::VisualizerStyle::options().len(),
        |i| config::VisualizerStyle::options()[i],
        |style| runtime::preview_visualizer_style(*style),
        |style| {
            runtime::activate_visualizer_style(*style);
            style.as_str().to_string()
        },
        |menu| runtime::activate_visualizer_style(menu.original),
        persist,
    );
}

// --- Selector de modo (sub-modal del panel) ---------

fn open_mode_menu(app: &mut App) {
    let current = runtime::active_visualizer_mode();
    let selected = config::VisualizerMode::options()
        .iter()
        .position(|mode| *mode == current)
        .unwrap_or(0);
    app.mode_menu = Some(ModeMenuState {
        selected,
        original: current,
    });
}

fn handle_mode_menu_key(
    key: KeyEvent,
    app: &mut App,
    persist: impl FnOnce(&str) -> Result<(), String>,
) {
    handle_menu_key(
        app,
        key,
        |app| &mut app.mode_menu,
        config::VisualizerMode::options().len(),
        |i| config::VisualizerMode::options()[i],
        |mode| runtime::preview_visualizer_mode(*mode),
        |mode| {
            runtime::activate_visualizer_mode(*mode);
            mode.as_str().to_string()
        },
        |menu| runtime::activate_visualizer_mode(menu.original),
        persist,
    );
}

// --- Panel de configuración (shell) -------------------------------------

/// Aplica la captura del mouse en la terminal en el momento (efecto
/// inmediato del toggle; el cierre del panel no la toca: el toggle manda).
fn apply_mouse_capture(enabled: bool) {
    if enabled {
        let _ = crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture);
    } else {
        let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture);
    }
}

/// Alterna el mouse en vivo: cambia el estado, aplica la captura en el
/// momento y persiste `[ui] mouse`. Como Temas/Bandas, se aplica primero y
/// un fallo de guardado solo deja el aviso; el panel sigue abierto.
fn toggle_mouse(app: &mut App, persist: impl FnOnce(bool) -> Result<(), String>) {
    let enabled = !app.mouse_enabled;
    app.mouse_enabled = enabled;
    apply_mouse_capture(enabled);
    if let Err(err) = persist(enabled) {
        app.error = Some(err);
    }
}

fn open_settings_panel(app: &mut App) {
    app.settings_panel = Some(SettingsPanelState { selected: 0 });
}

fn handle_settings_panel_key(
    key: KeyEvent,
    app: &mut App,
    persist_mouse: impl FnOnce(bool) -> Result<(), String>,
) {
    match key.code {
        KeyCode::Up | KeyCode::Down => {
            let Some(panel) = app.settings_panel.as_mut() else {
                return;
            };
            // Sin envoltura, como el menú de temas (`move_selection` con clamp).
            let delta = if matches!(key.code, KeyCode::Up) { -1 } else { 1 };
            panel.selected = move_selection(panel.selected, delta, SETTINGS_ROWS.len());
        }
        // Enter en «Temas» abre el
        // ThemeMenuState existente como sub-modal, Enter en «Bandas» abre el
        // selector de estilos, Enter en «Modo» abre el selector de modos (el
        // panel sigue abierto debajo; el dispatch prioriza al sub-modal y
        // su Enter/Esc vuelven al panel) y Enter en «Mouse» alterna la
        // captura en el momento + persiste (el panel sigue abierto).
        KeyCode::Enter => {
            let selected = match app.settings_panel.as_ref() {
                Some(panel) => panel.selected,
                None => return,
            };
            if selected == 0 {
                open_theme_menu(app);
            } else if selected == 1 {
                toggle_mouse(app, persist_mouse);
            } else if selected == 2 {
                open_bands_menu(app);
            } else if selected == 3 {
                open_mode_menu(app);
            }
        }
        // Un solo Esc cierra el panel; como no toca la guarda de salida, el
        // doble-Esc para salir solo aplica sin modal (igual que con el menú).
        KeyCode::Esc => {
            app.settings_panel = None;
        }
        _ => {}
    }
}

/// Filas del sub-modal de selección: una por opción; ● marca la opción
/// activa al abrir y la selección va resaltada. Compartido por los tres
/// selectores (Temas, Bandas y Modo): cada wrapper resuelve sus filas como
/// (etiqueta, es-actual).
fn menu_lines(rows: impl IntoIterator<Item = (String, bool)>, selected: usize) -> Vec<Line<'static>> {
    let colors = theme(); // snapshot coherente para todas las filas
    rows.into_iter()
        .enumerate()
        .map(|(i, (label, mark))| {
            let marker = if mark { "● " } else { "  " };
            let row_style = if i == selected {
                Style::default()
                    .bg(colors.select_bg)
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colors.text)
            };
            Line::from(Span::styled(format!(" {marker}{label} "), row_style))
        })
        .collect()
}

/// Modal centrado del sub-modal de selección, mismo estilo para los tres
/// selectores: fondo limpio + borde acentuado con título y las filas ya
/// resueltas (`render_*_menu` son wrappers con título y filas de cada uno).
fn render_menu(frame: &mut Frame<'_>, title: &'static str, lines: Vec<Line<'static>>) {
    let area = frame.area();
    let width = 44.min(area.width.saturating_sub(4)).max(24);
    let height = (lines.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(3);

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    frame.render_widget(Clear, rect);

    let block = Block::bordered()
        .border_style(Style::default().fg(theme().accent))
        .title(Span::styled(
            title,
            Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Filas del sub-modal: una por estilo; ● marca el estilo activo al abrir.
fn bands_menu_lines(menu: &BandsMenuState) -> Vec<Line<'static>> {
    let selected = menu.selected;
    menu_lines(
        config::VisualizerStyle::options()
            .iter()
            .map(|style| (style.as_str().to_string(), *style == menu.original)),
        selected,
    )
}

/// Modal centrado del selector de bandas, mismo estilo que el de temas.
fn render_bands_menu(frame: &mut Frame<'_>, menu: &BandsMenuState) {
    render_menu(frame, " Bandas ", bands_menu_lines(menu));
}

/// Filas del sub-modal: una por modo; ● marca el modo activo al abrir.
fn mode_menu_lines(menu: &ModeMenuState) -> Vec<Line<'static>> {
    let selected = menu.selected;
    menu_lines(
        config::VisualizerMode::options()
            .iter()
            .map(|mode| (mode.as_str().to_string(), *mode == menu.original)),
        selected,
    )
}

/// Modal centrado del selector de modo, mismo estilo
/// que el de bandas.
fn render_mode_menu(frame: &mut Frame<'_>, menu: &ModeMenuState) {
    render_menu(frame, " Modo ", mode_menu_lines(menu));
}

/// Filas del modal: una por tema builtin; ● marca el tema activo al abrir.
fn theme_menu_lines(menu: &ThemeMenuState) -> Vec<Line<'static>> {
    let selected = menu.selected;
    menu_lines(
        config::builtin_themes()
            .iter()
            .map(|(id, _)| (id.to_string(), *id == menu.original_name)),
        selected,
    )
}

fn start_local_playback(app: &mut App, engine: &mut AudioEngine, path: PathBuf) -> bool {
    cancel_pending_ytdlp(app);
    app.is_url = false;
    app.current_url = None;
    let label = path.to_string_lossy().into_owned();
    match engine.play(&path) {
        Ok(()) => {
            app.current_path = Some(path);
            app.track_label = label;
            app.has_player = true;
            app.status = Status::Playing;
            app.error = None;
            true
        }
        Err(err) => {
            app.status = Status::Stopped;
            app.error = Some(err.to_string());
            false
        }
    }
}

fn record_history_entry(app: &mut App, original_url: &str, label: &str) {
    let selected_history_url = match app.view.get(app.selected) {
        Some(ViewItem::History(idx)) => app.history.get(*idx).map(|e| e.url.clone()),
        _ => None,
    };
    let clean_url = crate::history::sanitize_url(original_url);
    app.history.add(label.to_string(), clean_url);
    if let Some(path) = &app.history_path
        && let Err(err) = crate::history::save_to(&app.history, path)
    {
        app.error = Some(format!("Error al guardar historial: {err}"));
    }
    refresh_view_with_hint(app, selected_history_url.as_deref());
}

fn start_url_playback(
    app: &mut App,
    engine: &mut AudioEngine,
    playable_url: String,
    original_url: String,
    label: String,
) {
    init_url_playback(app, original_url, label);
    if let Err(err) = engine.play_url(&playable_url) {
        app.status = Status::Stopped;
        app.error = Some(err.to_string());
        app.pending_history = None;
    }
}

/// Único punto del programa que lanza el proceso yt-dlp para resolver una URL
/// de página: devuelve el receiver del resultado. Centralizarlo acá permite
/// que los tests inyecten un doble y nunca toquen el proceso ni la red.
fn spawn_ytdlp_resolution(url: String, req_id: u64) -> Receiver<YtDlpMessage> {
    let (tx, rx) = mpsc::channel();
    let url_clone = url.clone();
    thread::spawn(move || {
        let result =
            crate::source::ytdlp::resolve(&url, &crate::source::ytdlp::ProcessCommandRunner);
        let _ = tx.send((req_id, url_clone, result));
    });
    rx
}

/// Único punto del programa que lista una playlist de YouTube con yt-dlp
/// `--flat-playlist`: devuelve el receiver del resultado (mismo patrón de
/// seam que [`spawn_ytdlp_resolution`], sin tocar red en los tests).
fn spawn_ytdlp_listing(url: String, req_id: u64) -> Receiver<YtDlpListMessage> {
    let (tx, rx) = mpsc::channel();
    let url_clone = url.clone();
    thread::spawn(move || {
        let result =
            crate::source::ytdlp::list_flat(&url, &crate::source::ytdlp::ProcessCommandRunner);
        let _ = tx.send((req_id, url_clone, result));
    });
    rx
}

fn submit_url_prompt(input: String, app: &mut App, engine: &mut AudioEngine) {
    submit_url_prompt_with(input, app, engine, spawn_ytdlp_resolution)
}

/// Seam inyectable de [`submit_url_prompt`] (resolución): la rama de página
/// recibe el resolver (`FnOnce(url, req_id) -> receiver`), que en producción
/// es [`spawn_ytdlp_resolution`] y en tests un doble que no lanza yt-dlp.
/// El listado de playlist usa el seam de producción; los tests de playlist
/// usan [`submit_url_prompt_with_seams`] para inyectarlo aislado.
fn submit_url_prompt_with(
    input: String,
    app: &mut App,
    engine: &mut AudioEngine,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
) {
    submit_url_prompt_with_seams(input, app, engine, resolve_page, spawn_ytdlp_listing)
}

/// Camino único de URL (página y playlist) con ambos seams inyectables: la
/// resolución de página y el listado de playlist. El boot por argumento y el
/// prompt `u` pasan por acá para que el flujo de playlist sea idéntico y
/// hermético a los tests en los dos puntos de entrada (S12).
fn submit_url_prompt_with_seams(
    input: String,
    app: &mut App,
    engine: &mut AudioEngine,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) {
    match classify_submit_input(&input) {
        SubmitAction::Empty => {}
        SubmitAction::Local(path) => {
            if path.is_file() {
                if start_local_playback(app, engine, path.clone()) {
                    app.mode = Mode::Player;
                }
            } else {
                app.error = Some(format!("'{}' no existe o no es un archivo.", path.display()));
            }
        }
        SubmitAction::DirectAudio(url) | SubmitAction::Unknown(url) => {
            let label = url_display_label(&url);
            start_url_playback(app, engine, url.clone(), url, label);
            app.mode = Mode::Player;
        }
        SubmitAction::Page(url) | SubmitAction::Playlist(url) => {
            begin_url_flow(app, url, resolve_page, list_playlist);
        }
    }
}

/// Inicio del flujo de URL de página/playlist (unificado para el boot por
/// argumento y el prompt `u`): prepara la sesión de URL, valida el `request
/// id` pendiente y lanza la resolución (página) o el listado `--flat-playlist`
/// (playlist). En S12 la playlist se lista y la primera canción se resuelve
/// enseguida; el resto entra a la cola al final cuando llega el listado.
fn begin_url_flow(
    app: &mut App,
    url: String,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) {
    let is_playlist = matches!(
        crate::source::classify(&url),
        crate::source::Input::Url {
            kind: crate::source::UrlKind::Playlist,
            ..
        }
    );

    cancel_pending_ytdlp(app);
    app.is_url = true;
    app.current_path = None;
    app.track_label = url_display_label(&url);
    app.has_player = true;
    app.mode = Mode::Player;
    app.status = Status::Resolving;
    app.error = None;
    // La URL original queda guardada desde el input: si la pista que estaba
    // sonando termina mientras resuelve, One repite ésta. Para la primera de
    // una playlist, esta es la URL de la playlist que registra el historial.
    app.current_url = Some((url.clone(), url_display_label(&url)));

    let req_id = app.ytdlp_request_id;
    if is_playlist {
        app.playlist_rx = Some(list_playlist(url, req_id));
    } else {
        app.ytdlp_rx = Some(resolve_page(url, req_id));
    }
}

/// Arranque por argumento de CLI (`mp3tui "<url>"`): URL directa / desconocida
/// reproduce ya, y página / playlist pasan por el mismo flujo unificado de
/// [`begin_url_flow`] que el prompt `u`.
fn start_boot_url(app: &mut App, engine: &mut AudioEngine, url: String, kind: crate::source::UrlKind) {
    start_boot_url_with(app, engine, url, kind, spawn_ytdlp_resolution, spawn_ytdlp_listing)
}

/// Variante testeable de [`start_boot_url`] con los seams inyectables de
/// resolución y listado.
fn start_boot_url_with(
    app: &mut App,
    engine: &mut AudioEngine,
    url: String,
    kind: crate::source::UrlKind,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) {
    match kind {
        crate::source::UrlKind::DirectAudio | crate::source::UrlKind::Unknown => {
            let label = url_display_label(&url);
            start_url_playback(app, engine, url.clone(), url, label);
        }
        crate::source::UrlKind::Page | crate::source::UrlKind::Playlist => {
            begin_url_flow(app, url, resolve_page, list_playlist);
        }
    }
}

/// Lanza el escaneo de `app.roots` en un thread aparte (una o varias
/// carpetas, agrupadas por carpeta de origen). Si ya hay un escaneo en
/// curso no lanza otro (el resultado viejo sigue siendo válido).
fn start_scan(app: &mut App) {
    if app.scan_rx.is_some() {
        return;
    }
    let roots = app.roots.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let playlist = Playlist::from_dirs_scan(&roots);
        // Si la UI ya salió, el receiver está caído y el resultado se descarta.
        let _ = tx.send(playlist);
    });
    app.scan_rx = Some(rx);
}

/// Chequeo sin bloquear, una vez por frame (mismo espíritu que
/// ingest()/analyze() del visualizador): `try_recv` nunca espera.
fn poll_scan(app: &mut App) {
    let result = app.scan_rx.as_ref().map(|rx| rx.try_recv());
    match result {
        None | Some(Err(TryRecvError::Empty)) => {}
        Some(Ok(playlist)) => {
            app.library = Some(playlist);
            app.scan_rx = None;
            // Reaplica filtro y, si había shuffle activo, regenera el orden
            // mezclado; la selección se preserva por pista.
            refresh_view(app);
        }
        Some(Err(TryRecvError::Disconnected)) => {
            // El thread murió sin entregar: biblioteca vacía para no dejar la
            // UI en "Escaneando…" para siempre.
            app.library = Some(Playlist::new());
            app.scan_rx = None;
            refresh_view(app);
        }
    }
}

/// Play / pausa / relanzar al terminar la pista.
fn toggle_play(app: &mut App, engine: &mut AudioEngine) {
    if engine.is_playing() {
        engine.pause();
    } else if engine.is_paused() {
        engine.resume();
    } else if let Some(path) = app.current_path.clone() {
        start_local_playback(app, engine, path);
    }
}

/// Enter en la lista: reproduce la pista seleccionada. Con el explorador a
/// pantalla completa pasa al modo reproducción; con el panel lateral abierto
/// el panel se mantiene y el reproductor de fondo arranca la pista. Si
/// el archivo no se puede abrir, seguimos donde estábamos con el error visible.
fn play_selected(app: &mut App, engine: &mut AudioEngine) {
    let Some(item) = app.view.get(app.selected).copied() else {
        return;
    };
    match item {
        ViewItem::History(hist_idx) => {
            if let Some(entry) = app.history.get(hist_idx) {
                let url = entry.url.clone();
                submit_url_prompt(url, app, engine);
            }
        }
        ViewItem::Track(library_index) => {
            let Some(track) = library_get(app, library_index) else {
                return;
            };
            if start_local_playback(app, engine, track.path.clone()) {
                app.mode = Mode::Player;
            }
        }
    }
}

/// Encolar: suma el ítem seleccionado de la Biblioteca al final de la cola.
/// Una pista local va como `QueueItem::Local`; una fila del historial, como
/// `QueueItem::Url` (se resuelve recién cuando le toque el turno).
fn enqueue_selected(app: &mut App) {
    let Some(item) = app.view.get(app.selected).copied() else {
        return;
    };
    match item {
        ViewItem::Track(library_index) => {
            if let Some(track) = library_get(app, library_index) {
                app.queue.push(QueueItem::Local(track.clone()));
            }
        }
        ViewItem::History(hist_idx) => {
            if let Some(entry) = app.history.get(hist_idx) {
                let title = if entry.title.is_empty() {
                    entry.url.clone()
                } else {
                    entry.title.clone()
                };
                app.queue.push(QueueItem::Url {
                    url: entry.url.clone(),
                    title,
                });
            }
        }
    }
}

/// Enter en la Cola: mueve el cursor de reproducción al ítem seleccionado y
/// lo reproduce ya (resuelve la URL por el camino normal si es una URL
/// pendiente). El ítem no sale de la cola: queda marcado como actual, para
/// poder volver con `p` (S9).
fn play_selected_queue_item(app: &mut App, engine: &mut AudioEngine) {
    if app.queue.is_empty() {
        return;
    }
    let index = app.queue_selected.min(app.queue.len() - 1);
    if play_queue_from(app, engine, index, QueueAdvance::Step(1), &mut dispatch_url_replay) {
        app.mode = Mode::Player;
    }
}

/// Posición de la cola que sigue al cursor de reproducción: la de abajo
/// mientras nada de la cola haya sonado todavía. `None` cuando no queda nada
/// por delante (o la cola está vacía): ahí vale el fallback al pool de la
/// biblioteca/historial (S5/S6).
fn queue_next_position(app: &App) -> Option<usize> {
    let next = match app.queue_cursor {
        Some(cursor) => cursor.checked_add(1)?,
        None => 0,
    };
    (next < app.queue.len()).then_some(next)
}

/// Posición anterior a la del cursor de reproducción. `None` en la primera
/// posición y mientras nada de la cola haya sonado: la cola no tiene
/// «anterior» desde donde está parado.
fn queue_prev_position(app: &App) -> Option<usize> {
    app.queue_cursor?.checked_sub(1)
}

/// El «próximo» de la franja del panel lateral (T1/S3): el ítem que sigue al
/// cursor de la cola; con la cola agotada (o vacía), el siguiente que sonaría
/// del pool de la biblioteca/historial con repeat/shuffle como están; sin
/// nada → `None` (la franja no aparece). Se refresca una vez por frame en el
/// loop, para que el draw solo lea.
fn refresh_next_up(app: &mut App) {
    if let Some(index) = queue_next_position(app) {
        app.next_up = Some(app.queue[index].label().to_string());
        return;
    }
    let pool = pool_positions_in_view(app);
    if pool.is_empty() {
        app.next_up = None;
        return;
    }
    let current = finished_item_pool_index(app, &pool);
    let Some(target_idx) = next_track(
        pool.len(),
        current,
        app.repeat,
        app.shuffle,
        &app.shuffle_order,
        1,
    ) else {
        app.next_up = None;
        return;
    };
    let label = match app.view[pool[target_idx]] {
        ViewItem::Track(library_index) => {
            library_get(app, library_index).map(|track| track.title.clone())
        }
        ViewItem::History(history_index) => {
            app.history.get(history_index).map(|entry| {
                if entry.title.is_empty() {
                    entry.url.clone()
                } else {
                    entry.title.clone()
                }
            })
        }
    };
    app.next_up = Some(label.unwrap_or_default());
}

/// Reproduce desde `start` hacia adelante de la cola saltando los ítems
/// locales que no abren (S13): aviso visible y se sigue con el próximo. Si el
/// ítem es una URL se arranca su resolución (asíncrona, esperando su turno,
/// S3) y se registra `queue_resolving` para saltearla si falla. Mueve el
/// cursor a la posición que quedó en marcha. Devuelve `false` cuando la cola
/// quedó agotada (entonces vale el fallback de S5/S6).
fn play_queue_from(
    app: &mut App,
    engine: &mut AudioEngine,
    start: usize,
    advance: QueueAdvance,
    replay: &mut impl FnMut(&mut App, &mut AudioEngine, UrlReplayRequest),
) -> bool {
    let mut index = start;
    let mut skipped = false;
    while let Some(item) = app.queue.get(index).cloned() {
        match item {
            QueueItem::Url { url, .. } => {
                // Se resuelve recién ahora: las URLs resueltas vencen. El aviso
                // de un salto previo se fija DESPUÉS de arrancar, porque los
                // flujos de reproducción limpian el error al comenzar (S13).
                replay(app, engine, UrlReplayRequest { original_url: url });
                app.queue_cursor = Some(index);
                app.queue_resolving = Some(index);
                app.queue_advance = advance;
                if skipped {
                    app.error = Some(QUEUE_ITEM_FAILED_MSG.to_string());
                }
                return true;
            }
            QueueItem::Local(track) => {
                if start_local_playback(app, engine, track.path) {
                    app.queue_cursor = Some(index);
                    app.queue_resolving = None;
                    if skipped {
                        // Igual que arriba: el aviso va después del arranque.
                        app.error = Some(QUEUE_ITEM_FAILED_MSG.to_string());
                    }
                    return true;
                }
                // No abrió: se saltea con aviso (S13).
                skipped = true;
                index += 1;
            }
        }
    }
    if skipped {
        app.error = Some(QUEUE_ITEM_FAILED_MSG.to_string());
    }
    false
}

/// Ítem encolado que falló al resolver justo cuando le tocaba el turno (S13):
/// se saltea con aviso visible — el cursor avanza al próximo ítem de la cola y
/// lo intenta (los locales que tampoco abran se saltean igual, con aviso); si
/// la cola se agota, vale el fallback de S5/S6. El ítem fallido queda en la
/// cola y el cursor lo pasa (queda atenuado al sonar lo que sigue).
fn skip_failed_queue_item(
    app: &mut App,
    engine: &mut AudioEngine,
    replay: impl FnMut(&mut App, &mut AudioEngine, UrlReplayRequest),
) {
    let Some(failed) = app.queue_resolving.take() else {
        return;
    };
    let mut replay = replay;
    let advance = app.queue_advance;
    let started = play_queue_from(app, engine, failed + 1, advance, &mut replay);
    if !started {
        // Cola agotada: fallback al pool. Siempre hacia la misma dirección
        // del disparador para no volver sobre un ítem recién fallido (garantía
        // de terminación del bucle de salto).
        let step = match advance {
            QueueAdvance::NaturalEnd => 1,
            QueueAdvance::Step(step) => step,
        };
        if step >= 0 {
            queue_exhausted_pool_fallback(app, engine, step, &mut replay);
        } else {
            queue_exhausted_pool_fallback(app, engine, -1, &mut replay);
        }
    }
    // El aviso queda visible aunque haya arrancado el siguiente (S13).
    app.error = Some(QUEUE_ITEM_FAILED_MSG.to_string());
}

/// Fallback de S5/S6 cuando la cola se agotó tras saltar ítems fallidos:
/// avanza el pool de la biblioteca/historial en `step` (`1` hacia adelante,
/// `-1` hacia atrás). Sin hacer nada si el pool está vacío o no hay destino.
fn queue_exhausted_pool_fallback(
    app: &mut App,
    engine: &mut AudioEngine,
    step: isize,
    replay: &mut impl FnMut(&mut App, &mut AudioEngine, UrlReplayRequest),
) {
    let pool = pool_positions_in_view(app);
    if pool.is_empty() {
        return;
    }
    let current = finished_item_pool_index(app, &pool);
    let Some(target_idx) =
        next_track(pool.len(), current, app.repeat, app.shuffle, &app.shuffle_order, step)
    else {
        return;
    };
    play_pool_target(app, engine, pool[target_idx], replay);
}

/// Borra el ítem seleccionado de la Cola y acota la selección al nuevo alto.
/// El cursor de reproducción se acomoda para seguir apuntando al mismo ítem:
/// si se borra el que estaba sonando desde la cola, el cursor vuelve a `None`
/// (nada de la cola está sonando).
fn delete_selected_queue_item(app: &mut App) {
    if app.queue.is_empty() {
        return;
    }
    let index = app.queue_selected.min(app.queue.len() - 1);
    let cursor_was = app.queue_cursor.filter(|&c| c < app.queue.len());
    app.queue.remove(index);
    app.queue_cursor = match cursor_was {
        Some(c) if index < c => Some(c - 1),
        Some(c) if index == c => None,
        _ => app.queue_cursor,
    };
    app.queue_selected = app.queue_selected.min(app.queue.len().saturating_sub(1));
}

/// Posiciones de la vista del pool de navegación (lo que recorren `n`/`p` y
/// el fin natural): siempre las pistas de la biblioteca, y además las
/// entradas del historial cuando el shuffle está activo (S6/D2), para que una
/// pista local pueda encadenar con un tema de YouTube y viceversa. Con el
/// shuffle apagado el pool queda como antes (solo pistas locales). Regenera
/// el orden de shuffle si la lista cambió desde que se armó (filtro/rescann).
fn pool_positions_in_view(app: &mut App) -> Vec<usize> {
    let pool: Vec<usize> = app
        .view
        .iter()
        .enumerate()
        .filter_map(|(pos, item)| match item {
            ViewItem::Track(_) => Some(pos),
            ViewItem::History(_) if app.shuffle => Some(pos),
            ViewItem::History(_) => None,
        })
        .collect();
    if app.shuffle && !pool.is_empty() && app.shuffle_order.len() != pool.len() {
        app.shuffle_order = shuffle_order(pool.len());
    }
    pool
}

/// Índice dentro del pool (`pool_positions_in_view`) del ítem que acaba de
/// terminar: para una pista local es el pool de `current_view_position(app)`;
/// para una URL (`app.is_url`) es el pool de la entrada del historial visible
/// cuya URL coincide con la original de `app.current_url` (`None` si esa URL no
/// está en el historial visible, lo que conserva la caída de antes).
fn finished_item_pool_index(app: &App, pool: &[usize]) -> Option<usize> {
    let view_pos = if app.is_url {
        let original_url = app.current_url.as_ref().map(|(url, _label)| url)?;
        app.view
            .iter()
            .position(|item| match item {
                ViewItem::History(idx) => {
                    app.history.get(*idx).map(|entry| &entry.url) == Some(original_url)
                }
                ViewItem::Track(_) => false,
            })?
    } else {
        current_view_position(app)?
    };
    pool.iter().position(|&pos| pos == view_pos)
}

/// Arranca la pista de una posición de la vista por el único camino de
/// reproducción local (limpia `is_url`/`current_url` y pendientes de URL,
/// sincroniza biblioteca y selección). Devuelve false si esa posición no es
/// una pista o el archivo no pudo abrirse.
fn play_view_position(app: &mut App, engine: &mut AudioEngine, target: usize) -> bool {
    let Some(ViewItem::Track(library_index)) = app.view.get(target).copied() else {
        return false;
    };
    let Some(track) = library_get(app, library_index) else {
        return false;
    };
    if start_local_playback(app, engine, track.path.clone()) {
        app.selected = target;
        true
    } else {
        false
    }
}

/// Arranca el objetivo del pool de navegación (el «siguiente» de `n`/`p` o
/// del fin natural). Para una pista local usa [`play_view_position`]; para
/// una entrada del historial la re-resuelve por el camino inyectable `replay`
/// (en producción re-pide la página con yt-dlp y en los tests se captura sin
/// red). Devuelve false si la posición no es jugable.
fn play_pool_target(
    app: &mut App,
    engine: &mut AudioEngine,
    target: usize,
    replay: &mut impl FnMut(&mut App, &mut AudioEngine, UrlReplayRequest),
) -> bool {
    match app.view.get(target).copied() {
        Some(ViewItem::History(hist_idx)) => {
            if let Some(entry) = app.history.get(hist_idx) {
                replay(
                    app,
                    engine,
                    UrlReplayRequest {
                        original_url: entry.url.clone(),
                    },
                );
                true
            } else {
                false
            }
        }
        Some(ViewItem::Track(_)) => play_view_position(app, engine, target),
        None => false,
    }
}

/// 'n' / 'p': salta a la siguiente/anterior pista de la playlist visible,
/// respetando shuffle y repeat, sin salir del modo actual (en el explorador
/// además mueve la selección). No envuelve con repeat Off/One.
fn play_step(app: &mut App, engine: &mut AudioEngine, step: isize) {
    play_step_with(app, engine, step, dispatch_url_replay)
}

/// Idéntico a [`play_step`] pero con el replay de URL inyectable (mismo
/// patrón que [`handle_natural_end`]/[`handle_natural_end_with`]): en
/// producción la URL se resuelve con yt-dlp y en los tests se captura sin
/// lanzar procesos ni tocar la red.
fn play_step_with(
    app: &mut App,
    engine: &mut AudioEngine,
    step: isize,
    mut replay: impl FnMut(&mut App, &mut AudioEngine, UrlReplayRequest),
) {
    // La cola manda (D1/S5/S9): `n` y `p` recorren el cursor de la cola en
    // ambos sentidos antes de caer al pool de la biblioteca/historial.
    let queue_start = if step > 0 {
        queue_next_position(app)
    } else {
        queue_prev_position(app)
    };
    if let Some(index) = queue_start
        && play_queue_from(app, engine, index, QueueAdvance::Step(step), &mut replay)
    {
        return;
    }
    let pool = pool_positions_in_view(app);
    if pool.is_empty() {
        return;
    }
    let current = finished_item_pool_index(app, &pool);
    let Some(target_idx) = next_track(
        pool.len(),
        current,
        app.repeat,
        app.shuffle,
        &app.shuffle_order,
        step,
    ) else {
        return;
    };
    play_pool_target(app, engine, pool[target_idx], &mut replay);
}

fn toggle_shuffle(app: &mut App) {
    app.shuffle = !app.shuffle;
    if app.shuffle {
        // El orden vive sobre el pool (pistas + historial), igual que el
        // pool que se usa al navegar / reproducir con el shuffle activo.
        let count = app
            .view
            .iter()
            .filter(|item| {
                matches!(item, ViewItem::Track(_)) || matches!(item, ViewItem::History(_))
            })
            .count();
        app.shuffle_order = shuffle_order(count);
    }
}

/// ← / →: seek relativo (±5 s) sobre la pista que está sonando. Sin pista en
/// reproducción no hace nada; el motor valida que haya sesión activa.
fn seek_by(app: &mut App, engine: &mut AudioEngine, direction: i64) {
    if !app.has_player {
        return;
    }
    let duration = engine.duration().or_else(|| current_track_duration(app));
    let target = seek_target(engine.position(), direction, duration);
    match engine.seek(target) {
        Ok(()) => app.error = None,
        Err(err) => app.error = Some(err.to_string()),
    }
}

/// `[` / `]`: baja o sube la velocidad en pasos de 0.25x (0.5x..2.0x). Sin
/// pista en reproducción no hace nada, igual que el seek.
fn step_speed(app: &App, engine: &mut AudioEngine, direction: f32) {
    if !app.has_player {
        return;
    }
    engine.set_speed(engine.speed() + direction * SPEED_STEP);
}

/// Duración declarada en los metadatos de la pista actual (fallback cuando el
/// contenedor no reporta la suya).
fn current_track_duration(app: &App) -> Option<Duration> {
    let path = app.current_path.as_ref()?;
    let library = app.library.as_ref()?;
    let index = library.index_of(path)?;
    library.get(index)?.duration
}

/// Orden pura de relanzamiento de URL que pide el fin natural con repetir
/// «una»: la URL original de la sesión. La ejecuta el dispatcher inyectable
/// de [`handle_natural_end_with`] (en producción, [`dispatch_url_replay`]);
/// los tests la capturan sin lanzar yt-dlp ni tocar la red.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlReplayRequest {
    original_url: String,
}

/// Fin natural de la pista (transición Playing → Ended): decide el repeat
/// sin esperar que el usuario apriete espacio. La regla única de avance vive
/// en [`natural_end_action`] y vale igual para pistas locales y para URL.
fn handle_natural_end(app: &mut App, engine: &mut AudioEngine) {
    handle_natural_end_with(app, engine, dispatch_url_replay)
}

/// Efecto real del replay de URL: re-lanza la URL ORIGINAL por el camino
/// normal de URL (las páginas se resuelven de nuevo con yt-dlp porque las
/// URLs resueltas vencen).
fn dispatch_url_replay(app: &mut App, engine: &mut AudioEngine, request: UrlReplayRequest) {
    submit_url_prompt(request.original_url, app, engine);
}

/// Seam inyectable del fin natural: el replay de URL pasa por `replay` en
/// lugar de disparar yt-dlp directamente, de modo que un test puede capturar
/// la orden y verificarla sin procesos ni red.
fn handle_natural_end_with(
    app: &mut App,
    engine: &mut AudioEngine,
    mut replay: impl FnMut(&mut App, &mut AudioEngine, UrlReplayRequest),
) {
    // La cola manda: mientras quede algo después del cursor, el fin de pista
    // avanza el cursor a esa posición y la reproduce, sin aplicar `repeat` ni
    // `shuffle` (S5/S9). Agotada la cola, valen las reglas de siempre.
    if let Some(index) = queue_next_position(app) {
        let local = matches!(app.queue.get(index), Some(QueueItem::Local(_)));
        if play_queue_from(app, engine, index, QueueAdvance::NaturalEnd, &mut replay) {
            if local {
                // La selección sigue a la pista que arrancó, igual que
                // cuando el "siguiente" sale de la biblioteca.
                resync_selection(app);
            }
            return;
        }
    }

    let pool = pool_positions_in_view(app);
    let current = finished_item_pool_index(app, &pool);

    match natural_end_action(
        app.repeat,
        app.shuffle,
        pool.len(),
        current,
        &app.shuffle_order,
        app.is_url,
        engine.position(),
    ) {
        NextAction::Replay => {
            if app.is_url {
                // Re-lanza la URL ORIGINAL por el camino normal de URL: las
                // páginas se resuelven de nuevo con yt-dlp porque las URLs
                // resueltas vencen. Sin URL original guardada no hay qué
                // repetir.
                if let Some((original_url, _label)) = app.current_url.clone() {
                    replay(
                        app,
                        engine,
                        UrlReplayRequest { original_url },
                    );
                }
            } else if let Some(path) = app.current_path.clone() {
                start_local_playback(app, engine, path);
            }
        }
        NextAction::Play(target_idx) => {
            // Un objetivo mixto (pista local o URL del historial) se arranca
            // por el mismo camino único del pool (`play_pool_target`): la URL
            // se re-resuelve por el seam inyectable `replay`.
            if let Some(&target) = pool.get(target_idx) {
                play_pool_target(app, engine, target, &mut replay);
            }
        }
        NextAction::Stop => {}
        NextAction::StopTooSoon => {
            // URL repetir «una» que terminó bajo el mínimo: no se re-lanza
            // (evita el bucle yt-dlp + red) y se avisa en la línea de estado.
            app.error = Some(URL_REPLAY_TOO_SOON_MSG.to_string());
        }
    }
}

/// Acción de «back» (Esc) desde la lista: con el panel lateral abierto
/// lo cierra y deja el reproductor de fondo; con el explorador a
/// pantalla completa vuelve al player si hay sesión y si no sale de la app.
fn go_back(app: &mut App) {
    if app.explorer_open {
        app.explorer_open = false;
        app.queue_panel = false;
    } else if app.has_player {
        app.mode = Mode::Player;
    } else {
        request_quit(app);
    }
}

/// 'b' / Tab / Ctrl+Tab: alterna el panel lateral del explorador.
/// Al abrirlo re-sincroniza la selección con la pista sonando y reescanea
/// las raíces en background (si ya hay un escaneo en curso, no lanza otro).
/// Sin sesión de audio no hace nada: el explorador ya ocupa toda la
/// pantalla (así Tab sin player no sale de la app).
fn toggle_explorer(app: &mut App) {
    if !app.has_player {
        return;
    }
    app.explorer_open = !app.explorer_open;
    if app.explorer_open {
        refresh_view(app);
        resync_selection(app);
        start_scan(app);
    } else {
        // Al cerrar el panel vuelve siempre a la solapa Biblioteca.
        app.queue_panel = false;
    }
}

fn render(frame: &mut Frame<'_>, app: &App, engine: &AudioEngine) {
    // Las zonas clickeables se vuelven a anotar en cada frame.
    *app.hit.borrow_mut() = HitMap::default();
    match app.mode {
        Mode::Player => render_player(frame, app, engine),
        Mode::Browser => render_browser(frame, app),
    }

    if let Some(buf) = &app.url_input {
        render_url_prompt(frame, buf);
    }

    if let Some(panel) = &app.settings_panel {
        render_settings_panel(frame, panel, app.mouse_enabled);
    }

    // El sub-modal de bandas  va encima del panel.
    if let Some(menu) = &app.bands_menu {
        render_bands_menu(frame, menu);
    }

    // El sub-modal de modo  va encima del panel, como
    // el de bandas (nunca están abiertos los dos a la vez).
    if let Some(menu) = &app.mode_menu {
        render_mode_menu(frame, menu);
    }

    // El sub-modal de temas  va encima del panel: se dibuja último.
    if let Some(menu) = &app.theme_menu {
        render_theme_menu(frame, menu);
    }
}

fn render_url_prompt(frame: &mut Frame<'_>, input: &str) {
    let area = frame.area();
    let prompt_width = 72.min(area.width.saturating_sub(4)).max(20);
    let prompt_height = 5.min(area.height.saturating_sub(2)).max(3);

    let x = area.x + (area.width.saturating_sub(prompt_width)) / 2;
    let y = area.y + (area.height.saturating_sub(prompt_height)) / 2;
    let prompt_rect = Rect::new(x, y, prompt_width, prompt_height);

    frame.render_widget(Clear, prompt_rect);

    let block = Block::bordered()
        .border_style(Style::default().fg(theme().accent))
        .title(Span::styled(
            " Abrir URL / Archivo ",
            Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(prompt_rect);
    frame.render_widget(block, prompt_rect);

    let display_text = format!(" {input}▌");
    let line = Line::from(Span::styled(display_text, Style::default().fg(theme().text)));
    frame.render_widget(Paragraph::new(vec![line]), inner);
}

/// Modal centrado de temas, al estilo del prompt de URL.
fn render_theme_menu(frame: &mut Frame<'_>, menu: &ThemeMenuState) {
    render_menu(frame, " Temas ", theme_menu_lines(menu));
}

/// Modal centrado del panel, con el mismo estilo del modal de temas:
/// fondo limpio + borde acentuado con título.
fn render_settings_panel(
    frame: &mut Frame<'_>,
    panel: &SettingsPanelState,
    mouse_enabled: bool,
) {
    let area = frame.area();
    let width = 44.min(area.width.saturating_sub(4)).max(24);
    let height = (SETTINGS_ROWS.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(3);

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    frame.render_widget(Clear, rect);

    let block = Block::bordered()
        .border_style(Style::default().fg(theme().accent))
        .title(Span::styled(
            " Configuración ",
            Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(settings_panel_lines(panel, mouse_enabled)), inner);
}

/// Filas del panel («Mouse» muestra el estado vivo; el resto fijas): la
/// selección va resaltada como en el menú de temas.
fn settings_panel_lines(
    panel: &SettingsPanelState,
    mouse_enabled: bool,
) -> Vec<Line<'static>> {
    let colors = theme(); // snapshot coherente para todas las filas
    SETTINGS_ROWS
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let label = if i == 1 {
                format!("Mouse: {}", if mouse_enabled { "sí" } else { "no" })
            } else {
                (*row).to_string()
            };
            let style = if i == panel.selected {
                Style::default()
                    .bg(colors.select_bg)
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colors.text)
            };
            Line::from(Span::styled(format!(" {label} "), style))
        })
        .collect()
}

/// Bloque con borde apagado y título acentuado: el patrón de todo el programa.
fn themed_block<'a>(title: Span<'a>) -> Block<'a> {
    Block::bordered()
        .border_style(Style::default().fg(theme().dim))
        .title(title)
}

fn accent_title(text: &str) -> Span<'static> {
    Span::styled(
        format!(" {text} "),
        Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
    )
}

/// Iconos Unicode en los estados de reproducción.
fn status_parts(status: Status) -> (&'static str, Color) {
    match status {
        Status::Playing => ("▶  Reproduciendo", theme().ok),
        Status::Paused  => ("⏸  Pausado",       theme().warn),
        Status::Ended   => ("⏹  Fin de pista",  theme().dim),
        Status::Stopped => ("⏹  Detenido",      theme().error),
        Status::Resolving => ("⏳  Resolviendo con yt-dlp…", theme().accent),
        Status::Connecting => ("⏳  Conectando…", theme().accent),
    }
}

fn status_line(status: Status, repeat: RepeatMode, shuffle: bool) -> Line<'static> {
    let (text, color) = status_parts(status);
    let mut spans = vec![
        Span::styled("Estado:  ", Style::default().fg(theme().dim)),
        Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(repeat_shuffle_spans(repeat, shuffle));
    Line::from(spans)
}

/// Indicadores de estado compartidos: solo lo que está activo ("repeat: todo",
/// "shuffle: sí"), así en los defaults la pantalla queda limpia.
fn repeat_shuffle_spans(repeat: RepeatMode, shuffle: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if let Some(label) = repeat.label() {
        spans.push(Span::styled("  ·  ", Style::default().fg(theme().dim)));
        spans.push(Span::styled(
            format!("repeat: {label}"),
            Style::default().fg(theme().accent),
        ));
    }
    if shuffle {
        spans.push(Span::styled("  ·  ", Style::default().fg(theme().dim)));
        spans.push(Span::styled(
            "shuffle: sí",
            Style::default().fg(theme().accent),
        ));
    }
    spans
}

fn error_line(app: &App) -> Line<'static> {
    if app.quit_guard.is_armed(Instant::now()) {
        let keys = &config::config().keys;
        return Line::from(Span::styled(
            format!("Apretá {} de nuevo para salir", key_labels(&keys.quit)),
            Style::default().fg(theme().warn),
        ));
    }
    match &app.error {
        Some(err) => Line::from(Span::styled(
            format!("Error:  {err}"),
            Style::default().fg(theme().error),
        )),
        None => Line::raw(""),
    }
}

/// Áreas del reproductor: info (8 filas), visualizador (resto) y pie. Con el
/// panel lateral abierto info + visualizador se achican al 64% del
/// ancho y el explorador ocupa el 36% derecho superpuesto (más barras
/// visibles pero el espectro sigue animando); el pie crece a 5 filas (3 de
/// texto) para los atajos del panel y sigue a todo lo ancho para no cortarse.
///
/// Footer comprimible — si `area.height < 30`, el pie se reduce a
/// 2 filas (1 de borde + 1 de contenido) independientemente del panel.
fn player_areas(area: Rect, explorer_open: bool) -> (Rect, Rect, Rect, Option<Rect>) {
    // Umbral de compresión: con exactamente 30 filas se usa el layout completo.
    let compact = area.height < 30;
    let footer_height = if compact {
        2
    } else if explorer_open {
        5
    } else {
        4
    };

    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9),
            Constraint::Min(4),
            Constraint::Length(footer_height),
        ])
        .split(area);
    let footer = zones[2];
    if !explorer_open {
        return (zones[0], zones[1], footer, None);
    }
    let upper = Rect {
        height: zones[0].height + zones[1].height,
        ..zones[0]
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(64),
            Constraint::Percentage(36),
        ])
        .split(upper);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9),
            Constraint::Min(4),
        ])
        .split(columns[0]);
    (left[0], left[1], footer, Some(columns[1]))
}

fn render_player(frame: &mut Frame<'_>, app: &App, engine: &AudioEngine) {
    let (info_area, visual_area, footer_area, panel) =
        player_areas(frame.area(), app.explorer_open);
    app.hit.borrow_mut().player = Some(info_area.union(visual_area));

    // El foco visual va al panel cuando está abierto; info y
    // visualizador quedan con borde apagado en ese caso.
    let player_focused = !app.explorer_open;

    render_player_info(frame, info_area, app, engine, player_focused);
    render_visualizer(frame, visual_area, &app.analyzer, player_focused);
    if let Some(panel_area) = panel {
        render_explorer_panel(frame, panel_area, app);
    }
    let compact = frame.area().height < 30;
    render_footer(frame, footer_area, app, compact);
}

/// Panel lateral del explorador: la biblioteca superpuesta sobre el
/// costado derecho, con el mismo bloque, título, scroll y marcadores de
/// selección/reproducción que el explorador a pantalla completa.
fn render_explorer_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    render_library(frame, area, app, true);
}

fn render_player_info(frame: &mut Frame<'_>, area: Rect, app: &App, engine: &AudioEngine, focused: bool) {
    // Borde acentuado cuando el player tiene el foco.
    let border_color = if focused { theme().accent } else { theme().dim };
    let block = Block::bordered()
        .border_style(Style::default().fg(border_color))
        .title(accent_title("mp3tui"));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Buscar metadatos del track actual en la biblioteca.
    let track_meta: Option<(String, Option<String>)> = app.current_path.as_ref()
        .and_then(|path| app.library.as_ref()?.index_of(path))
        .and_then(|idx| app.library.as_ref()?.get(idx))
        .and_then(|track| {
            track.artist.as_ref().map(|a| (a.clone(), track.album.clone()))
        });

    // La fila de velocidad aparece solo cuando no es 1.0x: en velocidad
    // normal el panel conserva su forma y el espacio no muestra ruido.
    let show_speed = (engine.speed() - 1.0).abs() > f32::EPSILON;
    let show_meta = track_meta.is_some();
    // La cola solo se anuncia cuando queda algo por delante del cursor; si no,
    // no agrega ruido (S9/S10).
    let show_queue = queue_next_position(app).is_some();

    // Bloque inicial: archivo + (meta opcional) + estado + (cola opcional) + error.
    let first_block_lines = if show_meta { 4 } else { 3 } + if show_queue { 1 } else { 0 };

    // Archivo/estado, volumen, progreso y la fila de controles clickeables.
    let mut constraints = vec![
        Constraint::Length(first_block_lines),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ];
    if show_speed {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let file_line = Line::from(vec![
        Span::styled("Archivo: ", Style::default().fg(theme().dim)),
        Span::styled(app.track_label.as_str(), Style::default().fg(theme().text)),
    ]);

    let mut info_lines = vec![file_line];

    // Línea de artista/álbum (solo si hay artista conocido).
    if let Some((artist, album_opt)) = &track_meta {
        let mut spans = vec![
            Span::styled("Meta:    ", Style::default().fg(theme().dim)),
            Span::styled(artist.clone(), Style::default().fg(theme().text)),
        ];
        if let Some(album) = album_opt {
            spans.push(Span::styled("  ·  ", Style::default().fg(theme().dim)));
            spans.push(Span::styled(album.clone(), Style::default().fg(theme().dim)));
        }
        info_lines.push(Line::from(spans));
    }

    info_lines.push(status_line(app.status, app.repeat, app.shuffle));

    // Lo próximo que suena: el ítem que sigue al cursor de la cola, solo
    // cuando queda algo por delante (S9/S10). El título usa `QueueItem::label`
    // (igual que la lista de la Cola) y el (+N) cuenta cuánto falta detrás.
    let next_queued = queue_next_position(app);
    if let Some(index) = next_queued {
        let next = app.queue[index].label();
        let count = app.queue.len() - 1 - index;
        let value = if count > 0 {
            format!("{next} (+{count})")
        } else {
            next.to_string()
        };
        info_lines.push(Line::from(vec![
            Span::styled("Siguiente: ", Style::default().fg(theme().dim)),
            Span::styled(value, Style::default().fg(theme().text)),
        ]));
    }

    info_lines.push(error_line(app));

    frame.render_widget(Paragraph::new(info_lines), rows[0]);

    let mut row = 1;
    if show_speed {
        frame.render_widget(Paragraph::new(speed_line(engine)), rows[row]);
        row += 1;
    }

    // Barras de volumen y progreso con caracteres ━/╸/░.
    let volume = engine.volume().clamp(0.0, 1.0);
    let percent = (volume * 100.0).round() as u16;
    let vol_suffix = format!("{percent}%");
    frame.render_widget(
        Paragraph::new(build_bar(
            "Volumen  ",
            volume as f64,
            &vol_suffix,
            theme().accent,
            theme().dim,
            inner.width as usize,
            theme().buffered,
            &[],
        )),
        rows[row],
    );
    if let Some((offset, width)) = bar_geometry("Volumen  ", &vol_suffix, inner.width as usize) {
        app.hit.borrow_mut().volume = Some(BarHit {
            x: rows[row].x + offset,
            y: rows[row].y,
            width,
        });
    }

    let controls_area = rows[row + 2];
    let (controls, buttons) =
        transport_row(engine.is_playing(), app.shuffle, app.repeat, controls_area.width);
    frame.render_widget(Paragraph::new(controls), controls_area);
    {
        let mut hit = app.hit.borrow_mut();
        for (button, start, width) in buttons {
            let x = controls_area.x.saturating_add(start);
            let right = controls_area.x.saturating_add(controls_area.width);
            if x >= right {
                continue;
            }
            let width = width.min(right - x);
            hit.buttons.push((Rect::new(x, controls_area.y, width, 1), button));
        }
    }

    let position = engine.position();
    let duration = engine.duration().or_else(|| current_track_duration(app));
    // Secciones precargadas de la sesión de URL actual; se consultan en cada
    // frame para que la barra se actualice mientras descarga.
    let buffered: Vec<(f32, f32)> = engine.buffered_fractions().unwrap_or_default();
    match duration.filter(|total| !total.is_zero()) {
        Some(total) => {
            let ratio = (position.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0);
            let prog_suffix = format!("{} / {}", format_duration(position), format_duration(total));
            frame.render_widget(
                Paragraph::new(build_bar(
                    "Progreso ",
                    ratio,
                    &prog_suffix,
                    theme().ok,
                    theme().dim,
                    inner.width as usize,
                    theme().buffered,
                    &buffered,
                )),
                rows[row + 1],
            );
            if let Some((offset, width)) =
                bar_geometry("Progreso ", &prog_suffix, inner.width as usize)
            {
                app.hit.borrow_mut().progress = Some(BarHit {
                    x: rows[row + 1].x + offset,
                    y: rows[row + 1].y,
                    width,
                });
            }
        }
        None => {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Progreso ", Style::default().fg(theme().dim)),
                    Span::styled(format_duration(position), Style::default().fg(theme().text)),
                ])),
                rows[row + 1],
            );
        }
    }
}

/// Estado de una celda de la barra de progreso.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BarCellKind {
    /// Parte ya reproducida (━).
    Played,
    /// Punta de la posición actual (╸).
    Tip,
    /// Sección descargada pero no reproducida (░ con el color `buffered`).
    Buffered,
    /// Track vacío (░ gris).
    Empty,
}

/// Mapea (ancho, fracción reproducida, secciones precargadas) a un estilo por
/// celda, de izquierda a derecha. Función pura, cubierta por tests.
///
/// Clampea `played` y las fracciones a `[0, 1]`; una celda se marca
/// `Buffered` si algún rango la toca. Las celdas ya reproducidas conservan
/// `Played`/`Tip`. `width == 0` devuelve vacío.
fn progress_bar_cells(width: usize, played: f64, buffered: &[(f32, f32)]) -> Vec<BarCellKind> {
    if width == 0 {
        return Vec::new();
    }
    let played = played.clamp(0.0, 1.0);
    let filled = (played * width as f64).round() as usize;
    let have_tip = filled > 0 && filled < width;

    let mut cells = Vec::with_capacity(width);
    for i in 0..width {
        if i < filled {
            cells.push(if have_tip && i == filled - 1 {
                BarCellKind::Tip
            } else {
                BarCellKind::Played
            });
            continue;
        }
        let covered = buffered.iter().any(|&(s, e)| {
            let s = (s as f64).clamp(0.0, 1.0);
            let e = (e as f64).clamp(0.0, 1.0);
            e > s && s < (i + 1) as f64 / width as f64 && e > i as f64 / width as f64
        });
        cells.push(if covered {
            BarCellKind::Buffered
        } else {
            BarCellKind::Empty
        });
    }
    cells
}

/// Construye una barra de progreso visual con ━, ╸ y ░.
///
/// Formato: `label [━━━━╸░░░░]  suffix`
/// `filled_ratio` en `[0.0, 1.0]`. `total_width` es el ancho disponible de
/// la fila completa (en celdas de terminal). Las secciones `buffered`
/// (fracciones `[0.0, 1.0]`) se pintan con `buffered_color` sobre el track
/// vacío, sin tocar la parte ya reproducida.
#[allow(clippy::too_many_arguments)]
fn build_bar(
    label: &str,
    filled_ratio: f64,
    suffix: &str,
    filled_color: Color,
    empty_color: Color,
    total_width: usize,
    buffered_color: Color,
    buffered: &[(f32, f32)],
) -> Line<'static> {
    // Ancho de la barra interna: misma geometría que usa el mouse para
    // ubicar el clic (etiqueta + " [" + barra + "] " + sufijo).
    let bar_chars = bar_geometry(label, suffix, total_width)
        .map_or(0, |(_, width)| usize::from(width));

    let label_span = Span::styled(label.to_string(), Style::default().fg(empty_color));

    if bar_chars == 0 {
        // Sin espacio para la barra, mostrar solo etiqueta + sufijo.
        return Line::from(vec![
            label_span,
            Span::styled(suffix.to_string(), Style::default().fg(theme().text)),
        ]);
    }

    // Función pura: decide el estilo de cada celda (reproducida / punta /
    // precargada / vacía) y agrupa contiguos en spans.
    let kinds = progress_bar_cells(bar_chars, filled_ratio, buffered);

    let mut spans = vec![
        label_span,
        Span::styled(" [", Style::default().fg(empty_color)),
    ];
    let mut i = 0;
    while i < kinds.len() {
        let kind = kinds[i];
        let mut j = i + 1;
        while j < kinds.len() && kinds[j] == kind {
            j += 1;
        }
        let (ch, color) = match kind {
            BarCellKind::Played => ("━", filled_color),
            BarCellKind::Tip => ("╸", filled_color),
            BarCellKind::Buffered => ("░", buffered_color),
            BarCellKind::Empty => ("░", empty_color),
        };
        let count = j - i;
        spans.push(Span::styled(ch.repeat(count), Style::default().fg(color)));
        i = j;
    }

    spans.push(Span::styled("] ", Style::default().fg(empty_color)));
    spans.push(Span::styled(suffix.to_string(), Style::default().fg(theme().text)));

    Line::from(spans)
}

/// Fila "Velocidad: 1.25x" del panel de info (solo se renderiza si != 1.0x).
fn speed_line(engine: &AudioEngine) -> Line<'static> {
    Line::from(vec![
        Span::styled("Velocidad: ", Style::default().fg(theme().dim)),
        Span::styled(speed_label(engine.speed()), Style::default().fg(theme().accent)),
    ])
}

fn speed_label(speed: f32) -> String {
    format!("{speed:.2}x")
}

/// Explorador a pantalla completa: solo cuando no hay sesión de audio;
/// con player el explorador va como panel lateral en `render_player`. Sin
/// sesión no hay nada que visualizar, así que la lista se queda con toda la
/// altura en vez de mostrar un bloque Visualizador vacío.
fn render_browser(frame: &mut Frame<'_>, app: &App) {
    debug_assert!(
        !app.has_player,
        "con sesión de audio el explorador es panel lateral, no pantalla completa"
    );
    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(6),
            Constraint::Length(5),
        ])
        .split(frame.area());

    render_browser_header(frame, zones[0], app);
    render_library(frame, zones[1], app, false);
    let compact = frame.area().height < 30;
    render_footer(frame, zones[2], app, compact);
}

fn render_browser_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = themed_block(accent_title("mp3tui"));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let now_playing = app
        .current_path
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned());

    let now_line = match &now_playing {
        Some(name) => Line::from(vec![
            Span::styled("Sonando: ", Style::default().fg(theme().dim)),
            Span::styled(PLAYING, Style::default().fg(theme().ok)),
            Span::raw(" "),
            Span::styled(name, Style::default().fg(theme().text)),
        ]),
        None => Line::from(vec![
            Span::styled("Sonando: ", Style::default().fg(theme().dim)),
            Span::styled("—", Style::default().fg(theme().dim)),
        ]),
    };

    // Con!has_player (único caso que llega acá) no hay sesión: la
    // línea de estado es siempre «Explorando» + repeat/shuffle si están.
    let mut spans = vec![
        Span::styled("Estado:  ", Style::default().fg(theme().dim)),
        Span::styled("Explorando", Style::default().fg(theme().dim)),
    ];
    spans.extend(repeat_shuffle_spans(app.repeat, app.shuffle));

    frame.render_widget(
        Paragraph::new(vec![now_line, Line::from(spans), error_line(app)]),
        inner,
    );
}

/// `focused` controla el color del borde (acento cuando tiene foco,
/// apagado cuando el panel del explorador tiene el foco).
fn render_library(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let folder = library_folder_label(app);
    // La solapa Cola solo existe en el panel lateral (con player); en el
    // explorador a pantalla completa siempre se ve la Biblioteca.
    let queue_view = app.explorer_open && app.queue_panel;
    // Buscando: borde y título en el color de aviso (warn) en vez del acento
    // normal, para que el modo búsqueda se note de un vistazo y no sólo por
    // el texto "Buscar:" (que puede pasar desapercibido).
    let searching = app.search.is_some() && !queue_view;
    let highlight = if searching {
        theme().warn
    } else if focused {
        theme().accent
    } else {
        theme().dim
    };
    let border_color = if searching {
        theme().warn
    } else if focused {
        theme().accent
    } else {
        theme().dim
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(border_color))
        .title(if queue_view {
            queue_title_line(app, highlight)
        } else {
            library_title_line(app, &folder, highlight)
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Franja «Próxima canción» (T1/S3): reserva las últimas 3 filas del panel
    // cuando hay próximo y deja el resto a la lista. Sin próximo no agrega
    // ruido, igual que la línea del reproductor.
    let marquee_budget = match &app.next_up {
        Some(title) if !title.is_empty() => 3u16,
        _ => 0,
    };
    let list_area = if marquee_budget > 0 {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(marquee_budget),
            ])
            .split(inner);
        let (list_area, marquee_area) = (split[0], split[1]);
        render_next_up_marquee(
            frame,
            marquee_area,
            app.next_up.as_deref().unwrap_or_default(),
            app.ui_frame,
        );
        list_area
    } else {
        inner
    };

    if queue_view {
        let lines = queue_rows(app, list_area.width, list_area.height);
        frame.render_widget(Paragraph::new(lines), list_area);
        return;
    }

    let Some(library) = &app.library else {
        // Primer escaneo en curso: estado de carga en vez de lista vacía.
        frame.render_widget(
            Paragraph::new(Line::styled(
                " Escaneando biblioteca… ",
                Style::default().fg(theme().accent),
            ))
            .alignment(Alignment::Center),
            list_area,
        );
        return;
    };

    if app.view.is_empty() {
        let (message, style) = if library.is_empty() {
            (
                " Sin archivos de audio (mp3, flac, ogg, wav) en esta carpeta ",
                Style::default().fg(theme().dim),
            )
        } else {
            (
                " Sin coincidencias para la búsqueda ",
                Style::default().fg(theme().accent),
            )
        };
        frame.render_widget(Paragraph::new(Line::styled(message, style)), list_area);
        return;
    }

    let lines = library_rows(app, list_area.width, list_area.height);
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// Franja del pie del panel lateral: `--- próxima canción ---`, el título en
/// marquee y el cierre (T1/S1). Ocupa `area` (3 filas) y recorta las líneas
/// fijas al ancho disponible.
fn render_next_up_marquee(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    frame_clock: u64,
) {
    let width = area.width as usize;
    let fixed = |text: &str| {
        let value: String = text.chars().take(width).collect();
        Line::from(Span::styled(value, Style::default().fg(theme().dim)))
    };
    let title_line = Line::from(Span::styled(
        marquee_window(title, width, frame_clock as usize),
        Style::default().fg(theme().text),
    ));
    frame.render_widget(
        Paragraph::new(vec![
            fixed("--- próxima canción ---"),
            title_line,
            fixed("------------------------"),
        ]),
        area,
    );
}

fn item_group<'a>(app: &'a App, item: &ViewItem) -> Option<&'a str> {
    match item {
        ViewItem::History(_) => Some("Agregadas por URL"),
        ViewItem::Track(idx) => library_get(app, *idx).map(|t| t.folder.as_str()),
    }
}

/// ¿La vista agrupa más de una carpeta o incluye historial?
/// Con una sola carpeta en pantalla (y sin historial) los encabezados solo ensucian,
/// así que no se dibujan. Si hay historial, siempre se dibuja "Agregadas por URL"
/// y el grupo correspondiente a las pistas locales.
fn view_is_grouped(app: &App) -> bool {
    let has_history = app.view.iter().any(|item| matches!(item, ViewItem::History(_)));
    if has_history {
        return true;
    }
    let mut first: Option<&str> = None;
    for position in 0..app.view.len() {
        let Some(track) = view_get(app, position) else {
            break;
        };
        match first {
            None => first = Some(track.folder.as_str()),
            Some(folder) if folder != track.folder.as_str() => return true,
            Some(_) => {}
        }
    }
    false
}

/// Filas que ocupan `[from..=to]`: una por item más un encabezado
/// cada vez que cambia de grupo; el grupo de `from` también cuenta si es
/// distinto del item anterior, que queda fuera de la vista.
fn rows_between(app: &App, from: usize, to: usize, grouped: bool) -> usize {
    if from > to {
        return 0;
    }
    let mut rows = 0;
    let mut previous: Option<&str> = None;
    if grouped && from > 0 {
        previous = app.view.get(from - 1).and_then(|item| item_group(app, item));
    }
    for position in from..=to {
        let Some(item) = app.view.get(position) else {
            break;
        };
        let group = item_group(app, item);
        if grouped && group.is_some() && previous != group {
            rows += 1;
        }
        rows += 1;
        previous = group;
    }
    rows
}

/// Primer track visible para que `selected` entre en `height` filas con los
/// encabezados incluidos: arranca del candidato centrado clásico
/// (`scroll_offset`) y sube mientras `[start..=selected]` no entre en el
/// alto. Nunca supera `selected`, así que el cursor siempre queda a la vista.
fn visible_start(app: &App, height: usize, grouped: bool) -> usize {
    let len = app.view.len();
    if len == 0 {
        return 0;
    }
    let mut start = scroll_offset(app.selected, len, height);
    while start < app.selected
        && rows_between(app, start, app.selected, grouped) > height
    {
        start += 1;
    }
    start
}

fn history_row_line(
    entry: &crate::history::HistoryEntry,
    selected: bool,
    playing: bool,
    width: u16,
) -> Line<'static> {
    let cursor = if selected { CURSOR } else { " " };
    let marker = if playing { PLAYING } else { " " };
    let badge = "[URL] ";

    let budget = (width as usize).saturating_sub(2 + badge.chars().count());

    let title_raw = if entry.title.is_empty() {
        &entry.url
    } else {
        &entry.title
    };

    let title_chars = title_raw.chars().count();
    let (title_display, padding) = if title_chars <= budget {
        (title_raw.to_string(), budget - title_chars)
    } else {
        let truncated = truncate_chars(title_raw, budget);
        let pad = budget.saturating_sub(truncated.chars().count());
        (truncated, pad)
    };

    let (cursor_style, marker_style, badge_style, title_style) = if selected {
        let bg = theme().select_bg;
        (
            Style::default().bg(bg).fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().ok),
            Style::default().bg(bg).fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().text).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().fg(theme().ok),
            Style::default().fg(theme().accent),
            Style::default().fg(theme().text),
        )
    };

    let spaces = " ".repeat(padding);
    let title_with_pad = format!("{title_display}{spaces}");

    let spans = vec![
        Span::styled(cursor, cursor_style),
        Span::styled(marker, marker_style),
        Span::styled(badge, badge_style),
        Span::styled(title_with_pad, title_style),
    ];

    Line::from(spans)
}

/// Filas visibles de la biblioteca: encabezados de grupo
/// intercalados ("Agregadas por URL" y carpetas). Los encabezados
/// no son seleccionables — `selected` sigue siendo un índice sobre `view`
/// y las flechas los saltan solas.
fn library_rows(app: &App, width: u16, height: u16) -> Vec<Line<'static>> {
    let height = height as usize;
    if height == 0 {
        return Vec::new();
    }
    let grouped = view_is_grouped(app);
    let start = visible_start(app, height, grouped);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
    let mut previous: Option<&str> = None;
    if grouped && start > 0 {
        previous = app.view.get(start - 1).and_then(|item| item_group(app, item));
    }
    for (position, item) in app.view.iter().enumerate().skip(start) {
        if lines.len() >= height {
            break;
        }
        let group = item_group(app, item);
        if grouped && previous != group {
            if let Some(grp) = group {
                // Dejan entrar encabezado + fila; si no, se corta la vista.
                if lines.len() + 2 > height {
                    break;
                }
                lines.push(folder_header_line(grp, width));
            }
            previous = group;
        }
        let is_selected = position == app.selected;
        match item {
            ViewItem::History(hist_idx) => {
                let Some(entry) = app.history.get(*hist_idx) else {
                    break;
                };
                let is_playing = app.is_url
                    && ((!entry.title.is_empty() && app.track_label == entry.title)
                        || app.track_label == entry.url);
                lines.push(history_row_line(
                    entry,
                    is_selected,
                    is_playing,
                    width,
                ));
            }
            ViewItem::Track(library_index) => {
                let Some(track) = library_get(app, *library_index) else {
                    break;
                };
                let is_playing = app.current_path.as_deref() == Some(track.path.as_path());
                lines.push(row_line(
                    track,
                    position + 1,
                    is_selected,
                    is_playing,
                    width,
                ));
            }
        }
    }
    lines
}

/// Ventana de marquee para un título de `width` celdas (T1/D2): si entra,
/// pegado a la izquierda (con padding a `width`); si no, recorre una ventana
/// de `width` celdas sobre `título + separador + título` en bucle continuo,
/// avanzando un carácter por frame.
fn marquee_window(text: &str, width: usize, frame: usize) -> String {
    let width = width.max(1);
    let chars = text.chars().count();
    if chars <= width {
        return format!("{text:<width$}");
    }
    let sep = "   ";
    let cycle: Vec<char> = format!("{text}{sep}{text}{sep}").chars().collect();
    let step = frame % (chars + sep.chars().count());
    cycle.iter().skip(step).take(width).collect()
}

/// Estado de una fila de la cola respecto del cursor de reproducción (S9): lo
/// ya sonado se atenúa, la posición donde está parado se marca y lo que falta
/// queda normal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum QueueRowState {
    Played,
    Current,
    Pending,
}

/// Estado de `position` en la cola: el ítem del cursor es el que suena (o el
/// último que sonó desde la cola), los de abajo ya sonaron y los de arriba
/// faltan.
fn queue_row_state(app: &App, position: usize) -> QueueRowState {
    match app.queue_cursor {
        Some(cursor) if position < cursor => QueueRowState::Played,
        Some(cursor) if position == cursor => QueueRowState::Current,
        _ => QueueRowState::Pending,
    }
}

/// Lista de la Cola del panel: el mismo resaltado de selección que la
/// Biblioteca (`queue_selected`), más el marcador `▶` del ítem parado en el
/// cursor de reproducción y los ya sonados atenuados. La cola vacía muestra un
/// estado explícito.
fn queue_rows(app: &App, width: u16, height: u16) -> Vec<Line<'static>> {
    let height = height as usize;
    if height == 0 {
        return Vec::new();
    }
    if app.queue.is_empty() {
        return vec![Line::styled(
            " La cola está vacía: apretá a en la Biblioteca para encolar ",
            Style::default().fg(theme().dim),
        )];
    }
    let start = scroll_offset(app.queue_selected, app.queue.len(), height);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
    for (position, item) in app.queue.iter().enumerate().skip(start) {
        if lines.len() >= height {
            break;
        }
        let selected = position == app.queue_selected;
        lines.push(queue_row_line(
            item.label(),
            position + 1,
            selected,
            queue_row_state(app, position),
            width,
        ));
    }
    lines
}

/// Una fila de la cola: cursor de selección, marcador de reproducción, número
/// y etiqueta del ítem, con la misma paleta que la fila de la biblioteca
/// (`select_bg` para la seleccionada).
fn queue_row_line(
    label: &str,
    number: usize,
    selected: bool,
    state: QueueRowState,
    width: u16,
) -> Line<'static> {
    let cursor = if selected { CURSOR } else { " " };
    let marker = if state == QueueRowState::Current {
        PLAYING
    } else {
        " "
    };
    let prefix = format!("{number:>2}. ");
    let budget = (width as usize).saturating_sub(2 + prefix.chars().count());
    let label = truncate_chars(label, budget);
    let pad = budget.saturating_sub(label.chars().count());
    let body = format!("{prefix}{label}{}", " ".repeat(pad));
    let (cursor_style, marker_style, text_style) = if selected {
        let bg = theme().select_bg;
        (
            Style::default().bg(bg).fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().ok),
            Style::default().bg(bg).fg(theme().text),
        )
    } else {
        let text = match state {
            QueueRowState::Played => theme().dim,
            QueueRowState::Current | QueueRowState::Pending => theme().text,
        };
        (
            Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().fg(theme().ok),
            Style::default().fg(text),
        )
    };
    Line::from(vec![
        Span::styled(cursor, cursor_style),
        Span::styled(marker, marker_style),
        Span::styled(body, text_style),
    ])
}

/// Título del bloque Cola como línea con estilo (mismo patrón que el de la
/// biblioteca).
fn queue_title_line(app: &App, highlight: Color) -> Line<'static> {
    Line::from(Span::styled(
        queue_title(app),
        Style::default().fg(highlight).add_modifier(Modifier::BOLD),
    ))
}

/// Título del bloque Cola: cantidad de ítems que tiene la cola.
fn queue_title(app: &App) -> String {
    let count = app.queue.len();
    let noun = if count == 1 { "ítem" } else { "ítems" };
    format!(" Cola · {count} {noun} ")
}

/// Encabezado de carpeta: nombre en acento y negrita seguido de un
/// guion fino que completa el ancho de la lista.
fn folder_header_line(folder: &str, width: u16) -> Line<'static> {
    let width = width as usize;
    let label = truncate_chars(&format!(" {folder} "), width);
    let used = label.chars().count();
    let mut spans = vec![Span::styled(
        label,
        Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
    )];
    if width > used {
        spans.push(Span::styled(
            "\u{2500}".repeat(width - used),
            Style::default().fg(theme().dim),
        ));
    }
    Line::from(spans)
}

/// Título del bloque biblioteca como línea con estilos: buscando, «Buscar:»
/// más una caja de ancho fijo con el fondo `select_bg` del tema, para que el
/// campo se distinga del borde; si no, el título de siempre.
fn library_title_line(app: &App, folder: &str, highlight: Color) -> Line<'static> {
    let title_style = Style::default().fg(highlight).add_modifier(Modifier::BOLD);
    let Some(query) = &app.search else {
        return Line::from(Span::styled(library_title(app, folder), title_style));
    };

    let mut spans = vec![
        Span::styled(" Buscar: ", title_style),
        search_box_span(query),
        Span::styled(format!(" · {} coincidencias ", app.view.len()), title_style),
    ];
    let suffix = mode_suffix(app);
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix, title_style));
    }
    Line::from(spans)
}

/// Caja del campo de búsqueda: siempre `SEARCH_BOX_WIDTH` columnas sobre el
/// fondo `select_bg`. Con texto largo muestra el final (donde está el cursor);
/// vacía muestra un placeholder tenue.
fn search_box_span(query: &str) -> Span<'static> {
    let (content, fg) = if query.is_empty() {
        (format!("{SEARCH_CARET}{SEARCH_PLACEHOLDER}"), theme().dim)
    } else {
        let typed = format!("{query}{SEARCH_CARET}");
        let len = typed.chars().count();
        let visible = if len > SEARCH_BOX_WIDTH {
            typed.chars().skip(len - SEARCH_BOX_WIDTH).collect()
        } else {
            typed
        };
        (visible, theme().text)
    };
    let padding = SEARCH_BOX_WIDTH.saturating_sub(content.chars().count());
    Span::styled(
        format!("{content}{}", " ".repeat(padding)),
        Style::default().fg(fg).bg(theme().select_bg),
    )
}

/// Indicadores de modo (repeat/shuffle) que se agregan al título solo cuando
/// están prendidos.
fn mode_suffix(app: &App) -> String {
    let mut suffix = String::new();
    if let Some(label) = app.repeat.label() {
        suffix.push_str(&format!("· repeat: {label} "));
    }
    if app.shuffle {
        suffix.push_str("· shuffle ");
    }
    suffix
}

/// Título del bloque biblioteca: lista visible, filtro, búsqueda activa y
/// modos de reproducción (solo cuando están prendidos, para no ensuciar).
fn library_title(app: &App, folder: &str) -> String {
    let mut title = if let Some(query) = &app.search {
        format!(
            " Buscar: {query}{SEARCH_CARET} · {} coincidencias ",
            app.view.len()
        )
    } else {
        match &app.library {
            None => format!(" Biblioteca · {folder} "),
            Some(library) => {
                let count = if app.filter.is_some() {
                    format!("{}/{}", app.view.len(), library.len())
                } else {
                    format!("{}", library.len())
                };
                let mut base = if app.scan_rx.is_some() {
                    format!(" Biblioteca · {count} pistas · {folder} · actualizando… ")
                } else {
                    format!(" Biblioteca · {count} pistas · {folder} ")
                };
                if let Some(filter) = &app.filter {
                    base.push_str(&format!("· filtro \"{filter}\" "));
                }
                base
            }
        }
    };
    title.push_str(&mode_suffix(app));
    title
}

fn folder_label(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

/// Carpeta que nombra a la biblioteca en el título: con una raíz, su
/// nombre; con varias, cuántas hay.
fn library_folder_label(app: &App) -> String {
    match app.roots.as_slice() {
        [root] => folder_label(root),
        roots => format!("{} carpetas", roots.len()),
    }
}

/// Una fila de la biblioteca con título en `theme().text` y
/// artista/álbum en `theme().dim`. Ocupa exactamente `width` celdas.
fn row_line(
    track: &Track,
    number: usize,
    selected: bool,
    playing: bool,
    width: u16,
) -> Line<'static> {
    let cursor = if selected { CURSOR } else { " " };
    let marker = if playing { PLAYING } else { " " };
    let mut duration = track
        .duration
        .map(format_duration)
        .map(|value| format!(" {value:>5}"))
        .unwrap_or_default();

    // Tope: en un terminal muy angosto una duración larga (1:02:05) no debe
    // desbordar la fila; el título tiene prioridad sobre la duración.
    if duration.chars().count() + 2 > width as usize {
        duration = String::new();
    }

    // Presupuesto del bloque central (número + título + meta); el cursor, el
    // indicador y la duración ya ocupan lo suyo.
    let budget = (width as usize).saturating_sub(2 + duration.chars().count());

    // Parte principal: número + título.
    let main_part = format!("{number:>2}. {}", track.title);
    // Parte dim: separador + artista/álbum (solo si existe).
    let dim_part_raw: String = track
        .secondary()
        .map(|s| format!("  \u{b7} {s}"))
        .unwrap_or_default();

    let main_chars = main_part.chars().count();
    let dim_chars = dim_part_raw.chars().count();
    let total_chars = main_chars + dim_chars;

    // Calcular cuántos chars van a cada parte dentro del budget.
    let (main_display, dim_display, padding) = if total_chars <= budget {
        // Caben ambas; el relleno va al final del bloque dim (o al final del main si no hay dim).
        let pad = budget - total_chars;
        (main_part.clone(), dim_part_raw.clone(), pad)
    } else if main_chars >= budget {
        // El título solo ya supera el budget: truncar el título, sin dim.
        let truncated = truncate_chars(&main_part, budget);
        let pad = budget.saturating_sub(truncated.chars().count());
        (truncated, String::new(), pad)
    } else {
        // El título cabe; el dim se trunca a lo que queda.
        let dim_budget = budget - main_chars;
        let truncated_dim = if dim_chars > dim_budget {
            truncate_chars(&dim_part_raw, dim_budget)
        } else {
            dim_part_raw.clone()
        };
        let used = main_chars + truncated_dim.chars().count();
        let pad = budget.saturating_sub(used);
        (main_part.clone(), truncated_dim, pad)
    };

    let (cursor_style, marker_style, title_style, dim_style, duration_style) = if selected {
        let bg = theme().select_bg;
        (
            Style::default().bg(bg).fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().ok),
            Style::default().bg(bg).fg(theme().text).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().dim),
            Style::default().bg(bg).fg(theme().dim),
        )
    } else {
        (
            Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().fg(theme().ok),
            Style::default().fg(theme().text),
            Style::default().fg(theme().dim),
            Style::default().fg(theme().dim),
        )
    };

    // El relleno de espacios se adjunta al último span del bloque central.
    let spaces = " ".repeat(padding);
    let dim_with_pad = format!("{dim_display}{spaces}");

    let mut spans = vec![
        Span::styled(cursor, cursor_style),
        Span::styled(marker, marker_style),
        Span::styled(main_display, title_style),
    ];
    // Si dim_with_pad está vacío (ni dim ni padding) no agregar el span.
    if !dim_with_pad.is_empty() {
        spans.push(Span::styled(dim_with_pad, dim_style));
    } else {
        // Sin parte dim: el padding va adjunto al span del título si el
        // presupuesto no fue exacto (esto puede ocurrir cuando dim_display
        // está vacío y padding > 0 pero la rama anterior lo puso en dim_with_pad).
        // La rama ya cubre esto: dim_with_pad = spaces en ese caso.
    }
    if !duration.is_empty() {
        spans.push(Span::styled(duration, duration_style));
    }

    Line::from(spans)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('\u{2026}'); // …
    out
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// `focused` controla el color del borde del visualizador.
fn render_visualizer(frame: &mut Frame<'_>, area: Rect, analyzer: &SpectrumAnalyzer, focused: bool) {
    let border_color = if focused { theme().accent } else { theme().dim };
    let block = Block::bordered()
        .border_style(Style::default().fg(border_color))
        .title(accent_title("Visualizador"));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let ascii = matches!(
        runtime::effective_visualizer_style(),
        config::VisualizerStyle::Ascii
    );
    // Cada modo va a su propio pintor con los datos que necesita: `onda` al
    // osciloscopio con la ventana triggered y con ganancia automática del
    // analizador (`scope_window`), `espejo` y `vu` con las bandas/RMS del
    // analizador, `circular` a su plot polar, `miku` a la bailarina ASCII;
    // `barras` sigue el path actual byte-a-byte idéntico vía `paint_bands`.
    match resolve_visualizer_paint(runtime::active_visualizer_mode()) {
        VisualizerPaint::Bars => {
            paint_bands(frame.buffer_mut(), inner, analyzer.bands(), &theme(), ascii)
        }
        VisualizerPaint::Wave => {
            let samples = analyzer.scope_window();
            paint_wave(frame.buffer_mut(), inner, &samples, &theme(), ascii);
        }
        VisualizerPaint::Mirror => {
            paint_mirror(frame.buffer_mut(), inner, analyzer.bands(), &theme(), ascii)
        }
        VisualizerPaint::Vu => paint_vu(
            frame.buffer_mut(),
            inner,
            analyzer.vu(),
            analyzer.vu_peak(),
            &theme(),
            ascii,
        ),
        VisualizerPaint::Circular => paint_circular(
            frame.buffer_mut(),
            inner,
            analyzer.bands(),
            &theme(),
            ascii,
        ),
        VisualizerPaint::Miku => paint_miku(frame.buffer_mut(), inner, analyzer),
    }
}


/// Color de paleta del arte animado (`miku_art`): letras a..g generadas del
/// `.asciimtn` original. `a` = negro: la silueta de Miku se dibuja en oscuro
/// sobre el fondo de `@` blancos (igual que en el archivo); `c`/`d` son los
/// acentos turquesa/cian.
fn miku_palette_color(letter: char) -> Option<Color> {
    match letter {
        'a' => Some(Color::Rgb(0x00, 0x00, 0x00)),
        'b' => Some(Color::Rgb(0x00, 0xcd, 0x00)),
        'c' => Some(Color::Rgb(0x00, 0xcd, 0xcd)),
        'd' => Some(Color::Rgb(0x00, 0xff, 0xff)),
        'e' => Some(Color::Rgb(0x7f, 0x7f, 0x7f)),
        'f' => Some(Color::Rgb(0xe5, 0xe5, 0xe5)),
        'g' => Some(Color::Rgb(0xff, 0xff, 0xff)),
        _ => None,
    }
}

/// Pintor de la bailarina (modo `miku`): reproduce la ANIMACIÓN real de
/// «hatsumemiku.asciimtn» (26 frames de la danza, con colores de paleta,
/// recortada al bbox de la figura), avanzando un frame cada `ANIM_TICKS` de
/// reloj para que el bucle dure ~3 s. El fondo del área es un campo de `@`
/// blancos, igual que el lienzo del archivo, y la figura (silueta oscura +
/// acentos) se dibuja encima **centrada en el eje X**: el arco de salto por el
/// beat mueve Miku verticalmente alrededor del centro, sin vaivén lateral
/// fuera de eje. Los dibujos son ASCII puro, idénticos con estilo ascii o
/// unicode.
fn paint_miku(
    buf: &mut ratatui::buffer::Buffer,
    inner: Rect,
    analyzer: &SpectrumAnalyzer,
) {
    // Fondo del lienzo del arte: campo de `@` blancos en todo el área.
    let white = Style::default().fg(Color::Rgb(0xff, 0xff, 0xff));
    for y in inner.y..inner.y + inner.height {
        buf.set_string(inner.x, y, "@".repeat(inner.width as usize), white);
    }

    use crate::visualizer::miku_art as art;
    let frame = (analyzer.dance_clock() / art::ANIM_TICKS as u64) as usize
        % art::SAMPLED_FRAMES;
    let lines = art::FRAME_CHARS[frame].lines().collect::<Vec<_>>();
    let color_lines = art::FRAME_COLORS[frame].lines().collect::<Vec<_>>();
    let art_w = art::CROP_RIGHT - art::CROP_LEFT + 1;
    let art_h = art::CROP_BOTTOM - art::CROP_TOP + 1;
    let (rows_avail, cols_avail) = (inner.height as usize, inner.width as usize);
    let rows = art_h.min(rows_avail);
    let cols = art_w.min(cols_avail);
    if rows == 0 || cols == 0 {
        return;
    }

    // Animación: arco de salto (0 reposo / 1..=3 arriba-bajando) por el beat.
    // El movimiento es solo vertical alrededor del centro: la figura queda
    // siempre centrada en el eje X del visualizador.
    let dy: i16 = match analyzer.dance_pose() {
        1 => -2,
        2 => -3,
        3 => -1,
        _ => 0,
    };
    let dx: i16 = 0;

    // Ventana dentro del arte (cuando el área es más chica que la figura) y
    // desplazamiento de la figura dentro del área (centrado + animación). Uno
    // recorta la fuente, el otro posiciona el resultado.
    let win_r = if art_h > rows {
        (((art_h - rows) / 2) as i16 + dy).clamp(0, (art_h - rows) as i16) as usize
    } else {
        0
    };
    let win_c = if art_w > cols {
        (((art_w - cols) / 2) as i16 + dx).clamp(0, (art_w - cols) as i16) as usize
    } else {
        0
    };
    let pos_r = if rows_avail > rows {
        (((rows_avail - rows) / 2) as i16 + dy).clamp(0, (rows_avail - rows) as i16) as usize
    } else {
        0
    };
    let pos_c = if cols_avail > cols {
        (((cols_avail - cols) / 2) as i16 + dx).clamp(0, (cols_avail - cols) as i16) as usize
    } else {
        0
    };

    for r in 0..rows {
        let y = inner.y + (pos_r + r) as u16;
        let art_row = art::CROP_TOP + win_r + r;
        let line = lines[art_row].as_bytes();
        let color_line = color_lines[art_row].as_bytes();
        for c in 0..cols {
            let art_col = art::CROP_LEFT + win_c + c;
            let ch = line[art_col] as char;
            if ch == ' ' {
                continue;
            }
            let Some(color) = miku_palette_color(color_line[art_col] as char) else {
                continue;
            };
            buf.set_string(
                inner.x + (pos_c + c) as u16,
                y,
                ch.to_string(),
                Style::default().fg(color),
            );
        }
    }
}

/// Destino del dispatch de render por modo: barras, osciloscopio, espejo, VU,
/// plot polar o bailarina Miku. Sin fallback: los 6 modos tienen pintor propio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisualizerPaint {
    Bars,
    Wave,
    Mirror,
    Vu,
    Circular,
    Miku,
}

/// Dispatch puro del modo al pintor: cada modo va al suyo, sin fallback.
fn resolve_visualizer_paint(mode: config::VisualizerMode) -> VisualizerPaint {
    match mode {
        config::VisualizerMode::Barras => VisualizerPaint::Bars,
        config::VisualizerMode::Onda => VisualizerPaint::Wave,
        config::VisualizerMode::Espejo => VisualizerPaint::Mirror,
        config::VisualizerMode::Vu => VisualizerPaint::Vu,
        config::VisualizerMode::Circular => VisualizerPaint::Circular,
        config::VisualizerMode::Miku => VisualizerPaint::Miku,
    }
}

/// Plot polar grueso de las 32 bandas: anillo pulsante rasterizado celda a
/// celda (no 32 radios finos), así el contorno es continuo.
///
/// Eje simétrico izquierda/derecha, con los graves abajo y los agudos arriba;
/// el nivel de cada banda se interpola entre vecinas. El hueco interior queda
/// como anillo base tenue, visible aun en silencio.
fn paint_circular(
    buffer: &mut ratatui::buffer::Buffer,
    inner: Rect,
    bands: &[f32; NUM_BANDS],
    theme: &Theme,
    ascii: bool,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    // Fondo (igual que las barras: puntos tenues en ascii, vacío en unicode).
    for y in inner.y..inner.y + inner.height {
        for x in inner.x..inner.x + inner.width {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                if ascii {
                    cell.set_symbol(".").set_fg(theme.dim);
                } else {
                    cell.set_symbol(" ").set_fg(Color::Reset);
                }
            }
        }
    }
    let width = f32::from(inner.width);
    let height = f32::from(inner.height);
    let max_r = (width / 2.0).min(height) - 0.5;
    if max_r < 2.0 {
        return;
    }
    let base_r = 0.45 * max_r;
    let span = max_r - base_r;
    let cx = f32::from(inner.x) + width / 2.0;
    let cy = f32::from(inner.y) + height / 2.0;
    let symbol = band_full_glyph(ascii);

    for y in inner.y..inner.y + inner.height {
        for x in inner.x..inner.x + inner.width {
            let dx = f32::from(x) + 0.5 - cx;
            let dy = f32::from(y) + 0.5 - cy;
            let r = dx.hypot(dy * 2.0);
            if r < base_r - 0.5 {
                continue; // Hueco: fondo ya pintado arriba.
            }
            let theta = dx.atan2(-(dy * 2.0));
            let a = (theta.abs() / std::f32::consts::PI).clamp(0.0, 1.0);
            let pos = (1.0 - a) * (NUM_BANDS as f32 - 1.0);
            let lo = pos.floor();
            let frac = pos - lo;
            // Clamp por banda ANTES de interpolar: así un nivel fuera de
            // rango (p. ej. 1.5) pinta igual que 1.0 exacto sin importar con
            // qué banda vecina interpole.
            let lo_level = bands[lo as usize].clamp(0.0, 1.0);
            let hi_level = bands[pos.ceil() as usize].clamp(0.0, 1.0);
            let level = lo_level * (1.0 - frac) + hi_level * frac;
            let fill_r = base_r + level * span;
            let Some(cell) = buffer.cell_mut((x, y)) else {
                continue;
            };
            if level > 0.0 && r <= fill_r {
                let t = ((r - base_r) / span).clamp(0.0, 1.0);
                let (red, green, blue) = spectrum_color(t, theme);
                cell.set_symbol(symbol).set_fg(Color::Rgb(red, green, blue));
            } else if (r - base_r).abs() < 0.5 {
                cell.set_symbol(symbol).set_fg(theme.dim);
            }
        }
    }
}

/// Espejo vertical de las 32 bandas desde una línea
/// central tenue (`-` en ascii, `─` en unicode, color `dim`, como el eje de
/// la onda). Cada banda crece arriba Y abajo con los mismos niveles y glifos
/// de `paint_bands` (helpers `band_slot`/`band_full_glyph`/`band_frac_glyph`,
/// sin lógica duplicada); el gradiente va de `spectrum_low` junto al eje a
/// `spectrum_high` en las puntas, simétrico en ambos lados. Fondo de puntos
/// `.` en ascii, vacío en unicode (igual que las barras). Alturas de 1–2
/// filas degradan graceful (solo el eje / un lado de 1 fila) sin pánico.
fn paint_mirror(
    buffer: &mut ratatui::buffer::Buffer,
    inner: Rect,
    bands: &[f32; NUM_BANDS],
    theme: &Theme,
    ascii: bool,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let total = u32::from(inner.width);
    // Fondo (igual que las barras: puntos tenues en ascii, vacío en unicode).
    for y in inner.y..inner.y + inner.height {
        for x in inner.x..inner.x + inner.width {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                if ascii {
                    cell.set_symbol(".").set_fg(theme.dim);
                } else {
                    cell.set_symbol(" ").set_fg(Color::Reset);
                }
            }
        }
    }
    // Eje central: misma fila que el cero de la onda.
    let axis_rel = wave_sample_row(0.0, inner.height);
    let axis_symbol = if ascii { "-" } else { "─" };
    for dx in 0..inner.width {
        if let Some(cell) = buffer.cell_mut((inner.x + dx, inner.y + axis_rel)) {
            cell.set_symbol(axis_symbol).set_fg(theme.dim);
        }
    }
    let top_h = axis_rel;
    let bottom_h = inner.height - axis_rel - 1;
    for (band, &magnitude) in bands.iter().enumerate() {
        let Some((slot_start, width)) = band_slot(band, total) else {
            break;
        };
        let level = magnitude.clamp(0.0, 1.0);
        for (above, side_h) in [(true, top_h), (false, bottom_h)] {
            if side_h == 0 {
                continue;
            }
            // Nivel continuo → celdas completas + fracción (igual que
            // `paint_bandas`, pero medido desde el eje hacia afuera).
            let height_f = level * f32::from(side_h);
            let full_cells = (height_f.floor() as u16).min(side_h);
            let frac = height_f - height_f.floor();
            // Fila junto al eje: hacia arriba resta, hacia abajo suma (en
            // rango por construcción: arriba `i <= axis_rel - 1`, abajo
            // `edge + i <= inner.y + alto - 1`).
            let edge = if above {
                inner.y + axis_rel - 1
            } else {
                inner.y + axis_rel + 1
            };
            for i in 0..full_cells {
                let y = if above { edge - i } else { edge + i };
                let t = if side_h > 1 {
                    f32::from(i) / f32::from(side_h - 1)
                } else {
                    1.0
                };
                let (red, green, blue) = spectrum_color(t, theme);
                let color = Color::Rgb(red, green, blue);
                let symbol = band_full_glyph(ascii);
                for dx in 0..width {
                    if let Some(cell) = buffer.cell_mut((inner.x + slot_start + dx, y)) {
                        cell.set_symbol(symbol).set_fg(color);
                    }
                }
            }
            if frac > 0.0 && full_cells < side_h {
                let y = if above {
                    edge - full_cells
                } else {
                    edge + full_cells
                };
                let symbol = band_frac_glyph(frac, ascii);
                let t = if side_h > 1 {
                    f32::from(full_cells) / f32::from(side_h - 1)
                } else {
                    1.0
                };
                let (red, green, blue) = spectrum_color(t, theme);
                let color = Color::Rgb(red, green, blue);
                for dx in 0..width {
                    if let Some(cell) = buffer.cell_mut((inner.x + slot_start + dx, y)) {
                        cell.set_symbol(symbol).set_fg(color);
                    }
                }
            }
        }
    }
}

/// Medidor VU mono horizontal sobre el nivel dBFS del tap
/// (`vu`/`db_meter_level`), con los mismos glifos y gradiente que las barras.
///
/// El pico se marca con un tick y baja con `PEAK_FALL`. En paneles de 1–2
/// filas se dibuja solo la barra, sin pico ni escala.
fn paint_vu(
    buffer: &mut ratatui::buffer::Buffer,
    inner: Rect,
    level: f32,
    peak: f32,
    theme: &Theme,
    ascii: bool,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let width = inner.width;
    // Fondo (igual que las barras: puntos tenues en ascii, vacío en unicode).
    for y in inner.y..inner.y + inner.height {
        for x in inner.x..inner.x + width {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                if ascii {
                    cell.set_symbol(".").set_fg(theme.dim);
                } else {
                    cell.set_symbol(" ").set_fg(Color::Reset);
                }
            }
        }
    }
    let level = level.clamp(0.0, 1.0);
    let peak = peak.clamp(0.0, 1.0);
    let bar_rows: u16 = if inner.height <= 2 { 1 } else { inner.height - 1 };
    let denom = f32::from(width.saturating_sub(1).max(1));
    // Barra: celdas llenas + parcial (mismos niveles y glifos que paint_bands).
    let fill_f = level * f32::from(width);
    let full = (fill_f.floor() as u16).min(width);
    let frac = fill_f - fill_f.floor();
    for row in 0..bar_rows {
        let y = inner.y + row;
        for dx in 0..full {
            let t = f32::from(dx) / denom;
            let (red, green, blue) = spectrum_color(t, theme);
            if let Some(cell) = buffer.cell_mut((inner.x + dx, y)) {
                cell.set_symbol(band_full_glyph(ascii))
                    .set_fg(Color::Rgb(red, green, blue));
            }
        }
        if frac > 0.0 && full < width {
            let t = f32::from(full) / denom;
            let (red, green, blue) = spectrum_color(t, theme);
            if let Some(cell) = buffer.cell_mut((inner.x + full, y)) {
                cell.set_symbol(band_frac_glyph(frac, ascii))
                    .set_fg(Color::Rgb(red, green, blue));
            }
        }
    }
    // Marca de pico: solo con altura para escala (en 1–2 filas no hay pico).
    if inner.height >= 3 && peak > 0.0 {
        let peak_f = peak * f32::from(width);
        let peak_full = peak_f.floor() as u16;
        // Columna del borde (borde exacto = primera celda vacía, con clip
        // al ancho; si cae a mitad de columna, la celda que lo contiene).
        let col = peak_full.min(width.saturating_sub(1));
        // Medidor horizontal: no hay glifo vertical de 7/8 que tenga sentido
        // para "a mitad de columna" (eso era del painter vertical original).
        // La misma celda llena, caiga el pico en el borde exacto o no.
        let symbol = band_full_glyph(ascii);
        for row in 0..bar_rows {
            if let Some(cell) = buffer.cell_mut((inner.x + col, inner.y + row)) {
                cell.set_symbol(symbol).set_fg(theme.spectrum_high);
            }
        }
    }
    // Escala tenue en la última fila (solo con altura para barra + escala).
    if inner.height >= 3 {
        let y = inner.y + inner.height - 1;
        let quarters = [
            0u16,
            width / 4,
            width / 2,
            width * 3 / 4,
            width.saturating_sub(1),
        ];
        let (tick, line) = if ascii { ("|", "-") } else { ("┼", "─") };
        for dx in 0..width {
            let symbol = if quarters.contains(&dx) { tick } else { line };
            if let Some(cell) = buffer.cell_mut((inner.x + dx, y)) {
                cell.set_symbol(symbol).set_fg(theme.dim);
            }
        }
    }
}

/// Slot horizontal proporcional de la banda `band` en un ancho total de
/// `total` columnas: `(x0_rel, ancho)` relativos al inicio del área, o `None`
/// si la banda cae fuera (menos columnas que bandas). Slot >= 2 → barra =
/// slot - 1 (1 col de gap); slot == 1 → sin gap posible. Extraído de
/// `paint_bands` para reusarlo en `paint_mirror` sin duplicar el reparto.
fn band_slot(band: usize, total: u32) -> Option<(u16, u16)> {
    let bands_count = NUM_BANDS as u32;
    let (slot_start, slot_end) = if total >= bands_count {
        (
            (total * band as u32 / bands_count) as u16,
            (total * (band as u32 + 1) / bands_count) as u16,
        )
    } else {
        (band as u16, band as u16 + 1)
    };
    if slot_start >= total as u16 {
        return None;
    }
    let bar_width = (slot_end - slot_start).saturating_sub(1).max(1);
    Some((slot_start, bar_width.min(total as u16 - slot_start)))
}

/// Glifo de celda llena de barra: `@` (nivel lleno de la rampa) en ascii,
/// `█` en unicode. Extraído de `paint_bands` para reusarlo en espejo.
fn band_full_glyph(ascii: bool) -> &'static str {
    if ascii { "@" } else { "█" }
}

/// Glifo de la celda parcial de una barra con fracción `frac` en 0.0..1.0:
/// rampa ASCII de 10 niveles o bloques de octavo en unicode (mismo gradiente
/// que la celda llena). Extraído de `paint_bands` para reusarlo en espejo.
fn band_frac_glyph(frac: f32, ascii: bool) -> &'static str {
    if ascii {
        let idx = ((frac * 10.0) as usize).min(9);
        ASCII_RAMP[idx]
    } else {
        let idx = ((frac * 8.0) as usize).min(7);
        EIGHTH_BLOCKS[idx]
    }
}

/// Osciloscopio amplitud-vs-tiempo con la ventana triggered y la ganancia
/// automática del analizador (`scope_window`), de más vieja a más nueva.
///
/// Cada columna se pinta con la envolvente mín/máx de sus muestras y se
/// enlaza con la anterior para que el trazo sea continuo. En unicode usa
/// Braille (2×4 por celda) con el gradiente del tema por altura; en ascii,
/// `#` sobre el fondo de puntos tenue.
fn paint_wave(
    buffer: &mut ratatui::buffer::Buffer,
    inner: Rect,
    samples: &[f32],
    theme: &Theme,
    ascii: bool,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let height = inner.height;
    let width = inner.width;
    // Fondo (igual que las barras: puntos tenues en ascii, vacío en unicode).
    for y in inner.y..inner.y + height {
        for x in inner.x..inner.x + width {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                if ascii {
                    cell.set_symbol(".").set_fg(theme.dim);
                } else {
                    cell.set_symbol(" ").set_fg(Color::Reset);
                }
            }
        }
    }
    if ascii {
        paint_wave_ascii(buffer, inner, samples, theme);
    } else {
        paint_wave_braille(buffer, inner, samples, theme);
    }
}

/// Rama ascii de `paint_wave`: eje tenue en toda la fila central, onda
/// `#` encima tapándolo donde corresponda (igual que antes: primero el
/// eje, después la onda).
fn paint_wave_ascii(
    buffer: &mut ratatui::buffer::Buffer,
    inner: Rect,
    samples: &[f32],
    theme: &Theme,
) {
    let height = inner.height;
    let width = inner.width;
    let axis_row = wave_sample_row(0.0, height);
    for dx in 0..width {
        if let Some(cell) = buffer.cell_mut((inner.x + dx, inner.y + axis_row)) {
            cell.set_symbol("-").set_fg(theme.dim);
        }
    }
    let spans = wave_column_spans(samples, usize::from(width), height);
    for dx in 0..width {
        let Some((lo, hi)) = spans[dx as usize] else {
            continue;
        };
        for row in lo..=hi {
            let t = if height > 1 {
                f32::from(height - 1 - row) / f32::from(height - 1)
            } else {
                0.5
            };
            let (red, green, blue) = spectrum_color(t, theme);
            if let Some(cell) = buffer.cell_mut((inner.x + dx, inner.y + row)) {
                cell.set_symbol(ASCII_RAMP[8]).set_fg(Color::Rgb(red, green, blue));
            }
        }
    }
}

/// Rama unicode de `paint_wave`: mismo eje tenue en toda la fila central
/// (`─`) que la rama ascii, con la onda encima en Braille (2×4 puntos por
/// celda, armados a partir de la envolvente conectada en resolución
/// `alto*4` × `ancho*2`) tapando el eje donde corresponda. El color de cada
/// celda con onda es el gradiente por altura de `spectrum_color`.
fn paint_wave_braille(
    buffer: &mut ratatui::buffer::Buffer,
    inner: Rect,
    samples: &[f32],
    theme: &Theme,
) {
    let height = inner.height;
    let width = inner.width;
    let axis_row = wave_sample_row(0.0, height);
    for dx in 0..width {
        if let Some(cell) = buffer.cell_mut((inner.x + dx, inner.y + axis_row)) {
            cell.set_symbol("─").set_fg(theme.dim);
        }
    }
    let rows_v = height.saturating_mul(4);
    let cols_v = usize::from(width) * 2;
    let spans = wave_column_spans(samples, cols_v, rows_v);
    for y in 0..height {
        let t = if height > 1 {
            f32::from(height - 1 - y) / f32::from(height - 1)
        } else {
            0.5
        };
        let (red, green, blue) = spectrum_color(t, theme);
        let wave_color = Color::Rgb(red, green, blue);
        for x in 0..width {
            let mut bits = 0u8;
            for sub_row in 0..4u16 {
                let row_v = y * 4 + sub_row;
                for (sub_col, &bit) in BRAILLE_DOT_BITS[sub_row as usize].iter().enumerate() {
                    let col_v = usize::from(x) * 2 + sub_col;
                    let span = spans.get(col_v).copied().flatten();
                    if span.is_some_and(|(lo, hi)| row_v >= lo && row_v <= hi) {
                        bits |= bit;
                    }
                }
            }
            if bits == 0 {
                continue; // Sin onda en esta celda: deja el eje o el fondo.
            }
            let Some(cell) = buffer.cell_mut((inner.x + x, inner.y + y)) else {
                continue;
            };
            let glyph = braille_glyph(bits);
            let mut glyph_buf = [0u8; 4];
            let symbol = glyph.encode_utf8(&mut glyph_buf);
            cell.set_symbol(symbol).set_fg(wave_color);
        }
    }
}

/// Bits de punto Braille (U+2800 + bits) por posición dentro de la celda
/// 2×4: `BRAILLE_DOT_BITS[fila_sub][col_sub]`, fila_sub 0..4 (arriba a
/// abajo), col_sub 0..2 (izquierda a derecha). Mapeo estándar del bloque
/// Unicode Braille Patterns (puntos 1..8 → bits 0x01..0x80).
const BRAILLE_DOT_BITS: [[u8; 2]; 4] = [
    [0x01, 0x08],
    [0x02, 0x10],
    [0x04, 0x20],
    [0x40, 0x80],
];

/// Carácter Braille (U+2800 + `bits`) para una máscara de puntos encendidos
/// de una celda 2×4. `bits == 0` es el patrón en blanco (U+2800); quien
/// pinta evita usarlo para no tapar el fondo con una celda "vacía" visible.
fn braille_glyph(bits: u8) -> char {
    char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ')
}

/// Para cada una de `columns` columnas (o sub-columnas Braille) virtuales,
/// en una grilla de `rows` filas virtuales, el rango `[fila_min, fila_max]`
/// que debe quedar encendido: la envolvente mín/máx propia de las muestras
/// que le tocan a esa columna (ninguna muestra se salta) MÁS la fila de
/// salida de la columna anterior (conecta sin huecos entre columnas). Una
/// columna sin muestras (ventana vacía) devuelve `None`.
fn wave_column_spans(samples: &[f32], columns: usize, rows: u16) -> Vec<Option<(u16, u16)>> {
    let mut spans = Vec::with_capacity(columns);
    let mut prev_exit: Option<u16> = None;
    let len = samples.len();
    for col in 0..columns {
        let (start, end) = wave_column_slice(len, columns, col);
        if start >= end {
            spans.push(None);
            continue;
        }
        let slice = &samples[start..end];
        let mut min_row = u16::MAX;
        let mut max_row = 0u16;
        for &sample in slice {
            let row = wave_sample_row(sample, rows);
            min_row = min_row.min(row);
            max_row = max_row.max(row);
        }
        let exit_row = wave_sample_row(slice[slice.len() - 1], rows);
        let (lo, hi) = match prev_exit {
            Some(prev) => (min_row.min(prev), max_row.max(prev)),
            None => (min_row, max_row),
        };
        spans.push(Some((lo, hi)));
        prev_exit = Some(exit_row);
    }
    spans
}

/// Reparto de `len` muestras entre `columns` columnas virtuales: proporcional
/// cuando hay más muestras que columnas (como `band_slot`, sin dejar resto
/// sin cubrir); con menos muestras que columnas, cada columna cae en la
/// muestra más cercana (puede repetirse, igual que el reescalado lineal
/// anterior). `len == 0` o `columns == 0` devuelve un rango vacío.
fn wave_column_slice(len: usize, columns: usize, col: usize) -> (usize, usize) {
    if columns == 0 || len == 0 {
        return (0, 0);
    }
    if len >= columns {
        let start = col * len / columns;
        let end = ((col + 1) * len / columns).max(start + 1).min(len);
        (start, end)
    } else {
        let idx = (col * len) / columns;
        (idx, (idx + 1).min(len))
    }
}

/// Muestra en [-1, 1] → fila desde arriba (0 = arriba), con clamp. El cero
/// cae en el centro redondeado; con 1 fila todo va a la fila 0.
fn wave_sample_row(sample: f32, height: u16) -> u16 {
    if height <= 1 {
        return 0;
    }
    let clamped = sample.clamp(-1.0, 1.0);
    ((1.0 - clamped) * 0.5 * f32::from(height - 1)).round() as u16
}

/// Pinta las 32 barras dentro del área interior del bloque, sin widgets.
/// Usa bloques de octavo de celda para la celda parcial superior.
/// El gap entre barras está garantizado cuando el slot mide >= 2:
///   - slot >= 2 → bar_width = slot - 1 ≥ 1 (gap de 1 col)
///   - slot == 1 → bar_width = 1 (sin espacio: no hay gap posible)
///
/// Los slots se reparten proporcionalmente, así las barras ocupan todo el ancho.
fn paint_bands(
    buffer: &mut ratatui::buffer::Buffer,
    inner: Rect,
    bands: &[f32; NUM_BANDS],
    theme: &Theme,
    ascii: bool,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let bottom = inner.y + inner.height;
    let total = u32::from(inner.width);

    for (band, &magnitude) in bands.iter().enumerate() {
        // Los slots se reparten proporcionalmente para cubrir todo el ancho
        // (sin dejar el resto de la división entera sin pintar). Si hay menos
        // columnas que bandas, cada banda ocupa una columna y `band_slot`
        // devuelve `None` al llegar al borde.
        let Some((slot_start, width)) = band_slot(band, total) else {
            break;
        };
        let x0 = inner.x + slot_start;

        // Sub-celda: altura continua → celdas completas + fracción.
        let height_f = magnitude.clamp(0.0, 1.0) * inner.height as f32;
        let full_cells = height_f.floor() as u16;
        let frac = height_f - height_f.floor();

        // Pintar las celdas completas (█ con gradiente de color; @ en ascii).
        for row in 0..full_cells {
            let y = bottom - 1 - row;
            let t = if inner.height > 1 {
                row as f32 / (inner.height - 1) as f32
            } else {
                1.0
            };
            let (red, green, blue) = spectrum_color(t, theme);
            let color = Color::Rgb(red, green, blue);
            let symbol = band_full_glyph(ascii);
            for dx in 0..width {
                if let Some(cell) = buffer.cell_mut((x0 + dx, y)) {
                    cell.set_symbol(symbol).set_fg(color);
                }
            }
        }

        // Pintar la celda parcial: bloque de octavo en unicode, rampa de 10
        // niveles en ascii. El color es el mismo gradiente en ambos modos.
        if frac > 0.0 && full_cells < inner.height {
            let y = bottom - full_cells - 1;
            let symbol = band_frac_glyph(frac, ascii);
            // Color: el mismo gradiente que tendría si fuera una celda completa.
            let t = if inner.height > 1 {
                full_cells as f32 / (inner.height - 1) as f32
            } else {
                1.0
            };
            let (red, green, blue) = spectrum_color(t, theme);
            let color = Color::Rgb(red, green, blue);
            for dx in 0..width {
                if let Some(cell) = buffer.cell_mut((x0 + dx, y)) {
                    cell.set_symbol(symbol).set_fg(color);
                }
            }
        }

        // Limpiar las celdas vacías por encima de la barra: espacio en
        // unicode, fondo de puntos tenue en ascii.
        let painted_rows = full_cells + if frac > 0.0 { 1 } else { 0 };
        for row in painted_rows..inner.height {
            let y = bottom - 1 - row;
            for dx in 0..width {
                if let Some(cell) = buffer.cell_mut((x0 + dx, y)) {
                    if ascii {
                        cell.set_symbol(".").set_fg(theme.dim);
                    } else {
                        cell.set_symbol(" ").set_fg(Color::Reset);
                    }
                }
            }
        }
    }
}

/// Verde (abajo) → amarillo (medio) → rojo (arriba), estilo cava: interpola
/// linealmente entre los tres colores del tema.
fn spectrum_color(t: f32, theme: &Theme) -> (u8, u8, u8) {
    fn channel(from: u8, to: u8, u: f32) -> u8 {
        (f64::from(from) + (f64::from(to) - f64::from(from)) * f64::from(u)).round() as u8
    }

    fn mix(from: Color, to: Color, u: f32) -> (u8, u8, u8) {
        let (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) = (from, to) else {
            return (0, 0, 0);
        };
        (
            channel(r0, r1, u),
            channel(g0, g1, u),
            channel(b0, b1, u),
        )
    }

    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        mix(theme.spectrum_low, theme.spectrum_mid, t * 2.0)
    } else {
        mix(theme.spectrum_mid, theme.spectrum_high, (t - 0.5) * 2.0)
    }
}

/// El pie de atajos creció a dos líneas: shuffle/repeat/n-p/buscar no
/// entraban en una. Con la búsqueda abierta muestra su propio contexto.
/// Pie de atajos: se arma desde la config para que muestre siempre las
/// teclas reales aunque el usuario las remapee.
///
/// `compact` activo cuando `area.height < 30`; muestra una sola
/// línea con los atajos críticos.
fn footer_lines(app: &App, compact: bool) -> Vec<Line<'static>> {
    if app.url_input.is_some() {
        return vec![
            Line::from(" Pegá o escribí una URL (directa o YouTube) o ruta local "),
            Line::from(" Enter: reproducir   Esc: cancelar   (pegar con el terminal) "),
        ];
    }

    if app.theme_menu.is_some() {
        return vec![Line::from(
            " ↑/↓: previsualizar tema   ·   Enter: guardar   ·   Esc: cancelar ",
        )];
    }

    // Selector de bandas: mismo patrón que el menú de temas.
    if app.bands_menu.is_some() {
        return vec![Line::from(
            " ↑/↓: previsualizar estilo   ·   Enter: guardar   ·   Esc: cancelar ",
        )];
    }

    // Selector de modo: mismo patrón que el de bandas.
    if app.mode_menu.is_some() {
        return vec![Line::from(
            " ↑/↓: previsualizar modo   ·   Enter: guardar   ·   Esc: cancelar ",
        )];
    }

    // Panel de configuración: Enter abre
    // el sub-modal de Temas en la primera fila, el de Bandas en la tercera y
    // el de Modo en la cuarta; en «Mouse» alterna la captura en vivo (el
    // pie dice `abrir/alternar` por eso). Los `·` son tramos sueltos entre
    // chips: el parser de chips separa por tres espacios, así todos los
    // atajos siguen clickeables.
    if app.settings_panel.is_some() {
        return vec![Line::from(
            " ↑/↓: mover   ·   Enter: abrir/alternar   ·   Esc: cerrar ",
        )];
    }

    if app.search.is_some() {
        return vec![
            Line::from(" Escribí para filtrar por título, artista, álbum o archivo "),
            Line::from(
                " Backspace: borrar   Enter: aplicar   Esc: cancelar (lista completa) ",
            ),
        ];
    }

    if compact {
        let keys = &config::config().keys;
        return vec![Line::from(format!(
            " {}: pausa   {}×2: salir",
            key_labels(&keys.play_pause),
            key_labels(&keys.quit),
        ))];
    }

    let keys = &config::config().keys;
    match app.mode {
        // Panel lateral abierto: 3 líneas — lista, reproducción y el
        // recordatorio de que ↑/↓ son volumen solo con el panel cerrado.
        // `u: URL` también vale con el panel abierto; `sig/ant` se
        // expande porque entra en 78 reagrupando (`u` pasa a la línea 2).
        // La línea de acciones de la lista cambia según la solapa: en
        // Biblioteca `a: encolar`; en Cola, `d: borrar` y `c: biblio`.
        Mode::Player if app.explorer_open => {
            let list_hint = if app.queue_panel {
                format!(
                    " {}: pausa   {}/{}: ±5s   {}: borrar   {}: biblio   {}: URL   {}×2: salir",
                    key_labels(&keys.play_pause),
                    key_labels(&keys.seek_back),
                    key_labels(&keys.seek_fwd),
                    key_labels(&keys.queue_delete),
                    key_labels(&keys.queue_panel),
                    key_labels(&keys.open_url),
                    key_labels(&keys.quit),
                )
            } else {
                format!(
                    " {}: pausa   {}/{}: ±5s   {}: encolar   {}: cola   {}: URL   {}×2: salir",
                    key_labels(&keys.play_pause),
                    key_labels(&keys.seek_back),
                    key_labels(&keys.seek_fwd),
                    key_labels(&keys.enqueue),
                    key_labels(&keys.queue_panel),
                    key_labels(&keys.open_url),
                    key_labels(&keys.quit),
                )
            };
            vec![
                Line::from(format!(
                    " {}/{}: mover   {}: reproducir   Ctrl+{}: config   {}/{}: cerrar",
                    key_labels(&keys.move_up),
                    key_labels(&keys.move_down),
                    key_labels(&keys.select),
                    settings_key_display(),
                    key_labels(&keys.back),
                    key_labels(&keys.explorer),
                )),
                Line::from(list_hint),
                Line::from(format!(
                    " {}/{}: pista siguiente/anterior   volumen {}/{}: con el panel cerrado   {}/{}: vel",
                    key_labels(&keys.next_track),
                    key_labels(&keys.prev_track),
                    key_labels(&keys.volume_up),
                    key_labels(&keys.volume_down),
                    key_labels(&keys.speed_down),
                    key_labels(&keys.speed_up),
                )),
            ]
        },
        // `Tab: biblio` vuelve; `b` sube a la línea 1 como
        // navegación y `↑/↓: vol` abrevia: es la única forma de que los 12
        // atajos entren en 2×78. Orden: navegación → reproducción → modos →
        // config/salida.
        Mode::Player => vec![
            Line::from(format!(
                " {}: biblio   {}: buscar   {}: pausa   {}/{}: vol   {}/{}: ±5s   \
                 {}/{}: pista",
                key_labels(&keys.explorer),
                key_labels(&keys.search),
                key_labels(&keys.play_pause),
                key_labels(&keys.volume_up),
                key_labels(&keys.volume_down),
                key_labels(&keys.seek_back),
                key_labels(&keys.seek_fwd),
                key_labels(&keys.next_track),
                key_labels(&keys.prev_track),
            )),
            Line::from(format!(
                " {}: shuffle   {}: repeat   {}/{}: vel   {}: URL   Ctrl+{}: config   {}×2: salir",
                key_labels(&keys.shuffle),
                key_labels(&keys.repeat),
                key_labels(&keys.speed_down),
                key_labels(&keys.speed_up),
                key_labels(&keys.open_url),
                settings_key_display(),
                key_labels(&keys.quit),
            )),
        ],
        // `sig/ant` expandido (`b` baja a la línea 2 para que entre);
        // `Inicio/Fin` se documenta y sale el `Esc: volver` inexacto (sin
        // player Esc arma la salida: lo cubre `Esc×2: salir`).
        Mode::Browser => vec![
            Line::from(format!(
                " {}/{} {}/{}: mover   {}: reproducir   {}/{}: pista siguiente/anterior",
                key_labels(&keys.move_up),
                key_labels(&keys.move_down),
                key_labels(&keys.page_up),
                key_labels(&keys.page_down),
                key_labels(&keys.select),
                key_labels(&keys.next_track),
                key_labels(&keys.prev_track),
            )),
            Line::from(format!(
                " {}: pausa   {}/{}: ±5s   {}/{}: vel   {}: shuffle   {}: repeat   {}: buscar",
                key_labels(&keys.play_pause),
                key_labels(&keys.seek_back),
                key_labels(&keys.seek_fwd),
                key_labels(&keys.speed_down),
                key_labels(&keys.speed_up),
                key_labels(&keys.shuffle),
                key_labels(&keys.repeat),
                key_labels(&keys.search),
            )),
            Line::from(format!(
                " {}/{}: primera/última   {}: URL   Ctrl+{}: config   {}×2: salir",
                key_labels(&keys.home),
                key_labels(&keys.end),
                key_labels(&keys.open_url),
                settings_key_display(),
                key_labels(&keys.quit),
            )),
        ],
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App, compact: bool) {
    // Mismo texto que `footer_lines`, dibujado como chips; cada chip que es un
    // atajo queda clickeable (simula su tecla).
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let mut lines = Vec::new();
    {
        let mut hit = app.hit.borrow_mut();
        for (index, line) in footer_lines(app, compact).into_iter().enumerate() {
            let text: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
            let line_width = line.width();
            let (spans, entries) = footer_chip_spans(&text);
            lines.push(Line::from(spans));
            let Ok(index) = u16::try_from(index) else { continue };
            if index >= inner.height {
                continue;
            }
            let Ok(line_width) = u16::try_from(line_width) else { continue };
            // Mismo centrado que el Paragraph de abajo (ratatui centra con
            // `(ancho/2) - (línea/2)`, no con `(ancho - línea)/2`: con resto
            // impar difieren en una columna y el chip quedaba corrido).
            let x0 = inner.x + (inner.width / 2).saturating_sub(line_width / 2);
            let right = inner.x + inner.width;
            for (start, width, keys) in entries {
                let Some(code) = key_for_chip(&keys) else { continue };
                let x = x0.saturating_add(start);
                if x >= right {
                    continue;
                }
                let width = width.min(right - x);
                hit.chips.push((Rect::new(x, inner.y + index, width, 1), code));
            }
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .border_style(Style::default().fg(theme().dim))
                    .title(accent_title("Atajos")),
            )
            .style(Style::default().fg(theme().dim))
            .alignment(Alignment::Center),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;

    fn sample_track() -> Track {
        Track {
            path: PathBuf::from(r"C:\music\cancion.mp3"),
            title: "Una canción con un título bastante largo".to_string(),
            artist: Some("Artista".to_string()),
            album: Some("Álbum".to_string()),
            duration: Some(Duration::from_secs(65)),
            folder: "music".to_string(),
        }
    }

    fn line_width(line: &Line<'_>) -> usize {
        line.spans.iter().map(|span| span.content.chars().count()).sum()
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.to_string()).collect()
    }

    #[test]
    fn row_fills_exact_width_and_duration_at_right() {
        let track = sample_track();
        let line = row_line(&track, 1, true, false, 40);
        assert_eq!(line_width(&line), 40, "la fila debe ocupar todo el ancho");
        let text = line_text(&line);
        assert!(text.starts_with('▌'), "cursor de selección: {text}");
        assert!(text.ends_with(" 1:05"), "duración a la derecha: {text}");
    }

    #[test]
    fn row_shows_playing_marker_and_number() {
        let track = sample_track();
        let line = row_line(&track, 12, false, true, 60);
        let text = line_text(&line);
        assert!(text.contains('▶'), "marcador de reproducción: {text}");
        assert!(text.contains("12. "), "número de pista: {text}");
        assert!(!text.starts_with('▌'), "sin selección no hay cursor");
    }

    #[test]
    fn row_truncates_very_long_titles_instead_of_overflowing() {
        let mut track = sample_track();
        track.title = "x".repeat(300);
        for width in [10usize, 20, 40] {
            let line = row_line(&track, 1, false, false, width as u16);
            assert_eq!(line_width(&line), width, "width={width}");
            assert!(line_text(&line).contains('\u{2026}'), "debe recortar con …");
        }
    }

    #[test]
    fn duration_format_is_human_readable() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0:00");
        assert_eq!(format_duration(Duration::from_secs(65)), "1:05");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn spectrum_color_extremes() {
        let theme = Theme::default();
        assert_eq!(
            spectrum_color(0.0, &theme),
            (0x89, 0x92, 0xA7),
            "base del gradiente = spectrum_low"
        );
        assert_eq!(
            spectrum_color(0.5, &theme),
            (0x7F, 0xB4, 0xCA),
            "mitad = spectrum_mid"
        );
        assert_eq!(
            spectrum_color(1.0, &theme),
            (0xC4, 0x74, 0x6E),
            "pico = spectrum_high"
        );
    }

    #[test]
    fn paint_bands_full_bar_is_violet_at_bottom_and_red_at_top() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 1.0;
        paint_bands(&mut buffer, area, &bands, &theme, false);

        let bottom = &buffer[(0u16, 9u16)];
        assert_eq!(bottom.symbol(), "\u{2588}");
        assert_eq!(
            bottom.fg,
            Color::Rgb(0x89, 0x92, 0xA7),
            "violeta de la base"
        );

        let top = &buffer[(0u16, 0u16)];
        assert_eq!(top.symbol(), "\u{2588}");
        assert_eq!(top.fg, Color::Rgb(0xC4, 0x74, 0x6E), "rojo del pico");

        let mid = &buffer[(0u16, 5u16)];
        assert_eq!(mid.symbol(), "\u{2588}");
        // La barra mapea t = (alto - 1 - y) / (alto - 1); fila 5 de 10 → 4/9.
        let expected = spectrum_color(4.0 / 9.0, &theme);
        assert_eq!(mid.fg, Color::Rgb(expected.0, expected.1, expected.2));
    }

    #[test]
    fn paint_bands_zero_leaves_empty_reset_cells() {
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        paint_bands(&mut buffer, area, &[0.0f32; NUM_BANDS], &Theme::default(), false);

        let cell = &buffer[(0u16, 9u16)];
        assert_eq!(cell.symbol(), " ");
        assert_eq!(cell.fg, Color::Reset);
    }

    #[test]
    fn paint_bands_half_height_covers_bottom_half_only() {
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 0.5;
        paint_bands(&mut buffer, area, &bands, &Theme::default(), false);

        // 0.5 * 10 = 5.0 → full_cells = 5, frac = 0.0 → no partial cell.
        // Rows y=9..5 (bottom 5) are full █, y=4 is empty.
        assert_eq!(buffer[(0u16, 5u16)].symbol(), "\u{2588}");
        assert_eq!(buffer[(0u16, 4u16)].symbol(), " ");
        assert_eq!(buffer[(0u16, 4u16)].fg, Color::Reset);
    }

    #[test]
    fn paint_bands_partial_cell_uses_eighth_block() {
        // magnitude = 0.45, inner.height = 10
        // height_f = 0.45 * 10 = 4.5
        // full_cells = 4, frac = 0.5
        // idx = (0.5 * 8.0) as usize = 4 → EIGHTH_BLOCKS[4] = "▅"
        // partial cell y = bottom - full_cells - 1 = 10 - 4 - 1 = 5
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 0.45;
        paint_bands(&mut buffer, area, &bands, &Theme::default(), false);

        let bottom = area.y + area.height; // = 10
        let partial_y = bottom - 4 - 1; // = 5
        assert_eq!(
            buffer[(0u16, partial_y)].symbol(),
            "▅",
            "frac=0.5 → EIGHTH_BLOCKS[4] = ▅"
        );
        // The full cells below the partial cell should be █
        assert_eq!(buffer[(0u16, bottom - 1)].symbol(), "█", "bottom full cell");
        // The cell above the partial cell should be empty
        assert_eq!(buffer[(0u16, partial_y - 1)].symbol(), " ", "above partial = empty");
    }

    #[test]
    fn paint_bands_spreads_bars_across_full_width_when_not_multiple_of_bands() {
        // 113 columnas / 32 bandas: la división entera dejaba ~17 columnas vacías.
        let width = 113u16;
        let area = Rect::new(0, 0, width, 3);
        let mut buffer = Buffer::empty(area);
        paint_bands(&mut buffer, area, &[1.0f32; NUM_BANDS], &Theme::default(), false);

        let rightmost = (0..width)
            .rev()
            .find(|&x| buffer[(x, 2u16)].symbol() == "\u{2588}")
            .expect("alguna columna pintada");
        // La última barra deja a lo sumo 1 columna de gap antes del borde.
        assert!(
            rightmost >= width - 2,
            "las barras deben llegar al borde derecho, última columna pintada: {rightmost}"
        );
    }

    #[test]
    fn ascii_ramp_has_ten_levels_from_empty_to_full() {
        assert_eq!(ASCII_RAMP.len(), 10, "la rampa ascii tiene 10 niveles");
        assert_eq!(ASCII_RAMP[0], " ", "índice 0 = vacío");
        assert_eq!(ASCII_RAMP[9], "@", "índice 9 = lleno");
        assert_eq!(
            ASCII_RAMP,
            [" ", ".", ":", "-", "=", ";", "+", "*", "#", "@"]
        );
    }

    #[test]
    fn paint_bands_ascii_full_bar_uses_at_with_same_gradient() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 1.0;
        paint_bands(&mut buffer, area, &bands, &theme, true);

        let bottom = &buffer[(0u16, 9u16)];
        assert_eq!(bottom.symbol(), "@", "celda llena en ascii");
        assert_eq!(
            bottom.fg,
            Color::Rgb(0x89, 0x92, 0xA7),
            "el gradiente no cambia en ascii"
        );

        let top = &buffer[(0u16, 0u16)];
        assert_eq!(top.symbol(), "@");
        assert_eq!(top.fg, Color::Rgb(0xC4, 0x74, 0x6E), "rojo del pico");
    }

    #[test]
    fn paint_bands_ascii_empty_cells_are_dim_dots() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        paint_bands(&mut buffer, area, &[0.0f32; NUM_BANDS], &theme, true);

        let cell = &buffer[(0u16, 9u16)];
        assert_eq!(cell.symbol(), ".", "fondo de puntos en ascii");
        assert_eq!(cell.fg, theme.dim);
    }

    #[test]
    fn paint_bands_ascii_partial_cell_uses_ramp() {
        // magnitude = 0.45, alto 10 → height_f = 4.5, full = 4, frac = 0.5
        // idx = (0.5 * 10.0) as usize = 5 → ASCII_RAMP[5] = ";"
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 0.45;
        paint_bands(&mut buffer, area, &bands, &theme, true);

        let bottom = area.y + area.height; // = 10
        let partial_y = bottom - 4 - 1; // = 5
        assert_eq!(
            buffer[(0u16, partial_y)].symbol(),
            ";",
            "frac=0.5 → ASCII_RAMP[5] = ;"
        );
        // Mismo gradiente que tendría esa celda en unicode.
        let expected = spectrum_color(4.0 / 9.0, &theme);
        assert_eq!(
            buffer[(0u16, partial_y)].fg,
            Color::Rgb(expected.0, expected.1, expected.2)
        );
        assert_eq!(buffer[(0u16, bottom - 1)].symbol(), "@", "llenas = @");
        let above = &buffer[(0u16, partial_y - 1)];
        assert_eq!(above.symbol(), ".", "sobre la barra = punto tenue");
        assert_eq!(above.fg, theme.dim);
    }

    // Osciloscopio `onda` con muestras crudas.
    // Área 8x7 → eje cero en fila 3 (0.5 * 6 = 3.0); la onda tapa al eje
    // donde lo cruza y el eje asoma donde la onda se desvía.
    // ------------------------------------------------------------------

    #[test]
    fn paint_wave_empty_samples_draws_dim_axis_only() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 7);
        // ASCII: eje `-` tenue, resto fondo de puntos.
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[], &theme, true);
        for x in 0..8u16 {
            let axis = &buffer[(x, 3u16)];
            assert_eq!(axis.symbol(), "-", "eje cero en fila 3 (x={x})");
            assert_eq!(axis.fg, theme.dim);
            for y in [0u16, 1, 2, 4, 5, 6] {
                let cell = &buffer[(x, y)];
                assert_eq!(cell.symbol(), ".", "fondo de puntos en ({x},{y})");
                assert_eq!(cell.fg, theme.dim);
            }
        }
        // Unicode: eje `─` tenue, resto vacío.
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[], &theme, false);
        for x in 0..8u16 {
            let axis = &buffer[(x, 3u16)];
            assert_eq!(axis.symbol(), "─", "eje cero unicode en fila 3");
            assert_eq!(axis.fg, theme.dim);
            assert_eq!(buffer[(x, 0u16)].symbol(), " ");
        }
    }

    #[test]
    fn paint_wave_ascii_silence_is_flat_hash_line_at_center() {
        // Sin señal la onda ES el cero: tapa al eje con una línea plana.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 7);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[0.0f32; 8], &theme, true);
        for x in 0..8u16 {
            assert_eq!(buffer[(x, 3u16)].symbol(), "#", "plana en el centro");
        }
        // Fila 3 de 7 → t = (6 - 3) / 6 = 0.5 → spectrum_mid.
        let expected = spectrum_color(0.5, &theme);
        assert_eq!(
            buffer[(0u16, 3u16)].fg,
            Color::Rgb(expected.0, expected.1, expected.2),
            "la onda usa el gradiente del tema por altura"
        );
        assert_eq!(buffer[(0u16, 0u16)].symbol(), ".");
    }

    #[test]
    fn paint_wave_newest_sample_at_right() {
        // La más reciente a la derecha: última muestra = 1.0 → arriba, y el
        // salto brusco desde el centro queda conectado (sin hueco) en esa
        // misma columna.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 7);
        let mut buffer = Buffer::empty(area);
        let samples = [0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        paint_wave(&mut buffer, area, &samples, &theme, true);
        assert_eq!(
            buffer[(7u16, 0u16)].symbol(),
            "#",
            "1.0 mapea a la fila de arriba"
        );
        assert_eq!(
            buffer[(7u16, 3u16)].symbol(),
            "#",
            "el salto top-centro queda conectado, sin hueco, en la misma columna"
        );
        assert_eq!(
            buffer[(0u16, 3u16)].symbol(),
            "#",
            "la primera (más vieja) queda a la izquierda, en el centro"
        );
        // Pico arriba → t = 1.0 → spectrum_high.
        let expected = spectrum_color(1.0, &theme);
        assert_eq!(
            buffer[(7u16, 0u16)].fg,
            Color::Rgb(expected.0, expected.1, expected.2)
        );
    }

    #[test]
    fn paint_wave_clamps_out_of_range_without_panic() {
        // El salto de +2.0 (clampa arriba) a -3.0 (clampa abajo) entre dos
        // columnas consecutivas conecta la columna entera, sin pánico ni
        // hueco.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 2, 5);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[2.0f32, -3.0], &theme, true);
        assert_eq!(buffer[(0u16, 0u16)].symbol(), "#", "2.0 se clampa arriba");
        assert_eq!(buffer[(1u16, 0u16)].symbol(), "#", "conectado hasta arriba");
        assert_eq!(buffer[(1u16, 4u16)].symbol(), "#", "-3.0 se clampa abajo");
    }

    #[test]
    fn paint_wave_tiny_and_zero_sized_areas_degrade_without_panic() {
        // Precedente: los menús no entran en pánico en terminales enanas;
        // la onda tampoco (0×0, 1×1, 1–2 filas).
        let theme = Theme::default();
        let samples = [0.5f32, -0.5, 1.0, -1.0, 0.25, -0.25, 0.75, -0.75];

        // 1 fila: con `height <= 1`, toda muestra mapea a la fila 0.
        let area = Rect::new(0, 0, 8, 1);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &samples, &theme, true);
        for x in 0..8u16 {
            assert_eq!(buffer[(x, 0u16)].symbol(), "#", "1 fila: todo a la fila 0");
        }

        // 2 filas: eje en (0.5 * 1).round() = 1 → fila de abajo.
        let area = Rect::new(0, 0, 8, 2);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[0.0f32; 8], &theme, true);
        for x in 0..8u16 {
            assert_eq!(buffer[(x, 1u16)].symbol(), "#", "2 filas: centro abajo");
        }

        // 1×1 y 0×0, ascii y unicode: no deben entrar en pánico.
        let area = Rect::new(0, 0, 1, 1);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &samples, &theme, true);
        paint_wave(&mut buffer, area, &samples, &theme, false);
        let area = Rect::new(0, 0, 0, 0);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        paint_wave(&mut buffer, area, &samples, &theme, true);
        paint_wave(&mut buffer, area, &[], &theme, false);
    }

    #[test]
    fn paint_wave_unicode_uses_braille_glyphs_for_the_trace() {
        // El trazo en unicode se compone de caracteres Braille (bloque
        // U+2800..=U+28FF): 2×4 puntos por celda.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 12, 8);
        let mut buffer = Buffer::empty(area);
        let samples: Vec<f32> = (0..200)
            .map(|i| (std::f32::consts::TAU * 3.0 * i as f32 / 200.0).sin())
            .collect();
        paint_wave(&mut buffer, area, &samples, &theme, false);
        let mut saw_braille = false;
        for y in 0..8u16 {
            for x in 0..12u16 {
                let symbol = buffer[(x, y)].symbol();
                let Some(ch) = symbol.chars().next() else {
                    continue;
                };
                if symbol == " " || symbol == "─" {
                    continue;
                }
                assert!(
                    ('\u{2800}'..='\u{28FF}').contains(&ch),
                    "la onda en unicode debe ser Braille, obtenido {symbol:?} en ({x},{y})"
                );
                saw_braille = true;
            }
        }
        assert!(saw_braille, "debe haber al menos una celda con trazo Braille");
    }

    #[test]
    fn wave_column_spans_has_no_column_to_column_gaps_for_a_full_scale_sine() {
        // Continuidad: con un seno a escala completa sobre un área 40x10,
        // toda columna tiene celda pintada y columnas adyacentes se tocan
        // o se superponen verticalmente (ningún hueco).
        let samples: Vec<f32> = (0..400)
            .map(|i| (std::f32::consts::TAU * 7.0 * i as f32 / 400.0).sin())
            .collect();
        let spans = wave_column_spans(&samples, 40, 10);
        assert_eq!(spans.len(), 40);
        for (col, span) in spans.iter().enumerate() {
            assert!(span.is_some(), "columna sin celda pintada: {col}");
        }
        for col in 1..40 {
            let (lo_prev, hi_prev) = spans[col - 1].unwrap();
            let (lo_cur, hi_cur) = spans[col].unwrap();
            assert!(
                lo_cur.max(lo_prev) <= hi_cur.min(hi_prev),
                "hueco entre las columnas {} y {col}: prev=({lo_prev},{hi_prev}) cur=({lo_cur},{hi_cur})",
                col - 1
            );
        }
    }

    #[test]
    fn paint_wave_ascii_uses_hash_and_axis_shows_where_wave_leaves() {
        let theme = Theme::default();
        // Alto 5 → eje en fila 2 (0.5 * 4 = 2.0).
        let area = Rect::new(0, 0, 4, 5);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[1.0f32, -1.0, 0.0, 0.0], &theme, true);
        assert_eq!(buffer[(0u16, 0u16)].symbol(), "#", "1.0 arriba con #");
        assert_eq!(buffer[(1u16, 4u16)].symbol(), "#", "-1.0 abajo con #");
        assert_eq!(buffer[(2u16, 2u16)].symbol(), "#", "el cero tapa al eje");
        assert_eq!(buffer[(0u16, 2u16)].symbol(), "-", "el eje asoma en x=0");
        assert_eq!(buffer[(0u16, 2u16)].fg, theme.dim);
        let empty = &buffer[(0u16, 1u16)];
        assert_eq!(empty.symbol(), ".", "celda vacía = punto tenue");
        assert_eq!(empty.fg, theme.dim);
    }

    // Espejo vertical de las 32 bandas.
    // Área 32x7 → 1 columna por banda; eje central en fila 3 (igual que la
    // onda: `wave_sample_row(0.0, 7)`), 3 filas por lado.
    // ------------------------------------------------------------------

    #[test]
    fn paint_mirror_full_bar_grows_both_ways_from_dim_center_axis() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 7);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 1.0;
        paint_mirror(&mut buffer, area, &bands, &theme, false);
        for y in [0u16, 1, 2, 4, 5, 6] {
            assert_eq!(
                buffer[(0u16, y)].symbol(),
                "█",
                "barra a ambos lados del eje (y={y})"
            );
        }
        let axis = &buffer[(0u16, 3u16)];
        assert_eq!(axis.symbol(), "─", "línea central tenue");
        assert_eq!(axis.fg, theme.dim);
        // Las demás bandas en cero: solo el eje, resto vacío.
        for x in 1..32u16 {
            assert_eq!(
                buffer[(x, 3u16)].symbol(),
                "─",
                "el eje cruza todo el ancho (x={x})"
            );
            assert_eq!(
                buffer[(x, 0u16)].symbol(),
                " ",
                "sin barra = vacío en unicode"
            );
        }
    }

    #[test]
    fn paint_mirror_gradient_goes_from_low_at_center_to_high_at_tips() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 7);
        let mut buffer = Buffer::empty(area);
        paint_mirror(&mut buffer, area, &[1.0f32; NUM_BANDS], &theme, false);
        let low = spectrum_color(0.0, &theme);
        let high = spectrum_color(1.0, &theme);
        for (y, label) in [(2u16, "arriba junto al eje"), (4u16, "abajo junto al eje")] {
            assert_eq!(
                buffer[(0u16, y)].fg,
                Color::Rgb(low.0, low.1, low.2),
                "{label} = spectrum_low"
            );
        }
        for (y, label) in [(0u16, "punta de arriba"), (6u16, "punta de abajo")] {
            assert_eq!(
                buffer[(0u16, y)].fg,
                Color::Rgb(high.0, high.1, high.2),
                "{label} = spectrum_high"
            );
        }
    }

    #[test]
    fn paint_mirror_reuses_band_glyph_levels_and_dot_background() {
        let theme = Theme::default();
        // Mitad de nivel con lado de 3: 1.5 → 1 llena + parcial 0.5.
        let area = Rect::new(0, 0, 32, 7);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 0.5;
        let mut buffer = Buffer::empty(area);
        paint_mirror(&mut buffer, area, &bands, &theme, false);
        assert_eq!(buffer[(0u16, 2u16)].symbol(), "█", "llena junto al eje");
        assert_eq!(
            buffer[(0u16, 1u16)].symbol(),
            "▅",
            "parcial 0.5 = EIGHTH_BLOCKS[4], como paint_bands"
        );
        assert_eq!(buffer[(0u16, 4u16)].symbol(), "█", "abajo simétrico");
        assert_eq!(buffer[(0u16, 5u16)].symbol(), "▅", "parcial simétrico");
        // ASCII: misma rampa que las barras + fondo de puntos.
        let mut buffer = Buffer::empty(area);
        paint_mirror(&mut buffer, area, &bands, &theme, true);
        assert_eq!(buffer[(0u16, 2u16)].symbol(), "@", "llena ascii = @");
        assert_eq!(
            buffer[(0u16, 1u16)].symbol(),
            ";",
            "parcial 0.5 = ASCII_RAMP[5], como paint_bands"
        );
        let empty = &buffer[(5u16, 0u16)];
        assert_eq!(empty.symbol(), ".", "fondo de puntos en ascii");
        assert_eq!(empty.fg, theme.dim);
        let axis = &buffer[(5u16, 3u16)];
        assert_eq!(axis.symbol(), "-", "eje ascii");
        assert_eq!(axis.fg, theme.dim);
    }

    #[test]
    fn paint_mirror_tiny_heights_degrade_without_panic() {
        let theme = Theme::default();
        let bands = [1.0f32; NUM_BANDS];
        let area = Rect::new(0, 0, 32, 1);
        let mut buffer = Buffer::empty(area);
        paint_mirror(&mut buffer, area, &bands, &theme, false);
        for x in 0..32u16 {
            assert_eq!(buffer[(x, 0u16)].symbol(), "─", "1 fila: solo el eje");
        }
        let area = Rect::new(0, 0, 32, 2);
        let mut buffer = Buffer::empty(area);
        paint_mirror(&mut buffer, area, &bands, &theme, false);
        for x in 0..32u16 {
            assert_eq!(buffer[(x, 1u16)].symbol(), "─", "el eje sigue visible");
            assert_eq!(
                buffer[(x, 0u16)].symbol(),
                "█",
                "arriba de 1 fila pinta sin pánico"
            );
        }
    }

    // VU mono horizontal con RMS.
    // Área 32x5 → barra en filas 0–3, escala tenue en fila 4.
    // ------------------------------------------------------------------

    #[test]
    fn paint_vu_full_level_fills_bar_with_gradient_and_peak_mark() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 5);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 1.0, 1.0, &theme, false);
        // Barra llena de izquierda a derecha en todas las filas de barra.
        for y in 0..4u16 {
            assert_eq!(buffer[(0u16, y)].symbol(), "█", "llena (y={y})");
            assert_eq!(buffer[(31u16, y)].symbol(), "█", "hasta el borde (y={y})");
        }
        // Gradiente del tema de izquierda (low) a derecha (high).
        let low = spectrum_color(0.0, &theme);
        let high = spectrum_color(1.0, &theme);
        assert_eq!(
            buffer[(0u16, 0u16)].fg,
            Color::Rgb(low.0, low.1, low.2),
            "izquierda = spectrum_low"
        );
        assert_eq!(
            buffer[(31u16, 0u16)].fg,
            Color::Rgb(high.0, high.1, high.2),
            "derecha = spectrum_high"
        );
        // Pico al tope: marca en spectrum_high que tapa la barra.
        assert_eq!(buffer[(31u16, 1u16)].fg, theme.spectrum_high);
        // Escala tenue en la última fila.
        assert_eq!(buffer[(0u16, 4u16)].fg, theme.dim, "escala en dim");
        assert_eq!(buffer[(5u16, 4u16)].fg, theme.dim, "escala en dim");
    }

    #[test]
    fn paint_vu_partial_level_uses_frac_glyph_and_dot_background() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 5);
        // 0.3 * 32 = 9.6 → 9 llenas + parcial 0.6.
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.3, 0.0, &theme, false);
        assert_eq!(buffer[(8u16, 0u16)].symbol(), "█", "llenas a la izquierda");
        assert_eq!(
            buffer[(9u16, 0u16)].symbol(),
            "▅",
            "parcial 0.6 = EIGHTH_BLOCKS[4], como paint_bands"
        );
        assert_eq!(buffer[(10u16, 0u16)].symbol(), " ", "vacío en unicode");
        // Sin pico no hay marca en spectrum_high.
        for x in 0..32u16 {
            assert_ne!(
                buffer[(x, 0u16)].fg,
                theme.spectrum_high,
                "sin pico no hay marca (x={x})"
            );
        }
        // ASCII: misma rampa que las barras + fondo de puntos.
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.3, 0.0, &theme, true);
        assert_eq!(buffer[(8u16, 0u16)].symbol(), "@", "llena ascii = @");
        assert_eq!(
            buffer[(9u16, 0u16)].symbol(),
            "+",
            "parcial 0.6 = ASCII_RAMP[6], como paint_bands"
        );
        let empty = &buffer[(20u16, 0u16)];
        assert_eq!(empty.symbol(), ".", "fondo de puntos en ascii");
        assert_eq!(empty.fg, theme.dim);
        let scale = &buffer[(0u16, 4u16)];
        assert_eq!(scale.symbol(), "|", "tics de escala ascii");
        assert_eq!(scale.fg, theme.dim);
    }

    #[test]
    fn paint_vu_peak_marks_above_bar_in_spectrum_high() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 5);
        let mut buffer = Buffer::empty(area);
        // Nivel 0.25 (8 cols) con pico en 0.5 (col 16): la marca flota sobre
        // el vacío, en todas las filas de barra.
        paint_vu(&mut buffer, area, 0.25, 0.5, &theme, false);
        for y in 0..4u16 {
            let marker = &buffer[(16u16, y)];
            assert_eq!(marker.symbol(), "█", "marca del pico (y={y})");
            assert_eq!(marker.fg, theme.spectrum_high, "el pico va en spectrum_high");
        }
        assert_eq!(
            buffer[(7u16, 0u16)].symbol(),
            "█",
            "la barra sigue normal a la izquierda"
        );
        assert_eq!(
            buffer[(20u16, 0u16)].symbol(),
            " ",
            "a la derecha del pico no hay nada"
        );
    }

    #[test]
    fn paint_vu_peak_at_fractional_column_uses_full_glyph() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 5);
        // Pico en 0.26 → columna 8 con fracción > 0 (no cae en el borde
        // exacto de una celda): el medidor es horizontal, así que el glifo
        // vertical de 7/8 no aplica — queda el mismo glifo lleno que el
        // borde exacto (`band_full_glyph`).
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.0, 0.26, &theme, false);
        assert_eq!(
            buffer[(8u16, 0u16)].symbol(),
            "█",
            "pico a mitad de columna usa el glifo lleno (unicode)"
        );

        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.0, 0.26, &theme, true);
        assert_eq!(
            buffer[(8u16, 0u16)].symbol(),
            "@",
            "pico a mitad de columna usa el glifo lleno (ascii)"
        );
    }

    #[test]
    fn paint_vu_silence_shows_scale_only() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 5);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.0, 0.0, &theme, false);
        for y in 0..4u16 {
            for x in 0..32u16 {
                assert_eq!(
                    buffer[(x, y)].symbol(),
                    " ",
                    "silencio: barra vacía ({x},{y})"
                );
            }
        }
        assert_eq!(buffer[(0u16, 4u16)].symbol(), "┼", "tics unicode");
        assert_eq!(buffer[(1u16, 4u16)].symbol(), "─", "línea de escala unicode");
        assert_eq!(buffer[(1u16, 4u16)].fg, theme.dim);
    }

    #[test]
    fn paint_vu_clamps_out_of_range_without_panic() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 4);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 1.5, 2.0, &theme, false);
        for y in 0..3u16 {
            assert_eq!(buffer[(7u16, y)].symbol(), "█", "clip al ancho (y={y})");
        }
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, -0.5, -1.0, &theme, true);
        for y in 0..3u16 {
            for x in 0..8u16 {
                assert_eq!(
                    buffer[(x, y)].symbol(),
                    ".",
                    "negativo = vacío ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn paint_vu_tiny_heights_degrade_without_panic() {
        let theme = Theme::default();
        // 1 fila: una sola fila de barra, sin pico ni escala.
        let area = Rect::new(0, 0, 16, 1);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.5, 1.0, &theme, false);
        for x in 0..8u16 {
            assert_eq!(buffer[(x, 0u16)].symbol(), "█", "media barra (x={x})");
        }
        for x in 8..16u16 {
            assert_ne!(
                buffer[(x, 0u16)].fg,
                theme.spectrum_high,
                "sin marca de pico en 1 fila (x={x})"
            );
        }
        // 2 filas: igual, sin pánico.
        let area = Rect::new(0, 0, 16, 2);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 1.0, 1.0, &theme, false);
        for x in 0..16u16 {
            assert_eq!(buffer[(x, 0u16)].symbol(), "█", "barra arriba (x={x})");
        }
        // Área vacía: sin pánico.
        let area = Rect::new(0, 0, 0, 0);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 1.0, 1.0, &theme, false);
    }

    // Anillo pulsante rasterizado celda a celda.
    // Área de referencia 40x20 (pareja): centro flotante (20.0, 10.0),
    // max_r = min(20, 20) - 0.5 = 19.5, base_r = 0.45·max_r ≈ 8.775,
    // span ≈ 10.725. Ángulo medido desde el norte, simétrico izq/derecha
    // (a = 0 arriba, a = 1 abajo); los graves (banda 0) quedan abajo.
    // ------------------------------------------------------------------

    #[test]
    fn paint_circular_silence_shows_a_closed_dim_ring_hole_as_background() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &[0.0f32; NUM_BANDS], &theme, false);

        // El hueco (celdas junto al centro) queda como fondo: sin punto
        // central, a diferencia del painter de radios anterior.
        for (x, y) in [(19u16, 9u16), (20, 9), (19, 10), (20, 10)] {
            let cell = &buffer[(x, y)];
            assert_eq!(cell.symbol(), " ", "hueco es fondo ({x},{y})");
            assert_eq!(cell.fg, Color::Reset, "hueco sin color ({x},{y})");
        }

        // Sin energía, ninguna celda lleva un color de gradiente: todo lo
        // pintado (anillo base) queda en `dim`, nunca en `spectrum_color`.
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "█" {
                    assert_eq!(cell.fg, theme.dim, "silencio: solo tenue ({x},{y})");
                }
            }
        }

        // El anillo base es continuo alrededor de todo el círculo (ángulo
        // real de cada celda tenue, 0°=norte, 90°=este, 180°=sur): cada
        // muestra cada 5° tiene una celda tenue cerca — los 32 radios de
        // antes dejaban huecos de ~11° SIN NADA, no una celda de por medio.
        let (cx, cy) = (20.0f32, 10.0f32);
        let mut dim_degrees: Vec<f32> = Vec::new();
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "█" && cell.fg == theme.dim {
                    let dx = f32::from(x) + 0.5 - cx;
                    let dy = f32::from(y) + 0.5 - cy;
                    let theta = dx.atan2(-(dy * 2.0)).to_degrees();
                    dim_degrees.push(if theta < 0.0 { theta + 360.0 } else { theta });
                }
            }
        }
        assert!(!dim_degrees.is_empty(), "el anillo base no pintó ninguna celda");
        // Tolerancia 15°: a este radio, la grilla entera rasteriza el
        // círculo con algún hueco de 1 celda entre ángulos vecinos — muy
        // lejos de los ~11° COMPLETAMENTE vacíos que dejaban los 32 radios
        // discretos (ahí no había ni una celda tenue en medio).
        for deg in (0..360).step_by(5) {
            let target = deg as f32;
            let nearest = dim_degrees
                .iter()
                .map(|&d| {
                    let diff = (d - target).abs();
                    diff.min(360.0 - diff)
                })
                .fold(f32::INFINITY, f32::min);
            assert!(
                nearest <= 15.0,
                "hueco en el anillo base cerca de {deg}° (más cercana a {nearest}°)"
            );
        }
    }

    #[test]
    fn paint_circular_uniform_levels_are_approximately_circular_not_oval() {
        // Compensación del aspecto de celda (~1:2): con nivel uniforme el
        // alcance horizontal del relleno ≈ 2× el vertical (misma distancia
        // física), igual que antes pero ahora hasta `fill_r`, no `max_r`.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &[0.6f32; NUM_BANDS], &theme, false);
        let filled = |x: u16, y: u16| {
            let cell = &buffer[(x, y)];
            cell.symbol() == "█" && cell.fg != theme.dim
        };
        let (cx, cy) = (20u16, 10u16);
        let dx_left = (0..cx).filter(|&x| filled(x, cy)).map(|x| cx - x).max();
        let dx_right = ((cx + 1)..area.width)
            .filter(|&x| filled(x, cy))
            .map(|x| x - cx)
            .max();
        let dy_up = (0..cy).filter(|&y| filled(cx, y)).map(|y| cy - y).max();
        let dy_down = ((cy + 1)..area.height)
            .filter(|&y| filled(cx, y))
            .map(|y| y - cy)
            .max();
        let (dx_left, dx_right, dy_up, dy_down) = (
            dx_left.unwrap_or(0),
            dx_right.unwrap_or(0),
            dy_up.unwrap_or(0),
            dy_down.unwrap_or(0),
        );
        assert!(
            dx_left > 0 && dx_right > 0 && dy_up > 0 && dy_down > 0,
            "hay relleno en las 4 direcciones (izq={dx_left}, der={dx_right}, \
             arr={dy_up}, aba={dy_down})"
        );
        assert!(
            dx_left.abs_diff(dx_right) <= 1 && dy_up.abs_diff(dy_down) <= 1,
            "simetría aproximada (izq={dx_left}, der={dx_right}, \
             arr={dy_up}, aba={dy_down})"
        );
        let dx = dx_left.max(dx_right);
        let dy = dy_up.max(dy_down);
        assert!(
            dx >= 2 * dy - 2 && dx <= 2 * dy + 2,
            "proporción física ~1:1 en celdas ~1:2 (dx={dx}, dy={dy})"
        );
    }

    #[test]
    fn paint_circular_uniform_level_has_no_gaps_between_bands() {
        // Anti-araña: con nivel uniforme, CADA ángulo (muestreado cada 5°)
        // tiene al menos una celda con energía entre `base_r` y `max_r` —
        // sin huecos, a diferencia de los 32 radios discretos de antes.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &[0.6f32; NUM_BANDS], &theme, false);

        let (cx, cy) = (20.0f32, 10.0f32);
        let max_r = (f32::from(area.width) / 2.0).min(f32::from(area.height)) - 0.5;
        let base_r = 0.45 * max_r;

        for deg in (0..360).step_by(5) {
            let theta = (deg as f32).to_radians();
            let (sin, cos) = theta.sin_cos();
            let mut hit = false;
            let mut r = base_r;
            while r <= max_r {
                let fx = cx + r * sin - 0.5;
                let fy = cy - (r * cos) / 2.0 - 0.5;
                if fx >= 0.0 && fy >= 0.0 {
                    let (cell_x, cell_y) = (fx.round() as u16, fy.round() as u16);
                    if cell_x < area.width && cell_y < area.height {
                        let cell = &buffer[(cell_x, cell_y)];
                        if cell.symbol() == "█" && cell.fg != theme.dim {
                            hit = true;
                            break;
                        }
                    }
                }
                r += 0.25;
            }
            assert!(hit, "hueco en {deg}°: sin energía entre base_r y max_r");
        }
    }

    #[test]
    fn paint_circular_mirrors_left_and_right_columns() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut bands = [0.0f32; NUM_BANDS];
        for (i, level) in bands.iter_mut().enumerate() {
            *level = (i as f32 / (NUM_BANDS - 1) as f32).clamp(0.0, 1.0);
        }
        bands[3] = 0.9;
        bands[20] = 0.1; // valores bien asimétricos entre bandas vecinas.
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &bands, &theme, false);
        for x in 0..(area.width / 2) {
            let mirror = area.width - 1 - x;
            for y in 0..area.height {
                let cell = &buffer[(x, y)];
                let mirrored = &buffer[(mirror, y)];
                assert_eq!(
                    cell.symbol(),
                    mirrored.symbol(),
                    "columna {x} y su espejo {mirror} con mismo símbolo (y={y})"
                );
                assert_eq!(
                    cell.fg,
                    mirrored.fg,
                    "columna {x} y su espejo {mirror} con mismo color (y={y})"
                );
            }
        }
    }

    #[test]
    fn paint_circular_bass_bands_extend_the_ring_at_the_bottom() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 1.0;
        bands[1] = 1.0; // graves a tope; agudos (resto) en cero.
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &bands, &theme, false);

        let is_energy = |x: u16, y: u16| {
            let cell = &buffer[(x, y)];
            cell.symbol() == "█" && cell.fg != theme.dim
        };
        let x = 19u16;
        let north = (0u16..10).filter(|&y| is_energy(x, y)).count();
        let south = (10u16..20).filter(|&y| is_energy(x, y)).count();
        assert_eq!(north, 0, "agudos en cero: nada relleno arriba, obtenido {north}");
        assert!(south > 0, "graves a tope: relleno abajo, obtenido {south}");
    }

    #[test]
    fn paint_circular_radial_gradient_low_near_the_base_high_at_the_edge() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &[1.0f32; NUM_BANDS], &theme, false);

        let (cx, cy) = (20.0f32, 10.0f32);
        let max_r = (f32::from(area.width) / 2.0).min(f32::from(area.height)) - 0.5;
        let base_r = 0.45 * max_r;
        let span = max_r - base_r;
        let expected_t = |x: u16, y: u16| {
            let dx = f32::from(x) + 0.5 - cx;
            let dy = f32::from(y) + 0.5 - cy;
            let r = dx.hypot(dy * 2.0);
            ((r - base_r) / span).clamp(0.0, 1.0)
        };
        let color_at = |t: f32| {
            let (r, g, b) = spectrum_color(t, &theme);
            Color::Rgb(r, g, b)
        };

        let near = (20u16, 5u16); // r ≈ 9.01, t ≈ 0.02: junto a la base.
        let far = (20u16, 19u16); // r ≈ 19.01, t ≈ 0.95: junto al borde.
        let t_near = expected_t(near.0, near.1);
        let t_far = expected_t(far.0, far.1);
        assert!(t_near < 0.1, "celda elegida cerca de la base, t={t_near}");
        assert!(t_far > 0.9, "celda elegida cerca del borde, t={t_far}");
        assert_eq!(
            buffer[near].fg,
            color_at(t_near),
            "gradiente bajo junto a la base"
        );
        assert_eq!(
            buffer[far].fg,
            color_at(t_far),
            "gradiente alto junto al borde exterior"
        );
    }

    #[test]
    fn paint_circular_ascii_uses_full_glyph_and_dot_background() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 1.0;
        bands[1] = 1.0;
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &bands, &theme, true);
        let x = 19u16;
        let filled = (10u16..20).any(|y| {
            let cell = &buffer[(x, y)];
            cell.symbol() == "@" && cell.fg != theme.dim
        });
        assert!(filled, "ascii usa @ para las celdas con energía");
        let hole = &buffer[(20u16, 10u16)];
        assert_eq!(hole.symbol(), ".", "fondo de puntos en ascii dentro del hueco");
        assert_eq!(hole.fg, theme.dim);
    }

    #[test]
    fn paint_circular_tiny_and_out_of_range_degrade_without_panic() {
        let theme = Theme::default();
        // Área vacía: sin pánico.
        let area = Rect::new(0, 0, 0, 0);
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &[1.0f32; NUM_BANDS], &theme, false);

        // 1x1 y 3x2: max_r < 2 degrada a solo fondo, sin pánico.
        for (w, h) in [(1u16, 1u16), (3u16, 2u16)] {
            let area = Rect::new(0, 0, w, h);
            let mut buffer = Buffer::empty(area);
            paint_circular(&mut buffer, area, &[1.0f32; NUM_BANDS], &theme, false);
            for y in 0..h {
                for x in 0..w {
                    assert_eq!(
                        buffer[(x, y)].symbol(),
                        " ",
                        "{w}x{h}: solo fondo, sin energía ({x},{y})"
                    );
                }
            }
        }

        // Nivel > 1.0 clampea igual que 1.0 exacto: mismo resultado.
        let area = Rect::new(0, 0, 40, 20);
        let mut over = [0.0f32; NUM_BANDS];
        over[0] = 1.5;
        over[1] = 1.5;
        let mut exact = [0.0f32; NUM_BANDS];
        exact[0] = 1.0;
        exact[1] = 1.0;
        let mut buffer_over = Buffer::empty(area);
        paint_circular(&mut buffer_over, area, &over, &theme, false);
        let mut buffer_exact = Buffer::empty(area);
        paint_circular(&mut buffer_exact, area, &exact, &theme, false);
        assert_eq!(
            buffer_over, buffer_exact,
            "1.5 clampeado a 1.0 pinta igual que un nivel de 1.0"
        );

        // Nivel negativo clampea a 0: no agrega relleno.
        let mut negative = [0.0f32; NUM_BANDS];
        negative[0] = -0.5;
        negative[1] = -0.5;
        let mut buffer_negative = Buffer::empty(area);
        paint_circular(&mut buffer_negative, area, &negative, &theme, false);
        let is_energy = |b: &Buffer, x: u16, y: u16| {
            let cell = &b[(x, y)];
            cell.symbol() == "█" && cell.fg != theme.dim
        };
        assert!(
            !(10u16..20).any(|y| is_energy(&buffer_negative, 19, y)),
            "nivel negativo clampea a 0: nada relleno abajo"
        );
    }

    fn test_app() -> App {
        App {
            mode: Mode::Browser,
            explorer_open: false,
            roots: vec![PathBuf::from(".")],
            library: None,
            scan_rx: None,
            view: Vec::new(),
            selected: 0,
            filter: None,
            search: None,
            shuffle: false,
            repeat: RepeatMode::Off,
            shuffle_order: Vec::new(),
            queue: Vec::new(),
            queue_cursor: None,
            queue_resolving: None,
            queue_advance: QueueAdvance::NaturalEnd,
            queue_panel: false,
            queue_selected: 0,
            next_up: None,
            ui_frame: 0,
            current_path: None,
            has_player: false,
            track_label: String::new(),
            status: Status::Ended,
            error: None,
            should_quit: false,
            analyzer: SpectrumAnalyzer::new(),
            url_input: None,
            ytdlp_rx: None,
            playlist_rx: None,
            ytdlp_request_id: 0,
            is_url: false,
            current_url: None,
            ytdlp_failed: false,
            history: crate::history::History::default(),
            pending_history: None,
            history_path: Some(test_temp_history_path()),
            theme_menu: None,
            bands_menu: None,
            mode_menu: None,
            settings_panel: None,
            // Los tests existentes asumen captura habilitada (clics que
            // actúan, guards de modal que frenan lo demás): default
            // habilitado, igual que la config por defecto.
            mouse_enabled: true,
            quit_guard: QuitGuard::default(),
            hit: RefCell::new(HitMap::default()),
            drag: None,
        }
    }

    static TEST_HIST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn test_temp_history_path() -> PathBuf {
        let id = TEST_HIST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("mp3tui_test_hist_{}_{}.toml", std::process::id(), id))
    }

    fn track_named(name: &str) -> Track {
        Track {
            path: PathBuf::from(format!(r"C:\x\{name}.mp3")),
            title: name.to_string(),
            artist: None,
            album: None,
            duration: None,
            folder: "x".to_string(),
        }
    }

    #[test]
    fn poll_scan_waits_then_delivers_and_syncs_selection() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.scan_rx = Some(rx);
        app.current_path = Some(PathBuf::from(r"C:\x\beta.mp3"));

        // Todavia escaneando: no hay biblioteca, pero el receiver sigue vivo.
        poll_scan(&mut app);
        assert!(app.library.is_none(), "espera sin resultados");
        assert!(app.scan_rx.is_some());

        tx.send(crate::playlist::from_tracks_for_test(vec![
            track_named("alpha"),
            track_named("beta"),
        ]))
        .unwrap();
        poll_scan(&mut app);

        assert!(app.scan_rx.is_none(), "scan terminado");
        assert_eq!(app.library.as_ref().map(Playlist::len), Some(2));
        assert_eq!(app.selected, 1, "seleccion en la pista sonando");
    }

    #[test]
    fn poll_scan_disconnected_yields_empty_library_not_hang() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.scan_rx = Some(rx);
        drop(tx);

        poll_scan(&mut app);
        assert!(app.library.is_some(), "no queda en Escaneando eterno");
        assert!(app.library.unwrap().is_empty());
        assert!(app.scan_rx.is_none());
    }

    #[test]
    fn toggle_explorer_never_starts_a_second_scan() {
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        let (tx, rx) = mpsc::channel();
        app.scan_rx = Some(rx);

        // Segundo 'b' con un escaneo ya en curso: conserva el receiver original.
        toggle_explorer(&mut app);
        assert!(app.explorer_open, "primer 'b' abre el panel");
        assert!(app.scan_rx.is_some());

        toggle_explorer(&mut app);
        assert!(!app.explorer_open, "segundo 'b' cierra el panel");
        assert!(app.scan_rx.is_some(), "cerrar tampoco toca el escaneo");

        // Sin sesión de audio el explorador ya está a pantalla completa.
        let mut bare = test_app();
        toggle_explorer(&mut bare);
        assert!(!bare.explorer_open, "sin player no hay panel que abrir");

        // El escaneo original es el que entrega el resultado.
        tx.send(crate::playlist::from_tracks_for_test(vec![track_named("alpha")]))
            .unwrap();
        poll_scan(&mut app);
        assert_eq!(app.library.as_ref().map(Playlist::len), Some(1));
        assert!(app.scan_rx.is_none());
    }

    #[test]
    fn player_areas_shrinks_the_main_column_and_keeps_the_footer_full_width() {
        let area = Rect::new(0, 0, 100, 30);

        let (info, visual, footer, panel) = player_areas(area, false);
        assert_eq!(panel, None, "panel cerrado = layout clásico");
        assert_eq!((info.x, info.width), (0, 100));
        assert_eq!((visual.x, visual.width), (0, 100));
        assert_eq!(footer.height, 4, "pie clásico con 2 líneas");

        let (info, visual, footer, panel) = player_areas(area, true);
        let panel = panel.expect("panel abierto");
        assert_eq!((panel.x, panel.width), (64, 36), "36% derecho para la lista");
        assert_eq!(
            info.x + info.width,
            64,
            "la info se achica antes del panel"
        );
        assert_eq!(
            visual.x + visual.width,
            64,
            "el visualizador sigue visible (comprimido) a la izquierda"
        );
        assert_eq!(
            visual.y,
            info.y + info.height,
            "el visualizador arranca debajo de la info"
        );
        assert_eq!(footer.width, 100, "el pie sigue a todo lo ancho");
        assert_eq!(footer.height, 5, "pie con 3 líneas para los atajos del panel");
        assert_eq!(
            info.y + info.height + visual.height,
            footer.y,
            "sin solapes entre zonas"
        );
    }

    #[test]
    fn go_back_closes_the_panel_and_only_quits_from_the_bare_explorer() {
        // Panel abierto: Esc cierra el panel, no sale de la app (Esc
        // es la única tecla de «back»; Tab vive en «explorer»).
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        go_back(&mut app);
        assert!(!app.explorer_open, "back cierra el panel");
        assert!(app.mode == Mode::Player, "el player de fondo queda");
        assert!(!app.should_quit, "no sale de la app");

        // Explorador a pantalla completa sin player: sale, pero con doble Esc
        // (el primero arma la guarda, el segundo sale).
        let mut bare = test_app();
        go_back(&mut bare);
        assert!(!bare.should_quit, "un Esc solo arma la salida");
        go_back(&mut bare);
        assert!(bare.should_quit, "sin player, Esc Esc sale");

        // Player sin panel (estado clásico): back vuelve al modo player.
        let mut classic = test_app();
        classic.mode = Mode::Browser;
        classic.has_player = true;
        go_back(&mut classic);
        assert!(classic.mode == Mode::Player);
        assert!(!classic.should_quit);
    }

    #[test]
    fn browser_is_safe_while_first_scan_runs() {
        let mut app = test_app();
        let (_tx, rx) = mpsc::channel(); // tx vivo: escaneo "en curso"
        app.scan_rx = Some(rx);

        assert_eq!(app.view.len(), 0);
        assert!(view_get(&app, 0).is_none());
        app.selected = move_selection(app.selected, 5, app.view.len());
        assert_eq!(app.selected, 0, "la seleccion no sale del rango vacio");

        poll_scan(&mut app);
        assert!(app.library.is_none(), "el scan sigue corriendo");
        assert!(app.scan_rx.is_some());
    }

    #[test]
    fn row_line_survives_narrow_width_with_hour_long_duration() {
        let mut track = sample_track();
        track.duration = Some(Duration::from_secs(3725)); // 1:02:05
        for width in [3u16, 6, 10, 20, 40] {
            let line = row_line(&track, 1, false, false, width);
            assert_eq!(line_width(&line), width as usize, "width={width}");
        }
        // Con muy poco ancho la duración se descarta y el título manda.
        let line = row_line(&track, 1, false, false, 6);
        assert!(!line_text(&line).contains("1:02:05"));
    }

    fn library_with_three() -> App {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_named("alpha"),
            track_named("beta"),
            track_named("gamma"),
        ]));
        refresh_view(&mut app);
        app
    }

    #[test]
    fn refresh_view_filters_and_keeps_the_selected_track() {
        let mut app = library_with_three();
        app.selected = 1; // beta

        app.filter = Some("bet".to_string());
        refresh_view(&mut app);
        assert_eq!(app.view, vec![1], "filtro por prefijo del título");
        assert_eq!(app.selected, 0, "la seleccionada sigue visible");

        app.filter = Some("no-existe".to_string());
        refresh_view(&mut app);
        assert!(app.view.is_empty(), "sin coincidencias la lista queda vacia");
        assert_eq!(app.selected, 0, "seleccion acotada al rango vacio");

        app.filter = None;
        refresh_view(&mut app);
        assert_eq!(app.view, vec![0, 1, 2], "sin filtro vuelve la lista completa");
    }

    #[test]
    fn refresh_view_syncs_selection_to_playing_track_on_first_scan() {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_named("alpha"),
            track_named("beta"),
        ]));
        app.current_path = Some(PathBuf::from(r"C:\x\beta.mp3"));
        refresh_view(&mut app);
        assert_eq!(app.selected, 1, "seleccion en la pista sonando");
    }

    #[test]
    fn search_mode_types_into_query_enter_applies_and_esc_clears() {
        let mut app = library_with_three();

        open_search(&mut app);
        assert!(app.search.is_some(), "'/' abre la búsqueda");

        handle_search_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &mut app);
        assert_eq!(app.search.as_deref(), Some("g"));
        assert_eq!(app.view, vec![2], "filtrado en vivo");

        handle_search_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut app,
        );
        assert_eq!(app.view, vec![0, 1, 2], "backspace amplía de vuelta");

        handle_search_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &mut app);
        handle_search_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE), &mut app);
        handle_search_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), &mut app);
        handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut app);
        assert!(app.search.is_none(), "Enter cierra el modo búsqueda");
        assert_eq!(app.filter.as_deref(), Some("bet"), "Enter confirma el filtro");
        assert_eq!(app.view, vec![1]);

        open_search(&mut app);
        assert_eq!(app.search.as_deref(), Some("bet"), "reabre con el filtro actual");
        handle_search_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut app);
        assert!(app.search.is_none());
        assert!(app.filter.is_none(), "Esc cancela y limpia el filtro");
        assert_eq!(app.view, vec![0, 1, 2], "lista completa restaurada");
    }

    #[test]
    fn shuffle_order_is_regenerated_when_view_changes() {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_named("alpha"),
            track_named("beta"),
            track_named("gamma"),
            track_named("delta"),
        ]));
        refresh_view(&mut app);

        assert!(!app.shuffle, "arranca sin shuffle");
        toggle_shuffle(&mut app);
        assert!(app.shuffle, "'s' activa el shuffle");
        assert_eq!(app.shuffle_order.len(), 4, "orden mezclado de la lista visible");

        app.filter = Some("ta".to_string()); // "ta" matchea beta y delta
        refresh_view(&mut app);
        assert_eq!(app.view, vec![1, 3]);
        assert_eq!(app.shuffle_order.len(), 2, "orden re-armado al cambiar el filtro");
        let mut sorted = app.shuffle_order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1], "sigue siendo una permutación");
    }

    /// D2/S6: con shuffle activo el orden mezclado cubre el **pool mixto** que
    /// recorren `n`/`p` y el fin de pista (biblioteca + historial de URLs), no
    /// solo las pistas locales: si un refresco de la vista lo dejara corto, el
    /// orden quedaría a medio armar hasta el próximo salto.
    #[test]
    fn refresh_view_shuffle_order_covers_the_mixed_pool() {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_named("alpha"),
            track_named("beta"),
        ]));
        app.history.add(
            "Video de YouTube".to_string(),
            "https://www.youtube.com/watch?v=x".to_string(),
        );
        app.shuffle = true;
        refresh_view(&mut app);
        let after_refresh = app.shuffle_order.len();

        let pool = pool_positions_in_view(&mut app);
        assert_eq!(pool.len(), 3, "el pool mixto suma la URL del historial");
        assert_eq!(
            after_refresh,
            pool.len(),
            "el orden mezclado ya cubre el pool mixto al refrescar la vista"
        );
        let mut sorted = app.shuffle_order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2], "sigue siendo una permutación");
    }

    #[test]
    fn footer_shows_the_new_shortcuts_in_each_mode() {
        let mut app = library_with_three();
        app.mode = Mode::Player;
        let player: String = footer_lines(&app, false)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        // Pie del player reequilibrado en 2 líneas — restaura
        // `Tab: biblio`; `↑/↓: vol` abrevia para que entre en 78 cols.
        for hint in ["Tab: biblio", "b: buscar", "espacio: pausa", "↑/↓: vol", "←/→: ±5s", "n/p: pista", "s: shuffle", "r: repeat", "[/]: vel", "u: URL", "Ctrl+P: config", "Esc×2: salir"] {
            assert!(player.contains(hint), "footer del player falta '{hint}': {player}");
        }

        app.mode = Mode::Browser;
        let browser: String = footer_lines(&app, false)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        // `sig/ant` expandido (entra en 78 reagrupando), `Inicio/Fin`
        // documentado y fuera el `Esc: volver` inexacto (sin player Esc arma
        // la salida, cubierto por `Esc×2: salir`).
        for hint in ["PgUp/PgDn: mover", "Enter: reproducir", "n/p: pista siguiente/anterior", "espacio: pausa", "←/→: ±5s", "[/]: vel", "s: shuffle", "r: repeat", "b: buscar", "Inicio/Fin: primera/última", "u: URL", "Ctrl+P: config", "Esc×2: salir"] {
            assert!(browser.contains(hint), "footer del explorador falta '{hint}': {browser}");
        }
        assert!(
            !browser.contains("Esc: volver"),
            "sin `Esc: volver` inexacto: {browser}"
        );

        // Panel lateral abierto: lista + playback + aviso de volumen.
        app.mode = Mode::Player;
        app.explorer_open = true;
        let panel: String = footer_lines(&app, false)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        for hint in [
            "↑/↓: mover",
            "Enter: reproducir",
            "Esc/Tab: cerrar",
            "espacio: pausa",
            "←/→: ±5s",
            "[/]: vel",
            "n/p: pista siguiente/anterior",
            "u: URL",
            "Esc×2: salir",
            "volumen ↑/↓: con el panel cerrado",
        ] {
            assert!(panel.contains(hint), "footer del panel falta '{hint}': {panel}");
        }
        assert_eq!(
            footer_lines(&app, false).len(),
            3,
            "el pie con panel usa sus 3 líneas"
        );
        app.explorer_open = false;

        // Pie del panel de configuración (`abrir/alternar`: en «Mouse»
        // Enter alterna, no abre sub-modal).
        open_settings_panel(&mut app);
        let settings: String = footer_lines(&app, false)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        for hint in ["↑/↓: mover", "Enter: abrir/alternar", "Esc: cerrar"] {
            assert!(settings.contains(hint), "pie del panel falta '{hint}': {settings}");
        }
        app.settings_panel = None;

        // Ninguna línea del pie puede exceder el ancho útil (78): si no, se
        // trunca en pantalla.
        for (mode, open) in [
            (Mode::Player, false),
            (Mode::Player, true),
            (Mode::Browser, false),
        ] {
            app.mode = mode;
            app.explorer_open = open;
            for line in footer_lines(&app, false) {
                assert!(
                    line_width(&line) <= 78,
                    "línea del pie demasiado ancha ({}): {}",
                    line_width(&line),
                    line_text(&line)
                );
            }
        }
        app.explorer_open = false;

        app.search = Some(String::new());
        let search: String = footer_lines(&app, false)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(search.contains("Backspace"));
        assert!(search.contains("Esc: cancelar"));
    }

    fn track_in(name: &str, folder: &str) -> Track {
        Track {
            path: PathBuf::from(format!(r"C:\lib\{folder}\{name}.mp3")),
            title: name.to_string(),
            artist: None,
            album: None,
            duration: None,
            folder: folder.to_string(),
        }
    }

    fn library_two_folders() -> App {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_in("alpha", "rock"),
            track_in("beta", "rock"),
            track_in("gamma", "jazz"),
            track_in("delta", "jazz"),
        ]));
        refresh_view(&mut app);
        app
    }

    #[test]
    fn library_rows_interleave_headers_and_keep_the_cursor_visible() {
        let mut app = library_two_folders();
        let lines = library_rows(&app, 40, 10);
        let texts: Vec<String> = lines.iter().map(|line| line_text(line)).collect();
        assert_eq!(texts.len(), 6, "2 encabezados + 4 pistas: {texts:?}");
        assert!(texts[0].contains("rock"), "encabezado de grupo: {:?}", texts[0]);
        assert!(
            texts[0].contains('\u{2500}'),
            "relleno de guiones: {:?}",
            texts[0]
        );
        assert_eq!(line_width(&lines[0]), 40, "el encabezado ocupa el ancho");
        assert!(!texts[0].contains('\u{258c}'), "el encabezado no selecciona");
        assert!(texts[1].contains("alpha"), "{:?}", texts[1]);
        assert!(texts[3].contains("jazz"), "segundo grupo: {:?}", texts[3]);

        // Selección al final con alto chico: el scroll cuenta los
        // encabezados y el cursor queda en la última fila visible.
        app.selected = 3;
        let lines = library_rows(&app, 40, 3);
        let texts: Vec<String> = lines.iter().map(|line| line_text(line)).collect();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(
            texts[2].starts_with('\u{258c}'),
            "cursor a la vista: {texts:?}"
        );
        assert!(texts[2].contains("delta"), "{:?}", texts[2]);
    }

    #[test]
    fn single_folder_library_renders_without_headers() {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_in("alpha", "rock"),
            track_in("beta", "rock"),
        ]));
        refresh_view(&mut app);

        let lines = library_rows(&app, 40, 10);
        let texts: Vec<String> = lines.iter().map(|line| line_text(line)).collect();
        assert_eq!(texts.len(), 2, "una sola carpeta no muestra encabezados");
        assert!(texts[0].contains("1. "), "fila normal: {:?}", texts[0]);
        assert!(
            texts.iter().all(|text| !text.contains('\u{2500}')),
            "sin guiones de encabezado"
        );
    }

    #[test]
    fn folder_label_shows_the_single_root_or_the_root_count() {
        let mut app = test_app();
        // Ruta armada con componentes para que el separador sea el del sistema:
        // `folder_label` depende de `Path::file_name`, así que un literal con
        // backslash solo es una ruta con nombre propio en Windows.
        app.roots = vec![PathBuf::from("musica").join("rock")];
        assert_eq!(library_folder_label(&app), "rock");

        app.roots = vec![PathBuf::from("a"), PathBuf::from("b")];
        assert_eq!(library_folder_label(&app), "2 carpetas");
    }

    /// La forma con backslash solo es una ruta con carpeta propia en Windows;
    /// se conserva la expectativa original en la plataforma donde aplica.
    #[cfg(windows)]
    #[test]
    fn folder_label_shows_the_last_component_of_a_windows_path() {
        let mut app = test_app();
        app.roots = vec![PathBuf::from(r"C:\musica\rock")];
        assert_eq!(library_folder_label(&app), "rock");
    }

    #[test]
    fn speed_label_shows_two_decimals_with_x() {
        assert_eq!(speed_label(1.25), "1.25x");
        assert_eq!(speed_label(0.75), "0.75x");
        assert_eq!(speed_label(2.0), "2.00x");
    }

    #[test]
    fn repeat_and_shuffle_show_up_in_the_status_line_only_when_active() {
        let plain = status_line(Status::Playing, RepeatMode::Off, false);
        let plain_text = line_text(&plain);
        assert!(plain_text.contains("Reproduciendo"));
        assert!(!plain_text.contains("repeat"));
        assert!(!plain_text.contains("shuffle"));

        let active = status_line(Status::Playing, RepeatMode::All, true);
        let active_text = line_text(&active);
        assert!(active_text.contains("repeat: todo"), "{active_text}");
        assert!(active_text.contains("shuffle: sí"), "{active_text}");
    }

    #[test]
    fn playback_failure_shows_detenido_not_fin_de_la_pista() {
        let failed = status_line(Status::Stopped, RepeatMode::Off, false);
        let failed_text = line_text(&failed);
        assert!(failed_text.contains("Detenido"), "{failed_text}");
        assert!(
            !failed_text.contains("Fin de pista"),
            "stopped no debe mostrar fin de pista: {failed_text}"
        );

        let ended = status_line(Status::Ended, RepeatMode::Off, false);
        let ended_text = line_text(&ended);
        assert!(ended_text.contains("Fin de pista"), "{ended_text}");
        assert!(!ended_text.contains("Detenido"), "{ended_text}");
    }

    #[test]
    fn test_url_display_label() {
        assert_eq!(
            url_display_label("https://example.com/audio/song.mp3"),
            "song.mp3"
        );
        assert_eq!(
            url_display_label("http://audio.example.org/path/stream.m4a?token=1#part"),
            "stream.m4a"
        );
        assert_eq!(
            url_display_label("https://radio.example.com/"),
            "radio.example.com"
        );
        assert_eq!(
            url_display_label("http://localhost:8080"),
            "localhost:8080"
        );
    }

    #[test]
    fn test_classify_submit_input() {
        assert_eq!(classify_submit_input(""), SubmitAction::Empty);
        assert_eq!(classify_submit_input("   "), SubmitAction::Empty);
        assert_eq!(
            classify_submit_input("https://example.com/audio.mp3"),
            SubmitAction::DirectAudio("https://example.com/audio.mp3".to_string())
        );
        assert_eq!(
            classify_submit_input("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            SubmitAction::Page("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            classify_submit_input("https://example.com/unknown_stream"),
            SubmitAction::Unknown("https://example.com/unknown_stream".to_string())
        );
    }

    #[test]
    fn test_prompt_editing_and_paste() {
        let mut prompt = String::new();
        handle_prompt_char(&mut prompt, 'h');
        handle_prompt_char(&mut prompt, 't');
        handle_prompt_char(&mut prompt, 't');
        handle_prompt_char(&mut prompt, 'p');
        assert_eq!(prompt, "http");

        handle_prompt_backspace(&mut prompt);
        assert_eq!(prompt, "htt");

        handle_prompt_paste(&mut prompt, "p://example.com/song.mp3\r\n");
        assert_eq!(prompt, "http://example.com/song.mp3");
    }

    #[test]
    fn test_status_text_transitions() {
        let (resolving_text, _) = status_parts(Status::Resolving);
        assert!(
            resolving_text.contains("Resolviendo con yt-dlp…"),
            "texto: {resolving_text}"
        );

        let (connecting_text, _) = status_parts(Status::Connecting);
        assert!(
            connecting_text.contains("Conectando…"),
            "texto: {connecting_text}"
        );
    }

    #[test]
    fn test_footer_shows_open_url_key() {
        let app = test_app();
        let lines = footer_lines(&app, false);
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join(" ");
        assert!(
            joined.contains("u: URL") || joined.contains("u: url") || joined.contains("URL"),
            "footer debe mencionar atajo de URL: {joined}"
        );
    }

    // -------------------------------------------------------------------------
    // Menú de temas
    // -------------------------------------------------------------------------

    static THEME_MENU_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ThemeMenuTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        name: String,
        theme: Theme,
    }

    impl Drop for ThemeMenuTestGuard {
        fn drop(&mut self) {
            runtime::activate_theme(&self.name, self.theme.clone());
        }
    }

    /// Serializa los tests que mutan el tema global y restaura el estado
    /// activo al salir (incluso si el test falla).
    fn theme_menu_test_setup() -> ThemeMenuTestGuard {
        let lock = THEME_MENU_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ThemeMenuTestGuard {
            _lock: lock,
            name: runtime::active_theme_name(),
            theme: runtime::active_theme(),
        }
    }

    fn theme_menu_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    fn sample_hit() -> HitMap {
        HitMap {
            progress: Some(BarHit { x: 20, y: 6, width: 51 }),
            volume: Some(BarHit { x: 20, y: 5, width: 11 }),
            player: Some(Rect::new(0, 0, 100, 20)),
            buttons: Vec::new(),
            chips: Vec::new(),
        }
    }

    #[test]
    fn bar_geometry_matches_build_bar_layout() {
        // "Progreso " (9) + " [" (2) => la barra empieza en la columna 11.
        let (offset, width) = bar_geometry("Progreso ", "1:00 / 3:00", 80).expect("entra");
        assert_eq!(offset, 11);
        assert_eq!(width as usize, 80 - (9 + 2 + 2 + "1:00 / 3:00".len()));
        assert!(bar_geometry("Progreso ", "1:00 / 3:00", 10).is_none(), "sin espacio");
    }

    #[test]
    fn bar_hit_ratio_and_contains() {
        let bar = BarHit { x: 20, y: 6, width: 51 };
        assert!(bar.contains(20, 6) && bar.contains(70, 6));
        assert!(!bar.contains(19, 6) && !bar.contains(71, 6) && !bar.contains(30, 7));
        assert_eq!(bar.ratio(20), 0.0);
        assert_eq!(bar.ratio(70), 1.0);
        assert!((bar.ratio(45) - 0.5).abs() < 1e-9);
        assert_eq!(bar.ratio(5), 0.0, "a la izquierda se recorta a 0");
        assert_eq!(bar.ratio(200), 1.0, "a la derecha se recorta a 1");
    }

    #[test]
    fn click_on_progress_seeks_and_starts_drag() {
        let (action, drag) =
            mouse_action(&sample_hit(), None, mouse(MouseEventKind::Down(MouseButton::Left), 45, 6));
        assert_eq!(drag, Some(Drag::Progress(None)));
        match action {
            MouseAction::SeekRatio(r) => assert!((r - 0.5).abs() < 1e-9),
            other => panic!("esperaba seek: {other:?}"),
        }
    }

    #[test]
    fn dragging_progress_only_seeks_on_release() {
        // Arrastrar no dispara un seek por cada movimiento (con URLs serían
        // decenas de pedidos): marca la posición y el seek llega al soltar.
        let (action, drag) = mouse_action(
            &sample_hit(),
            Some(Drag::Progress(None)),
            mouse(MouseEventKind::Drag(MouseButton::Left), 70, 12),
        );
        assert_eq!(action, MouseAction::None, "sin seek durante el arrastre");
        assert_eq!(drag, Some(Drag::Progress(Some(1.0))), "sigue la barra fuera de su fila");

        let (action, drag) = mouse_action(
            &sample_hit(),
            drag,
            mouse(MouseEventKind::Up(MouseButton::Left), 70, 12),
        );
        assert_eq!((action, drag), (MouseAction::SeekRatio(1.0), None), "soltar hace el seek");

        let (action, _) = mouse_action(
            &sample_hit(),
            Some(Drag::Progress(None)),
            mouse(MouseEventKind::Up(MouseButton::Left), 70, 12),
        );
        assert_eq!(action, MouseAction::None, "clic sin arrastre no repite el seek");
    }

    #[test]
    fn mouse_is_ignored_while_a_modal_is_open() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        *app.hit.borrow_mut() = sample_hit();
        app.url_input = Some(String::new());
        handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 30, 5), &mut app, &mut engine);
        assert_eq!(engine.volume(), 0.0, "el clic no atraviesa el prompt de URL");
        assert_eq!(app.drag, None);
    }

    #[test]
    fn click_on_volume_sets_volume() {
        let (action, drag) =
            mouse_action(&sample_hit(), None, mouse(MouseEventKind::Down(MouseButton::Left), 30, 5));
        assert_eq!(drag, Some(Drag::Volume));
        assert_eq!(action, MouseAction::SetVolume(1.0));
    }

    #[test]
    fn wheel_over_player_steps_volume_and_elsewhere_does_nothing() {
        let hit = sample_hit();
        let (up, _) = mouse_action(&hit, None, mouse(MouseEventKind::ScrollUp, 10, 10));
        let (down, _) = mouse_action(&hit, None, mouse(MouseEventKind::ScrollDown, 10, 10));
        assert_eq!(up, MouseAction::VolumeStep(1));
        assert_eq!(down, MouseAction::VolumeStep(-1));
        let (outside, _) = mouse_action(&hit, None, mouse(MouseEventKind::ScrollUp, 150, 40));
        assert_eq!(outside, MouseAction::None);
        let (miss, drag) =
            mouse_action(&hit, None, mouse(MouseEventKind::Down(MouseButton::Left), 2, 2));
        assert_eq!((miss, drag), (MouseAction::None, None), "clic fuera de las barras");
    }

    fn render_player_frame(app: &App, engine: &AudioEngine) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
        terminal
            .draw(|frame| render(frame, app, engine))
            .expect("dibuja");
        terminal.backend().buffer().clone()
    }

    /// Texto del frame de prueba: todas las filas del buffer concatenadas.
    fn player_screen_text(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .flat_map(|y| {
                (0..buffer.area.width)
                    .map(move |x| buffer[(x, y)].symbol().to_string())
                    .chain(std::iter::once("\n".to_string()))
            })
            .collect()
    }

    /// La cola es visible mientras se reproduce: una línea «Siguiente: <label>»
    /// aparece solo cuando hay ítems encolados, con el título del próximo.
    #[test]
    fn player_info_shows_the_next_queued_item_while_playing() {
        let Some(engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.current_path = Some(PathBuf::from(r"C:\x\alpha.mp3"));
        app.track_label = "alpha.mp3".to_string();
        // Dos ítems encolados: el próximo es «beta» y queda 1 pendiente.
        app.queue.push(QueueItem::Local(track_named("beta")));
        app.queue.push(QueueItem::Local(track_named("gamma")));

        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            text.contains("Siguiente: beta (+1)"),
            "la línea con lo siguiente es visible: {text}"
        );
    }

    /// Con la cola vacía el panel se ve igual que hoy: sin fila «Siguiente» ni
    /// ningún placeholder de «0».
    #[test]
    fn player_info_hides_the_queue_line_when_queue_is_empty() {
        let Some(engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.current_path = Some(PathBuf::from(r"C:\x\alpha.mp3"));
        app.track_label = "alpha.mp3".to_string();

        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            !text.contains("Siguiente"),
            "sin cola no hay fila de próxima: {text}"
        );
    }

    /// S9: con el cursor parado en el primer ítem, lo «siguiente» es el segundo
    /// de la cola (y no queda nada detrás, así que no hay «(+N)»).
    #[test]
    fn player_info_shows_the_item_after_the_queue_cursor() {
        let Some(engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.current_path = Some(PathBuf::from(r"C:\x\alpha.mp3"));
        app.track_label = "alpha.mp3".to_string();
        app.queue.push(QueueItem::Local(track_named("beta")));
        app.queue.push(QueueItem::Local(track_named("gamma")));
        app.queue_cursor = Some(0);

        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            text.contains("Siguiente: gamma"),
            "lo siguiente sale de la posición después del cursor: {text}"
        );
        assert!(
            !text.contains("(+1)"),
            "no queda nada detrás de gamma: {text}"
        );
    }

    /// S9: con el cursor en el último ítem no hay nada siguiente, así que la
    /// fila desaparece igual que con la cola vacía.
    #[test]
    fn player_info_hides_the_queue_line_when_the_cursor_is_at_the_last_item() {
        let Some(engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.current_path = Some(PathBuf::from(r"C:\x\alpha.mp3"));
        app.track_label = "alpha.mp3".to_string();
        app.queue.push(QueueItem::Local(track_named("beta")));
        app.queue.push(QueueItem::Local(track_named("gamma")));
        app.queue_cursor = Some(1);

        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            !text.contains("Siguiente"),
            "sin nada después del cursor no hay fila de próxima: {text}"
        );
    }

    #[test]
    fn render_registers_the_volume_bar_where_it_is_drawn() {
        let Some(engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        let buffer = render_player_frame(&app, &engine);

        let bar = app.hit.borrow().volume.expect("barra de volumen registrada");
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, bar.y)].symbol().to_string())
            .collect();
        assert!(row.contains("Volumen"), "la fila registrada es la del volumen: {row}");
        assert_eq!(buffer[(bar.x - 1, bar.y)].symbol(), "[", "empieza justo después de «[»");
        assert!(app.hit.borrow().player.is_some(), "área del reproductor registrada");
    }

    #[test]
    fn clicking_the_end_of_the_volume_bar_sets_full_volume() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        render_player_frame(&app, &engine);
        let bar = app.hit.borrow().volume.expect("barra registrada");

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: bar.x + bar.width - 1,
            row: bar.y,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(click, &mut app, &mut engine);
        assert_eq!(engine.volume(), 1.0, "clic al final = 100%");
        engine.set_volume(0.0);
        assert_eq!(app.drag, Some(Drag::Volume));
    }

    fn row_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn transport_row_lists_buttons_in_order_with_play_state() {
        let (paused, buttons) = transport_row(false, false, RepeatMode::Off, 200);
        let kinds: Vec<Button> = buttons.iter().map(|(b, _, _)| *b).collect();
        assert_eq!(
            kinds,
            vec![Button::Prev, Button::PlayPause, Button::Next, Button::Shuffle, Button::Repeat]
        );
        assert!(row_text(&paused).contains('▶'), "pausado muestra play");
        let (playing, _) = transport_row(true, false, RepeatMode::Off, 200);
        assert!(row_text(&playing).contains("▮▮"), "sonando muestra pausa");
    }

    #[test]
    fn transport_row_positions_match_the_drawn_text() {
        let (line, buttons) = transport_row(true, true, RepeatMode::All, 200);
        let text: Vec<char> = row_text(&line).chars().collect();
        let mut previous_end = 0;
        for (button, start, width) in &buttons {
            assert!(*width > 0, "{button:?} tiene ancho");
            assert!(*start >= previous_end, "{button:?} no se solapa");
            let label: String = text[*start as usize..(*start + *width) as usize].iter().collect();
            assert!(!label.trim().is_empty(), "{button:?} cubre texto visible: {label:?}");
            previous_end = start + width;
        }
        let row = row_text(&line);
        assert!(row.contains("aleatorio") && row.contains("repetir: todo"), "{row}");
    }

    #[test]
    fn active_toggles_are_highlighted() {
        let style_of = |line: &Line<'_>, needle: &str| {
            line.spans
                .iter()
                .find(|span| span.content.contains(needle))
                .and_then(|span| span.style.fg)
        };
        let (off, _) = transport_row(false, false, RepeatMode::Off, 200);
        let (on, _) = transport_row(false, true, RepeatMode::One, 200);
        assert_eq!(style_of(&on, "aleatorio"), Some(theme().accent));
        assert_ne!(style_of(&off, "aleatorio"), Some(theme().accent));
        assert_eq!(style_of(&on, "repetir"), Some(theme().accent));
    }

    #[test]
    fn click_on_a_button_presses_it() {
        let mut hit = sample_hit();
        hit.buttons = vec![(Rect::new(10, 8, 4, 1), Button::Next)];
        let (action, drag) =
            mouse_action(&hit, None, mouse(MouseEventKind::Down(MouseButton::Left), 12, 8));
        assert_eq!((action, drag), (MouseAction::Press(Button::Next), None));
    }

    #[test]
    fn clicking_the_shuffle_button_toggles_shuffle() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        render_player_frame(&app, &engine);
        let shuffle = app
            .hit
            .borrow()
            .buttons
            .iter()
            .find(|(_, b)| *b == Button::Shuffle)
            .map(|(rect, _)| *rect)
            .expect("botón aleatorio registrado");
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), shuffle.x, shuffle.y),
            &mut app,
            &mut engine,
        );
        assert!(app.shuffle, "el clic activa el aleatorio");
    }

    #[test]
    fn footer_chips_keep_the_text_and_highlight_keys() {
        let text = " s: shuffle   ←/→: ±5s   volumen ↑/↓: con el panel cerrado";
        let (spans, entries) = footer_chip_spans(text);
        let joined: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(joined, text, "el texto no cambia");
        let keycap = spans
            .iter()
            .find(|span| span.content == "s")
            .expect("la tecla es un span propio");
        assert_eq!(keycap.style.bg, Some(theme().select_bg), "keycap con fondo");
        let keys: Vec<&str> = entries.iter().map(|(_, _, k)| k.as_str()).collect();
        assert_eq!(keys, vec!["s", "←/→", "volumen ↑/↓"]);
        let (start, width, _) = &entries[0];
        assert_eq!((*start, *width), (1, "s: shuffle".chars().count() as u16));
    }

    #[test]
    fn chip_keys_resolve_to_configured_shortcuts() {
        assert_eq!(key_for_chip("s"), Some(KeyCode::Char('s')));
        assert_eq!(key_for_chip("←/→"), Some(KeyCode::Left));
        assert_eq!(key_for_chip("Esc×2"), Some(KeyCode::Esc));
        assert_eq!(key_for_chip("espacio"), Some(KeyCode::Char(' ')));
        assert_eq!(key_for_chip("volumen ↑/↓"), None, "texto suelto, no es un atajo");
    }

    #[test]
    fn new_queue_chips_resolve_to_their_shortcuts() {
        assert_eq!(key_for_chip("a"), Some(KeyCode::Char('a')), "encolar");
        assert_eq!(key_for_chip("c"), Some(KeyCode::Char('c')), "Biblioteca/Cola");
        assert_eq!(key_for_chip("d"), Some(KeyCode::Char('d')), "borrar de la cola");
    }

    #[test]
    fn click_on_a_footer_chip_sends_its_key() {
        let mut hit = sample_hit();
        hit.chips = vec![(Rect::new(40, 25, 10, 1), KeyCode::Char('s'))];
        let (action, drag) =
            mouse_action(&hit, None, mouse(MouseEventKind::Down(MouseButton::Left), 42, 25));
        assert_eq!((action, drag), (MouseAction::Key(KeyCode::Char('s')), None));
    }

    #[test]
    fn clicking_the_shuffle_chip_in_the_footer_toggles_shuffle() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        let buffer = render_player_frame(&app, &engine);
        let chip = app
            .hit
            .borrow()
            .chips
            .iter()
            .find(|(_, key)| *key == KeyCode::Char('s'))
            .map(|(rect, _)| *rect)
            .expect("chip «s: shuffle» registrado");
        let row: String = (chip.x..chip.x + chip.width)
            .map(|x| buffer[(x, chip.y)].symbol().to_string())
            .collect();
        assert!(row.starts_with("s: shuffle"), "el área es la del chip dibujado: {row:?}");
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), chip.x, chip.y),
            &mut app,
            &mut engine,
        );
        assert!(app.shuffle);
    }

    #[test]
    fn registered_buttons_are_actually_drawn_on_small_terminals() {
        let Some(engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        for height in [8u16, 10, 12, 14] {
            let backend = ratatui::backend::TestBackend::new(100, height);
            let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
            terminal.draw(|frame| render(frame, &app, &engine)).expect("dibuja");
            let buffer = terminal.backend().buffer().clone();
            for (rect, button) in app.hit.borrow().buttons.iter() {
                assert!(rect.y < height, "{height}: {button:?} fuera del frame");
                let cell = buffer[(rect.x + 1, rect.y)].symbol().to_string();
                assert!(!cell.trim().is_empty(), "{height}: {button:?} registrado pero no dibujado");
            }
        }
    }

    fn find_search_box(line: &Line<'_>) -> Option<Span<'static>> {
        line.spans
            .iter()
            .find(|span| span.style.bg == Some(theme().select_bg))
            .map(|span| Span::styled(span.content.to_string(), span.style))
    }

    #[test]
    fn search_title_shows_a_fixed_width_box_with_theme_background() {
        let mut app = library_with_three();
        app.search = Some("ab".to_string());
        let line = library_title_line(&app, "Music", theme().warn);
        let text = line_text(&line);
        assert!(text.contains("Buscar:"), "{text}");
        assert!(text.contains("coincidencias"), "{text}");

        let boxed = find_search_box(&line).expect("la caja usa el fondo select_bg");
        assert_eq!(boxed.content.chars().count(), SEARCH_BOX_WIDTH, "ancho fijo");
        assert!(
            boxed.content.starts_with(&format!("ab{SEARCH_CARET}")),
            "{:?}",
            boxed.content
        );
    }

    #[test]
    fn search_box_keeps_the_end_of_long_queries_visible() {
        let mut app = library_with_three();
        let long = "abcdefghijklmnopqrstuvwxyz0123456789";
        app.search = Some(long.to_string());
        let line = library_title_line(&app, "Music", theme().warn);
        let boxed = find_search_box(&line).expect("caja");
        assert_eq!(boxed.content.chars().count(), SEARCH_BOX_WIDTH);
        assert!(
            boxed.content.ends_with(&format!("789{SEARCH_CARET}")),
            "{:?}",
            boxed.content
        );
    }

    #[test]
    fn empty_search_box_shows_dim_placeholder() {
        let mut app = library_with_three();
        app.search = Some(String::new());
        let line = library_title_line(&app, "Music", theme().warn);
        let boxed = find_search_box(&line).expect("caja");
        assert!(boxed.content.contains(SEARCH_PLACEHOLDER), "{:?}", boxed.content);
        assert_eq!(boxed.style.fg, Some(theme().dim), "placeholder tenue");
        assert_eq!(boxed.content.chars().count(), SEARCH_BOX_WIDTH);
    }

    #[test]
    fn search_title_keeps_mode_indicators_and_plain_title_has_no_box() {
        let mut app = library_with_three();
        app.shuffle = true;
        app.search = Some("x".to_string());
        let searching = line_text(&library_title_line(&app, "Music", theme().warn));
        assert!(searching.contains("shuffle"), "{searching}");

        app.search = None;
        let plain = library_title_line(&app, "Music", theme().accent);
        assert!(find_search_box(&plain).is_none(), "sin búsqueda no hay caja");
        assert_eq!(line_text(&plain), library_title(&app, "Music"));
    }

    #[test]
    fn quit_guard_needs_two_presses_within_the_window() {
        let t0 = Instant::now();
        let mut guard = QuitGuard::default();
        assert!(!guard.is_armed(t0), "arranca desarmado");

        assert_eq!(guard.on_quit_key(t0), QuitDecision::Arm, "primer Esc arma");
        assert!(guard.is_armed(t0 + Duration::from_millis(500)));

        assert_eq!(
            guard.on_quit_key(t0 + Duration::from_millis(800)),
            QuitDecision::Quit,
            "segundo Esc dentro de la ventana sale"
        );
    }

    #[test]
    fn quit_guard_expires_and_rearms() {
        let t0 = Instant::now();
        let mut guard = QuitGuard::default();
        guard.on_quit_key(t0);
        let late = t0 + QUIT_CONFIRM_WINDOW + Duration::from_millis(1);
        assert!(!guard.is_armed(late), "vencida la ventana ya no está armado");
        assert_eq!(
            guard.on_quit_key(late),
            QuitDecision::Arm,
            "un Esc tarde vuelve a armar, no sale"
        );
    }

    #[test]
    fn quit_guard_other_key_disarms() {
        let t0 = Instant::now();
        let mut guard = QuitGuard::default();
        guard.on_quit_key(t0);
        guard.disarm();
        assert!(!guard.is_armed(t0));
        assert_eq!(guard.on_quit_key(t0), QuitDecision::Arm);
    }

    fn engine_or_skip() -> Option<AudioEngine> {
        AudioEngine::new().ok()
    }

    #[test]
    fn esc_in_player_arms_then_second_esc_quits() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.explorer_open = false;

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(!app.should_quit, "un solo Esc no sale");
        assert!(app.quit_guard.is_armed(Instant::now()), "queda armado con aviso");
        assert!(
            line_text(&error_line(&app)).contains("Esc de nuevo"),
            "muestra el aviso: {}",
            line_text(&error_line(&app))
        );

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.should_quit, "el segundo Esc sale");
    }

    #[test]
    fn other_key_between_escs_cancels_the_exit() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.explorer_open = false;

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        handle_key(theme_menu_key(KeyCode::Char('s')), &mut app, &mut engine);
        assert!(!app.quit_guard.is_armed(Instant::now()), "otra tecla desarma");
        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(!app.should_quit, "tras desarmar, un Esc solo vuelve a armar");
    }

    #[test]
    fn q_no_longer_quits() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.explorer_open = false;
        handle_key(theme_menu_key(KeyCode::Char('q')), &mut app, &mut engine);
        handle_key(theme_menu_key(KeyCode::Char('q')), &mut app, &mut engine);
        assert!(!app.should_quit);
    }

    #[test]
    fn esc_with_explorer_panel_open_closes_it_without_arming() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(!app.explorer_open, "Esc cierra el panel");
        assert!(!app.quit_guard.is_armed(Instant::now()), "y no arma la salida");
        assert!(!app.should_quit);
    }

    #[test]
    fn esc_in_fullscreen_browser_without_player_needs_two_presses() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Browser;
        app.has_player = false;
        app.explorer_open = false;

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(!app.should_quit, "un Esc no sale ni sin reproductor");
        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.should_quit);
    }

    #[test]
    fn b_opens_search_and_tab_toggles_explorer_from_player() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = false;

        handle_key(theme_menu_key(KeyCode::Tab), &mut app, &mut engine);
        assert!(app.explorer_open, "Tab abre el explorador");
        handle_key(theme_menu_key(KeyCode::Tab), &mut app, &mut engine);
        assert!(!app.explorer_open, "Tab lo cierra");

        handle_key(theme_menu_key(KeyCode::Char('b')), &mut app, &mut engine);
        assert!(app.search.is_some(), "b abre la búsqueda");
    }

    #[test]
    fn theme_menu_open_snapshots_current_theme() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();

        open_theme_menu(&mut app);

        let menu = app
            .theme_menu
            .as_ref()
            .expect("open_theme_menu debe abrir el menú");
        let current = runtime::active_theme_name();
        let expected = config::builtin_themes()
            .iter()
            .position(|(id, _)| *id == current)
            .expect("el tema activo es un id builtin");
        assert_eq!(menu.selected, expected, "selección = tema actual");
        assert_eq!(menu.original_name, current, "guarda el id original");
        assert_eq!(
            menu.original_theme,
            runtime::active_theme(),
            "guarda los colores originales para Esc"
        );
        // Abrir no cambia el tema.
        assert_eq!(runtime::active_theme_name(), current, "solo previsualiza al mover");
    }

    #[test]
    fn theme_menu_renders_inside_tiny_terminals() {
        // Más angosta que 24 columnas o más baja que 3 filas: el modal no puede
        // salirse del frame (ratatui entra en pánico al escribir fuera del buffer).
        let menu = ThemeMenuState {
            selected: 0,
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
        };
        for (w, h) in [(10, 2), (23, 10), (40, 2), (1, 1), (0, 0)] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
            terminal
                .draw(|frame| render_theme_menu(frame, &menu))
                .unwrap_or_else(|e| panic!("{w}x{h}: {e}"));
        }
    }

    #[test]
    fn theme_menu_navigation_previews_live() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        app.theme_menu = Some(ThemeMenuState {
            selected: 0,
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        let menu = app
            .theme_menu
            .as_ref()
            .expect("el menú sigue abierto al navegar");
        assert_eq!(menu.selected, 1, "↓ mueve la selección");
        let second_id = config::builtin_themes()[1].0;
        assert_eq!(
            runtime::active_theme(),
            config::resolve_theme(second_id),
            "↓ previsualiza la paleta en vivo"
        );

        handle_theme_menu_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(
            app.theme_menu.as_ref().expect("menú abierto").selected,
            0,
            "↑ vuelve a la primera fila"
        );
        assert_eq!(
            runtime::active_theme(),
            config::resolve_theme(config::builtin_themes()[0].0),
            "↑ restaura la previsualización anterior"
        );
    }

    #[test]
    fn theme_menu_enter_applies_and_persists_injected_path() {
        let _setup = theme_menu_test_setup();
        let path = std::env::temp_dir()
            .join(format!("mp3tui_theme_menu_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut app = test_app();
        let last = config::builtin_themes().len() - 1;
        app.theme_menu = Some(ThemeMenuState {
            selected: last,
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |id| {
            config::persist_theme_name_at(&path, id)
        });

        let expected = config::builtin_themes()[last].0;
        assert!(app.theme_menu.is_none(), "Enter cierra el menú");
        assert_eq!(
            runtime::active_theme_name(),
            expected,
            "el id aplicado queda activo"
        );
        assert_eq!(
            runtime::active_theme(),
            config::resolve_theme(expected),
            "la paleta aplicada queda activa"
        );
        assert!(app.error.is_none(), "sin error: {:?}", app.error);
        let text = std::fs::read_to_string(&path).expect("archivo persistido");
        assert_eq!(
            text,
            format!("[theme]\nname = \"{expected}\"\n"),
            "config.toml temporal con el name nuevo"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// D1: aplicar el tema Miku desde el menú activa en vivo la bailarina
    /// (además del default de arranque que cubre `parse()`).
    #[test]
    fn applying_the_miku_theme_activates_the_dancer_mode_live() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        runtime::activate_visualizer_mode(config::VisualizerMode::Barras);
        let miku_index = config::builtin_themes()
            .iter()
            .position(|(id, _)| *id == "miku")
            .expect("el tema miku existe");
        app.theme_menu = Some(ThemeMenuState {
            selected: miku_index,
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));

        assert_eq!(
            runtime::active_theme_name(),
            "miku",
            "el tema miku queda activo"
        );
        assert_eq!(
            runtime::active_visualizer_mode(),
            config::VisualizerMode::Miku,
            "aplicar miku activa la bailarina en vivo"
        );
    }

    #[test]
    fn theme_menu_enter_persist_failure_keeps_applied_theme() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        app.theme_menu = Some(ThemeMenuState {
            selected: 2,
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |_| {
            Err("no se pudo guardar el tema: disco lleno".to_string())
        });

        let expected = config::builtin_themes()[2].0;
        assert!(app.theme_menu.is_none(), "Enter cierra igual");
        assert_eq!(
            runtime::active_theme_name(),
            expected,
            "el tema queda aplicado en la sesión pese al fallo"
        );
        assert_eq!(runtime::active_theme(), config::resolve_theme(expected));
        let err = app.error.as_deref().expect("error visible en la app");
        assert!(!err.is_empty(), "mensaje corto no vacío");
    }

    #[test]
    fn theme_menu_escape_restores_previous_theme() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        let before_name = runtime::active_theme_name();
        let before_theme = runtime::active_theme();
        app.theme_menu = Some(ThemeMenuState {
            selected: 0,
            original_name: before_name.clone(),
            original_theme: before_theme.clone(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| {
            Ok(())
        });
        handle_theme_menu_key(theme_menu_key(KeyCode::Esc), &mut app, |_| {
            panic!("Esc no debe persistir")
        });

        assert!(app.theme_menu.is_none(), "Esc cierra el menú");
        assert_eq!(
            runtime::active_theme(),
            before_theme,
            "Esc restaura los colores originales"
        );
        assert_eq!(
            runtime::active_theme_name(),
            before_name,
            "Esc restaura el id original"
        );
    }

    #[test]
    fn handle_key_routes_to_theme_menu_when_open() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(engine) => engine,
            Err(_) => return,
        };
        app.theme_menu = Some(ThemeMenuState {
            selected: 1,
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
        });

        handle_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);

        assert_eq!(
            app.theme_menu
                .as_ref()
                .expect("el menú sigue abierto")
                .selected,
            2,
            "con el menú abierto las teclas van al menú, no a la lista"
        );
    }

    #[test]
    fn theme_menu_lines_list_all_themes_and_mark_current() {
        let _setup = theme_menu_test_setup();
        let menu = ThemeMenuState {
            selected: 3,
            original_name: "nord".to_string(),
            original_theme: runtime::active_theme(),
        };

        let lines = theme_menu_lines(&menu);
        let texts: Vec<String> =
            lines.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(texts.len(), 8, "una fila por tema: {texts:?}");
        for (id, _) in config::builtin_themes() {
            assert!(
                texts.iter().any(|t| t.contains(id)),
                "falta la fila de {id}: {texts:?}"
            );
        }
        let nord_row = config::builtin_themes()
            .iter()
            .position(|(id, _)| *id == "nord")
            .expect("nord builtin");
        assert!(
            texts[nord_row].contains('●'),
            "● marca el tema actual: {:?}",
            texts[nord_row]
        );
        assert!(
            !texts[0].contains('●'),
            "solo la fila actual lleva el marcador: {:?}",
            texts[0]
        );
        let selected = &lines[3].spans[0];
        assert_eq!(
            selected.style.bg,
            Some(runtime::active_theme().select_bg),
            "la fila seleccionada va resaltada"
        );
    }

    // -------------------------------------------------------------------------
    // Shell del panel de configuración (Ctrl+P)
    // -------------------------------------------------------------------------

    #[test]
    fn settings_opens_with_ctrl_p_from_player_and_list() {
        let Some(mut engine) = engine_or_skip() else { return };
        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);

        let mut app = test_app();
        app.mode = Mode::Player;
        handle_player_key(ctrl_p, &mut app, &mut engine);
        assert!(
            app.settings_panel.is_some(),
            "Ctrl+P abre el panel desde el reproductor"
        );
        assert_eq!(
            app.settings_panel.as_ref().expect("abierto").selected,
            0,
            "arranca en la primera fila"
        );

        let mut app = test_app();
        handle_list_key(ctrl_p, &mut app, &mut engine);
        assert!(
            app.settings_panel.is_some(),
            "Ctrl+P abre el panel desde el explorador"
        );

        // La letra suelta sigue siendo pista anterior, no abre el panel.
        let mut app = test_app();
        app.mode = Mode::Player;
        handle_player_key(theme_menu_key(KeyCode::Char('p')), &mut app, &mut engine);
        assert!(app.settings_panel.is_none(), "la 'p' suelta no abre el panel");
    }

    #[test]
    fn settings_panel_navigation_clamps_without_wrapping() {
        let mut app = test_app();
        open_settings_panel(&mut app);
        let selected = |app: &App| {
            app.settings_panel.as_ref().expect("panel abierto").selected
        };

        // Como el menú de temas (move_selection con clamp): no envuelve.
        handle_settings_panel_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 0, "↑ en la primera fila no se mueve");
        handle_settings_panel_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_settings_panel_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_settings_panel_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_settings_panel_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 3, "↓ frena en la última fila sin envolver");
        handle_settings_panel_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 2, "↑ vuelve una fila");
    }

    // Enter en «Mouse» alterna el estado en vivo (el panel sigue abierto y
    // la selección intacta, como antes).
    #[test]
    fn settings_esc_closes_panel_without_arming_quit() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        open_settings_panel(&mut app);

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.settings_panel.is_none(), "un solo Esc cierra el panel");
        assert!(!app.quit_guard.is_armed(Instant::now()), "y no arma la salida");
        assert!(!app.should_quit);

        // El doble-Esc de salida solo aplica sin modal: hacen falta dos más.
        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(!app.should_quit, "tras cerrar, un Esc solo arma");
        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.should_quit, "el segundo Esc sin modal sale");
    }

    #[test]
    fn settings_panel_lists_the_four_fixed_rows_with_highlight() {
        assert_eq!(
            SETTINGS_ROWS,
            ["Temas", "Mouse", "Bandas", "Modo"],
            "filas fijas (+ Modo al final, sin reordenar)"
        );
        let panel = SettingsPanelState { selected: 1 };
        let lines = settings_panel_lines(&panel, true);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts.len(), 4, "una fila por sección: {texts:?}");
        for row in SETTINGS_ROWS {
            assert!(
                texts.iter().any(|t| t.contains(row)),
                "falta la fila {row}: {texts:?}"
            );
        }
        assert_eq!(
            lines[1].spans[0].style.bg,
            Some(theme().select_bg),
            "la fila seleccionada va resaltada"
        );
        assert_ne!(
            lines[0].spans[0].style.bg,
            Some(theme().select_bg),
            "las demás filas no van resaltadas"
        );
    }

    #[test]
    fn settings_panel_renders_inside_tiny_terminals() {
        let panel = SettingsPanelState { selected: 0 };
        for (w, h) in [(10, 2), (23, 10), (40, 2), (1, 1), (0, 0)] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
            terminal
                .draw(|frame| render_settings_panel(frame, &panel, true))
                .unwrap_or_else(|e| panic!("{w}x{h}: {e}"));
        }
    }

    #[test]
    fn footer_shows_settings_chip_in_player_and_browser() {
        let mut app = test_app();
        // Reproductor y explorador (panel + pantalla completa): chip del panel
        // sin `t` (la tecla queda libre).
        for (mode, open) in [
            (Mode::Player, false),
            (Mode::Player, true),
            (Mode::Browser, false),
        ] {
            app.mode = mode;
            app.explorer_open = open;
            let joined: String = footer_lines(&app, false)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                joined.contains("Ctrl+P: config"),
                "falta el chip en {mode:?}/{open}: {joined}"
            );
            assert!(
                !joined.contains(": temas"),
                "sin `t: temas` ({mode:?}/{open}): {joined}"
            );
            for line in footer_lines(&app, false) {
                assert!(
                    line_width(&line) <= 78,
                    "línea del pie demasiado ancha ({}): {}",
                    line_width(&line),
                    line_text(&line)
                );
            }
        }
    }

    #[test]
    fn mouse_is_inert_under_settings_panel() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        *app.hit.borrow_mut() = sample_hit();
        open_settings_panel(&mut app);
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "el clic no atraviesa el panel");
        assert_eq!(app.drag, None);
    }

    #[test]
    fn settings_chip_click_opens_the_panel() {
        let Some(mut engine) = engine_or_skip() else { return };
        assert_eq!(
            key_for_chip("Ctrl+P"),
            Some(KeyCode::Char('p')),
            "el chip resuelve a la tecla de settings"
        );
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        let buffer = render_player_frame(&app, &engine);
        let chip = app
            .hit
            .borrow()
            .chips
            .iter()
            .find(|(_, key)| *key == KeyCode::Char('p'))
            .map(|(rect, _)| *rect)
            .expect("chip «Ctrl+P: config» registrado");
        let row: String = (chip.x..chip.x + chip.width)
            .map(|x| buffer[(x, chip.y)].symbol().to_string())
            .collect();
        assert!(row.starts_with("Ctrl+P"), "el área es la del chip dibujado: {row:?}");
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), chip.x, chip.y),
            &mut app,
            &mut engine,
        );
        assert!(app.settings_panel.is_some(), "clickear el chip abre el panel");
    }

    #[test]
    fn handle_key_routes_to_settings_panel_when_open() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        open_settings_panel(&mut app);

        handle_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);

        assert_eq!(
            app.settings_panel.as_ref().expect("sigue abierto").selected,
            1,
            "con el panel abierto las teclas van al panel, no a la lista"
        );
        assert!(app.search.is_none());
    }

    // -------------------------------------------------------------------------
    // Temas dentro del panel + eliminar `t`
    // -------------------------------------------------------------------------

    #[test]
    fn settings_enter_on_themes_opens_theme_submodal_keeping_panel() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        open_settings_panel(&mut app);
        assert_eq!(
            app.settings_panel.as_ref().expect("panel abierto").selected,
            0,
            "la primera fila es Temas"
        );
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        assert!(
            app.settings_panel.is_some(),
            "el panel sigue abierto debajo del sub-modal"
        );
        assert!(
            app.theme_menu.is_some(),
            "Enter en Temas abre el ThemeMenuState existente"
        );
    }

    // -------------------------------------------------------------------------
    // Toggle de Mouse en vivo + persistencia de `[ui] mouse`
    // -------------------------------------------------------------------------
    // Enter en «Mouse» alterna el estado y persiste.

    #[test]
    fn settings_enter_on_mouse_toggles_state_and_persists() {
        // Enter en «Mouse» alterna el estado en el momento, deja el panel
        // abierto y persiste `[ui] mouse` (ruta temporal inyectada).
        let path = std::env::temp_dir()
            .join(format!("mp3tui_t4_mouse_toggle_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "[ui]\nmouse = true\n").unwrap();

        let mut app = test_app();
        assert!(app.mouse_enabled, "el estado vivo arranca habilitado");
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 1;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |enabled| {
            config::persist_mouse_enabled_at(&path, enabled)
        });
        assert!(!app.mouse_enabled, "Enter apaga el mouse en el momento");
        assert!(
            app.settings_panel.is_some(),
            "el toggle no cierra el panel"
        );
        let text = std::fs::read_to_string(&path).expect("mouse persistido");
        assert!(
            text.contains("mouse = false"),
            "persiste `[ui] mouse`: {text:?}"
        );

        // Segundo Enter lo vuelve a prender (toggle, no set).
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |enabled| {
            config::persist_mouse_enabled_at(&path, enabled)
        });
        assert!(app.mouse_enabled, "otro Enter lo vuelve a prender");
        let text = std::fs::read_to_string(&path).expect("mouse persistido");
        assert!(
            text.contains("mouse = true"),
            "persiste el re-encendido: {text:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settings_enter_on_mouse_persist_failure_keeps_toggled_state() {
        // Como Temas/Bandas: se aplica primero y un fallo de guardado solo
        // deja el aviso, sin revertir el toggle.
        let mut app = test_app();
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 1;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| {
            Err("sin disco".to_string())
        });
        assert!(!app.mouse_enabled, "el toggle aplica aunque falle el guardado");
        assert!(
            app.settings_panel.is_some(),
            "y el panel sigue abierto"
        );
        assert_eq!(
            app.error.as_deref(),
            Some("sin disco"),
            "el fallo queda como aviso"
        );
    }

    #[test]
    fn settings_mouse_row_shows_current_state() {
        // La fila «Mouse» muestra el estado vivo: «Mouse: sí» / «Mouse: no».
        let panel = SettingsPanelState { selected: 1 };
        let on: Vec<String> = settings_panel_lines(&panel, true)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            on.iter().any(|t| t.contains("Mouse: sí")),
            "prendido se lee «Mouse: sí»: {on:?}"
        );
        let off: Vec<String> = settings_panel_lines(&panel, false)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            off.iter().any(|t| t.contains("Mouse: no")),
            "apagado se lee «Mouse: no»: {off:?}"
        );
        assert_eq!(on.len(), 4, "siguen siendo 4 filas: {on:?}");
    }

    #[test]
    fn mouse_toggle_off_stays_off_after_closing_panel() {
        // El toggle manda sobre el estado previo: apagar con el panel abierto
        // y cerrarlo no reactiva la captura.
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 1;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        assert!(!app.mouse_enabled, "el toggle apaga en el momento");

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.settings_panel.is_none(), "Esc cierra el panel");
        assert!(
            !app.mouse_enabled,
            "cerrar el panel no reactiva la captura"
        );

        // Y el handler lo respeta: el clic ya no actúa.
        *app.hit.borrow_mut() = sample_hit();
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "apagado, el clic no actúa");
        assert_eq!(app.drag, None);
    }

    #[test]
    fn mouse_disabled_globally_ignores_events_even_without_modals() {
        // El estado vivo está ENCIMA del chequeo de modales: apagado, el
        // mouse se ignora aunque no haya ningún modal abierto.
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.mouse_enabled = false;
        assert!(
            app.url_input.is_none()
                && app.theme_menu.is_none()
                && app.bands_menu.is_none()
                && app.mode_menu.is_none()
                && app.settings_panel.is_none(),
            "sin modales abiertos"
        );
        *app.hit.borrow_mut() = sample_hit();
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "apagado, el clic no fija volumen");
        assert_eq!(app.drag, None, "ni arranca arrastre");
    }

    #[test]
    fn t_key_is_free_does_nothing_in_player_and_list() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        handle_player_key(theme_menu_key(KeyCode::Char('t')), &mut app, &mut engine);
        assert!(
            app.theme_menu.is_none() && app.settings_panel.is_none(),
            "'t' queda libre en el reproductor: no abre nada"
        );
        let mut app = test_app();
        handle_list_key(theme_menu_key(KeyCode::Char('t')), &mut app, &mut engine);
        assert!(
            app.theme_menu.is_none() && app.settings_panel.is_none(),
            "'t' queda libre en la lista: no abre nada"
        );
    }

    #[test]
    fn theme_submodal_enter_saves_and_returns_to_panel() {
        let _setup = theme_menu_test_setup();
        let path = std::env::temp_dir()
            .join(format!("mp3tui_t2_submodal_enter_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut app = test_app();
        open_settings_panel(&mut app);
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        let menu = app.theme_menu.as_ref().expect("sub-modal abierto");
        let expected = config::builtin_themes()[menu.selected].0;
        handle_theme_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |id| {
            config::persist_theme_name_at(&path, id)
        });
        assert!(app.theme_menu.is_none(), "Enter cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "pero VUELVE al panel, no lo cierra"
        );
        assert_eq!(runtime::active_theme_name(), expected, "el tema queda aplicado");
        let text = std::fs::read_to_string(&path).expect("tema persistido");
        assert!(
            text.contains(expected),
            "el tema se persiste como hoy: {text:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn theme_submodal_esc_restores_and_returns_to_panel() {
        let _setup = theme_menu_test_setup();
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        open_settings_panel(&mut app);
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        let before_name = runtime::active_theme_name();
        let before_theme = runtime::active_theme();
        // Previsualiza otro tema moviéndose dentro del sub-modal.
        handle_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);
        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.theme_menu.is_none(), "Esc cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "y VUELVE al panel, no lo cierra"
        );
        assert_eq!(runtime::active_theme_name(), before_name, "Esc restaura el id");
        assert_eq!(runtime::active_theme(), before_theme, "Esc restaura los colores");
    }

    #[test]
    fn mouse_is_inert_under_theme_submodal() {
        // El guard inerte ya cubre el panel  y el sub-modal (existente):
        // se verifica, no se duplica.
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        *app.hit.borrow_mut() = sample_hit();
        open_settings_panel(&mut app);
        open_theme_menu(&mut app);
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "el clic no atraviesa el sub-modal");
        assert_eq!(app.drag, None);
    }

    #[test]
    fn footer_no_longer_shows_t_temas_but_keeps_ctrl_p() {
        let mut app = test_app();
        for (mode, open) in [
            (Mode::Player, false),
            (Mode::Player, true),
            (Mode::Browser, false),
        ] {
            app.mode = mode;
            app.explorer_open = open;
            let joined: String = footer_lines(&app, false)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                !joined.contains(": temas"),
                "sin chip `t: temas` en {mode:?}/{open}: {joined}"
            );
            assert!(
                joined.contains("Ctrl+P: config"),
                "el chip del panel sigue en {mode:?}/{open}: {joined}"
            );
            for line in footer_lines(&app, false) {
                assert!(
                    line_width(&line) <= 78,
                    "línea del pie demasiado ancha ({}): {}",
                    line_width(&line),
                    line_text(&line)
                );
            }
        }
    }

    // -------------------------------------------------------------------------
    // Selector de Bandas dentro del panel + persistencia del estilo
    // -------------------------------------------------------------------------

    static BANDS_MENU_TEST_LOCK: std::sync::Mutex<()> =
        std::sync::Mutex::new(());

    struct BandsMenuTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        style: config::VisualizerStyle,
    }

    impl Drop for BandsMenuTestGuard {
        fn drop(&mut self) {
            runtime::activate_visualizer_style(self.style);
        }
    }

    /// Serializa los tests que mutan el estilo activo global y lo restaura al
    /// salir (incluso si el test falla).
    fn bands_menu_test_setup() -> BandsMenuTestGuard {
        let lock = BANDS_MENU_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        BandsMenuTestGuard {
            _lock: lock,
            style: runtime::active_visualizer_style(),
        }
    }

    #[test]
    fn settings_enter_on_bands_opens_bands_submodal_keeping_panel() {
        let _setup = bands_menu_test_setup();
        let mut app = test_app();
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 2;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        assert!(
            app.settings_panel.is_some(),
            "el panel sigue abierto debajo del sub-modal"
        );
        assert!(
            app.bands_menu.is_some(),
            "Enter en Bandas abre el selector de estilos"
        );
    }

    #[test]
    fn bands_submodal_navigation_clamps_and_previews_live() {
        let _setup = bands_menu_test_setup();
        runtime::activate_visualizer_style(config::VisualizerStyle::Ascii);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_bands_menu(&mut app);
        let selected = |app: &App| {
            app.bands_menu.as_ref().expect("sub-modal abierto").selected
        };
        assert_eq!(selected(&app), 1, "arranca en el estilo activo (ascii)");

        // Sin envoltura, como el menú de temas.
        handle_bands_menu_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 0, "↑ va a auto");
        handle_bands_menu_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 0, "↑ en la primera fila no se mueve");
        handle_bands_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_bands_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_bands_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 2, "↓ frena en unicode sin envolver");

        // Navegar previsualiza en vivo: el estilo efectivo sigue al cursor
        // (forzados ganan sin importar WT_SESSION, así no depende del env).
        assert_eq!(
            runtime::active_visualizer_style(),
            config::VisualizerStyle::Unicode
        );
        assert_eq!(
            runtime::effective_visualizer_style(),
            config::VisualizerStyle::Unicode,
            "el fondo previsualiza el estilo navegado"
        );
    }

    #[test]
    fn dispatch_routes_to_bands_submodal_when_open() {
        let _setup = bands_menu_test_setup();
        let Some(mut engine) = engine_or_skip() else { return };
        runtime::activate_visualizer_style(config::VisualizerStyle::Auto);
        let mut app = test_app();
        app.mode = Mode::Player;
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 2;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));

        handle_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);

        assert_eq!(
            app.bands_menu.as_ref().expect("sigue abierto").selected,
            1,
            "con el sub-modal abierto las teclas van al sub-modal, no al panel"
        );
        assert_eq!(
            app.settings_panel.as_ref().expect("panel debajo").selected,
            2,
            "el panel de abajo no se mueve"
        );
    }

    #[test]
    fn bands_submodal_enter_saves_and_returns_to_panel() {
        let _setup = bands_menu_test_setup();
        let path = std::env::temp_dir()
            .join(format!("mp3tui_t3_bands_enter_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        runtime::activate_visualizer_style(config::VisualizerStyle::Auto);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_bands_menu(&mut app);
        handle_bands_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_bands_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |style| {
            config::persist_visualizer_style_at(&path, style)
        });
        assert!(app.bands_menu.is_none(), "Enter cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "pero VUELVE al panel, no lo cierra"
        );
        assert_eq!(
            runtime::active_visualizer_style(),
            config::VisualizerStyle::Ascii,
            "el estilo queda aplicado"
        );
        let text = std::fs::read_to_string(&path).expect("estilo persistido");
        assert!(
            text.contains("style = \"ascii\""),
            "el estilo se persiste como hoy: {text:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bands_submodal_esc_restores_and_returns_to_panel() {
        let _setup = bands_menu_test_setup();
        runtime::activate_visualizer_style(config::VisualizerStyle::Ascii);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_bands_menu(&mut app);
        // Previsualiza otro estilo moviéndose dentro del sub-modal.
        handle_bands_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        assert_eq!(
            runtime::active_visualizer_style(),
            config::VisualizerStyle::Unicode,
            "navegar previsualiza"
        );
        handle_bands_menu_key(theme_menu_key(KeyCode::Esc), &mut app, |_| Ok(()));
        assert!(app.bands_menu.is_none(), "Esc cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "y VUELVE al panel, no lo cierra"
        );
        assert_eq!(
            runtime::active_visualizer_style(),
            config::VisualizerStyle::Ascii,
            "Esc restaura el estilo previo"
        );
    }

    #[test]
    fn mouse_is_inert_under_bands_submodal() {
        // El guard inerte cubre el panel y el sub-modal: se verifica.
        let _setup = bands_menu_test_setup();
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        *app.hit.borrow_mut() = sample_hit();
        open_settings_panel(&mut app);
        open_bands_menu(&mut app);
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "el clic no atraviesa el sub-modal");
        assert_eq!(app.drag, None);
    }

    #[test]
    fn bands_footer_shows_preview_hints() {
        let _setup = bands_menu_test_setup();
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_bands_menu(&mut app);
        let joined: String = footer_lines(&app, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("previsualizar"),
            "el pie del sub-modal explica el preview: {joined}"
        );
        assert!(
            joined.contains("guardar"),
            "el pie del sub-modal explica Enter: {joined}"
        );
    }

    // -------------------------------------------------------------------------
    // Selector de Modo dentro del panel + fallback a
    // barras
    // -------------------------------------------------------------------------

    static MODE_MENU_TEST_LOCK: std::sync::Mutex<()> =
        std::sync::Mutex::new(());

    struct ModeMenuTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        mode: config::VisualizerMode,
    }

    impl Drop for ModeMenuTestGuard {
        fn drop(&mut self) {
            runtime::activate_visualizer_mode(self.mode);
        }
    }

    /// Serializa los tests que mutan el modo activo global y lo restaura al
    /// salir (incluso si el test falla).
    fn mode_menu_test_setup() -> ModeMenuTestGuard {
        let lock = MODE_MENU_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ModeMenuTestGuard {
            _lock: lock,
            mode: runtime::active_visualizer_mode(),
        }
    }

    #[test]
    fn settings_enter_on_mode_opens_mode_submodal_keeping_panel() {
        let _setup = mode_menu_test_setup();
        let mut app = test_app();
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 3;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        assert!(
            app.settings_panel.is_some(),
            "el panel sigue abierto debajo del sub-modal"
        );
        assert!(
            app.mode_menu.is_some(),
            "Enter en Modo abre el selector de modos"
        );
    }

    #[test]
    fn mode_submodal_navigation_clamps_and_previews_live() {
        let _setup = mode_menu_test_setup();
        runtime::activate_visualizer_mode(config::VisualizerMode::Barras);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_mode_menu(&mut app);
        let selected = |app: &App| {
            app.mode_menu.as_ref().expect("sub-modal abierto").selected
        };
        assert_eq!(selected(&app), 0, "arranca en el modo activo (barras)");

        // Sin envoltura, como los otros selectores.
        handle_mode_menu_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 0, "↑ en la primera fila no se mueve");
        for _ in 0..7 {
            handle_mode_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        }
        assert_eq!(selected(&app), 5, "↓ frena en miku sin envolver (6 modos)");

        // Navegar previsualiza en vivo: el modo activo sigue al cursor.
        assert_eq!(
            runtime::active_visualizer_mode(),
            config::VisualizerMode::Miku
        );
    }

    #[test]
    fn dispatch_routes_to_mode_submodal_when_open() {
        let _setup = mode_menu_test_setup();
        let Some(mut engine) = engine_or_skip() else { return };
        runtime::activate_visualizer_mode(config::VisualizerMode::Barras);
        let mut app = test_app();
        app.mode = Mode::Player;
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 3;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));

        handle_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);

        assert_eq!(
            app.mode_menu.as_ref().expect("sigue abierto").selected,
            1,
            "con el sub-modal abierto las teclas van al sub-modal, no al panel"
        );
        assert_eq!(
            app.settings_panel.as_ref().expect("panel debajo").selected,
            3,
            "el panel de abajo no se mueve"
        );
    }

    #[test]
    fn mode_submodal_enter_saves_and_returns_to_panel() {
        let _setup = mode_menu_test_setup();
        let path = std::env::temp_dir()
            .join(format!("mp3tui_t1_mode_enter_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        runtime::activate_visualizer_mode(config::VisualizerMode::Barras);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_mode_menu(&mut app);
        handle_mode_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_mode_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |mode| {
            config::persist_visualizer_mode_at(&path, mode)
        });
        assert!(app.mode_menu.is_none(), "Enter cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "pero VUELVE al panel, no lo cierra"
        );
        assert_eq!(
            runtime::active_visualizer_mode(),
            config::VisualizerMode::Onda,
            "el modo queda aplicado"
        );
        let text = std::fs::read_to_string(&path).expect("modo persistido");
        assert!(
            text.contains("mode = \"onda\""),
            "el modo se persiste como hoy: {text:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mode_submodal_esc_restores_and_returns_to_panel() {
        let _setup = mode_menu_test_setup();
        runtime::activate_visualizer_mode(config::VisualizerMode::Barras);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_mode_menu(&mut app);
        // Previsualiza otro modo moviéndose dentro del sub-modal.
        handle_mode_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        assert_eq!(
            runtime::active_visualizer_mode(),
            config::VisualizerMode::Onda,
            "navegar previsualiza"
        );
        handle_mode_menu_key(theme_menu_key(KeyCode::Esc), &mut app, |_| Ok(()));
        assert!(app.mode_menu.is_none(), "Esc cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "y VUELVE al panel, no lo cierra"
        );
        assert_eq!(
            runtime::active_visualizer_mode(),
            config::VisualizerMode::Barras,
            "Esc restaura el modo previo"
        );
    }

    #[test]
    fn mouse_is_inert_under_mode_submodal() {
        // El guard inerte cubre el panel y el sub-modal: se verifica.
        let _setup = mode_menu_test_setup();
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        *app.hit.borrow_mut() = sample_hit();
        open_settings_panel(&mut app);
        open_mode_menu(&mut app);
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "el clic no atraviesa el sub-modal");
        assert_eq!(app.drag, None);
    }

    #[test]
    fn mode_footer_shows_preview_hints() {
        let _setup = mode_menu_test_setup();
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_mode_menu(&mut app);
        let joined: String = footer_lines(&app, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("previsualizar"),
            "el pie del sub-modal explica el preview: {joined}"
        );
        assert!(
            joined.contains("guardar"),
            "el pie del sub-modal explica Enter: {joined}"
        );
    }

    #[test]
    fn visualizer_paint_routes_all_modes_to_their_painters() {
        // Los 5 modos tienen pintor propio, sin fallback a barras.
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Barras),
            VisualizerPaint::Bars,
            "barras recorre el path actual"
        );
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Onda),
            VisualizerPaint::Wave,
            "onda va al osciloscopio, no a las barras"
        );
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Espejo),
            VisualizerPaint::Mirror,
            "espejo va a su pintor simétrico, no a las barras"
        );
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Vu),
            VisualizerPaint::Vu,
            "vu va a su medidor con RMS, no a las barras"
        );
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Circular),
            VisualizerPaint::Circular,
            "circular va a su plot polar, no a las barras"
        );
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Miku),
            VisualizerPaint::Miku,
            "miku va a su bailarina, no a las barras"
        );
        assert!(
            crate::visualizer::MIKU_POSES >= 4,
            "el arco de salto del analyzer tiene poses suficientes"
        );
    }

    /// El modo miku pinta la bailarina: avanza de pose por timer aunque haya
    /// silencio y el frame renderiza los glifos de la pose actual, también con
    /// el panel abierto a ancho chico.
    #[test]
    fn visualizer_miku_mode_paints_the_dancer_in_all_poses() {
        let Some(engine) = engine_or_skip() else { return };
        runtime::preview_visualizer_mode(config::VisualizerMode::Miku);
        let mut analyzer = SpectrumAnalyzer::new();
        // Silencio: el timer de respiración mueve la pose sola (arco de salto).
        for _ in 0..200 {
            analyzer.tick_dance(true);
            if analyzer.dance_pose() != 0 {
                break;
            }
        }
        assert_ne!(analyzer.dance_pose(), 0, "el timer movió la pose");

        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.analyzer = analyzer;
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app, &engine))
            .expect("draw");
        let text = player_screen_text(&terminal.backend().buffer().clone());
        assert!(
            text.contains('@'),
            "la animación real de Miku está en pantalla en el estilo por defecto"
        );

        // Panel abierto a 64x24: la bailarina se recorta sin paniquear y
        // dejando el resto del frame intacto.
        app.explorer_open = true;
        let small = ratatui::backend::TestBackend::new(64, 24);
        let mut terminal = ratatui::Terminal::new(small).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app, &engine))
            .expect("draw sin panico con el panel abierto");
    }

    /// Modo `miku` en estilo unicode: dibuja la textura real de Miku
    /// (emojicombos) reducida a medios bloques, animada con el arco de salto,
    /// y se recorta sin paniquear con el panel abierto.
    #[test]
    fn visualizer_miku_unicode_paints_the_real_art_animated() {
        let Some(engine) = engine_or_skip() else { return };
        let previous = runtime::active_visualizer_style();
        runtime::preview_visualizer_style(config::VisualizerStyle::Unicode);
        runtime::preview_visualizer_mode(config::VisualizerMode::Miku);

        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        let mut analyzer = SpectrumAnalyzer::new();
        // Silencio: el timer de respiración saca la pose del reposo y activa
        // el arco de salto (el camino del beat está cubierto en los tests
        // puros de `next_dance_pose`).
        for _ in 0..25 {
            analyzer.tick_dance(true);
        }
        assert!(
            analyzer.dance_pose() != 0,
            "el timer movió la pose al arco de salto"
        );
        app.analyzer = analyzer;

        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app, &engine))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let text = player_screen_text(&buffer);
        assert!(
            text.contains('@'),
            "el fondo del lienzo (@ blancos) está en pantalla"
        );
        assert!(
            !text.contains("MIKU"),
            "la animación real, no las poses viejas"
        );
        // El fondo es @ blanco y la silueta de Miku se dibuja negra encima.
        let has_white = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                buffer[(x, y)].symbol() == "@"
                    && buffer[(x, y)].style().fg
                        == Some(ratatui::style::Color::Rgb(0xff, 0xff, 0xff))
            })
        });
        let has_black = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                buffer[(x, y)].symbol() != "@"
                    && buffer[(x, y)].style().fg
                        == Some(ratatui::style::Color::Rgb(0x00, 0x00, 0x00))
            })
        });
        assert!(has_white, "el cuerpo de Miku se dibuja sobre @ blancos");
        assert!(has_black, "la silueta oscura de Miku está sobre el fondo");

        app.explorer_open = true;
        let small = ratatui::backend::TestBackend::new(64, 24);
        let mut terminal = ratatui::Terminal::new(small).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app, &engine))
            .expect("draw sin panico en 64x24 con panel abierto");

        runtime::preview_visualizer_style(previous);
    }

    #[test]
    fn test_stale_resolution_ignored_by_request_id() {
        let mut app = test_app();
        app.ytdlp_request_id = 42;
        app.status = Status::Resolving;
        app.error = Some("error previo".to_string());

        // Stale result (request_id 41 != 42) must be ignored
        let stale_result = Ok(crate::source::ytdlp::Resolved {
            title: "Stale Song".to_string(),
            audio_url: "https://example.com/stale.m4a".to_string(),
        });
        let action = handle_ytdlp_result(&mut app, 41, stale_result);
        assert_eq!(action, None, "stale result must be discarded");
        assert_eq!(app.status, Status::Resolving);
        assert_eq!(app.error.as_deref(), Some("error previo"));

        // Matching result (request_id 42 == 42) must be accepted
        let matching_result = Ok(crate::source::ytdlp::Resolved {
            title: "Fresh Song".to_string(),
            audio_url: "https://example.com/fresh.m4a".to_string(),
        });
        let action = handle_ytdlp_result(&mut app, 42, matching_result);
        assert_eq!(
            action,
            Some(("https://example.com/fresh.m4a".to_string(), "Fresh Song".to_string()))
        );
        assert_eq!(app.status, Status::Connecting);
        assert_eq!(app.track_label, "Fresh Song");
        assert_eq!(app.error, None);
    }

    #[test]
    fn test_compute_status_transitions() {
        let snapshot_stopped = EngineSnapshot {
            is_connecting: false,
            is_playing: false,
            is_paused: false,
            had_playback_error: false,
        };
        assert_eq!(
            compute_status(true, snapshot_stopped),
            Status::Resolving,
            "must stay Resolving while yt-dlp is pending even if engine is stopped"
        );

        let snapshot_connecting = EngineSnapshot {
            is_connecting: true,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        assert_eq!(
            compute_status(true, snapshot_connecting),
            Status::Resolving,
            "must stay Resolving while yt-dlp is pending even if engine is connecting"
        );

        assert_eq!(
            compute_status(false, snapshot_connecting),
            Status::Connecting,
            "must stay Connecting while engine.is_connecting is true"
        );

        let snapshot_playing = EngineSnapshot {
            is_connecting: false,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        assert_eq!(
            compute_status(false, snapshot_playing),
            Status::Playing,
            "must transition to Playing once connection finishes"
        );

        let snapshot_failed = EngineSnapshot {
            is_connecting: false,
            is_playing: false,
            is_paused: false,
            had_playback_error: true,
        };
        assert_eq!(
            compute_status(false, snapshot_failed),
            Status::Stopped,
            "connection or playback failure must result in Stopped"
        );
    }

    #[test]
    fn test_url_track_natural_end_with_repeat_all_advances_to_library() {
        let mut app = test_app();
        app.is_url = true;
        app.current_url = Some((
            "https://example.com/audio.mp3".to_string(),
            "audio.mp3".to_string(),
        ));
        app.status = Status::Playing;
        app.repeat = RepeatMode::All;
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t1_urlall1_{}.wav", std::process::id()));
        let t2 = dir.join(format!("mp3tui_t1_urlall2_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);
        let _ = std::fs::write(&t2, &wav);
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(t1.clone(), "local_1"),
            track_with_path(t2.clone(), "local_2"),
        ]));
        refresh_view(&mut app);
        app.selected = 0;
        app.track_label = "https://example.com/audio.mp3".to_string();

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                let _ = std::fs::remove_file(&t2);
                return;
            }
        };

        handle_natural_end(&mut app, &mut engine);

        assert!(
            !app.is_url,
            "URL terminada con repeat All debe entrar a la biblioteca local"
        );
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "debe avanzar a la primera pista local"
        );
        assert!(app.current_url.is_none(), "la sesión ya no es de URL");
        assert!(
            app.error.is_none(),
            "no debe fallar al arrancar la pista local: {:?}",
            app.error
        );

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    #[test]
    fn test_non_playback_app_error_does_not_mask_natural_end() {
        let mut app = test_app();
        app.error = Some("config: advertencia de configuración".to_string());
        app.status = Status::Playing;

        let engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        // Engine is not playing, not paused, and has no playback error (ended normally).
        let playback_failed = engine.had_playback_error() || app.ytdlp_failed;
        let snapshot = EngineSnapshot::from_engine(&engine, playback_failed);
        let status = compute_status(app.ytdlp_rx.is_some(), snapshot);
        assert_eq!(
            status,
            Status::Ended,
            "non-playback app.error must not mask natural end into Stopped"
        );
    }

    fn make_test_wav(num_samples: usize) -> Vec<u8> {
        let sample_rate = 44100u32;
        let channels = 1u16;
        let bits_per_sample = 16u16;
        let byte_rate = sample_rate * (channels as u32) * (bits_per_sample as u32 / 8);
        let block_align = channels * (bits_per_sample / 8);
        let data_len = (num_samples * 2) as u32;
        let riff_len = 36 + data_len;

        let mut wav = Vec::with_capacity(44 + num_samples * 2);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&riff_len.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits_per_sample.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..num_samples {
            let sample = ((i as f32 / 10.0).sin() * 10000.0) as i16;
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    /// Servidor HTTP local de un solo uso que sirve `wav` y devuelve la URL
    /// (`http://127.0.0.1:<puerto>/<filename>`). Permite que las pruebas de la
    /// ruta de URL dialen SOLO loopback, nunca un host externo. El thread
    /// acepta reconexiones del precargador y termina con el proceso.
    fn local_wav_url(filename: &str, wav: &[u8]) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");
        let wav = wav.to_vec();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else {
                    break;
                };
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    wav.len()
                );
                if socket.write_all(header.as_bytes()).is_err() {
                    continue;
                }
                let _ = socket.write_all(&wav);
            }
        });
        format!("http://{addr}/{filename}")
    }

    fn track_with_path(path: PathBuf, name: &str) -> Track {
        Track {
            path,
            title: name.to_string(),
            artist: None,
            album: None,
            duration: None,
            folder: "x".to_string(),
        }
    }

    /// Ítem de cola con URL pendiente: se resuelve recién cuando le toca sonar.
    fn queue_url(title: &str) -> QueueItem {
        QueueItem::Url {
            url: format!("https://x/{title}"),
            title: title.to_string(),
        }
    }

    /// App con dos pistas locales REALES (WAV temporales) para los casos de
    /// fallback al pool; devuelve sus caminos para poder borrarlos al final.
    fn queue_fallback_app() -> (App, PathBuf, PathBuf) {
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t8pool1_{}.wav", std::process::id()));
        let t2 = dir.join(format!("mp3tui_t8pool2_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);
        let _ = std::fs::write(&t2, &wav);
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(t1.clone(), "track_1"),
            track_with_path(t2.clone(), "track_2"),
        ]));
        refresh_view(&mut app);
        (app, t1, t2)
    }

    #[test]
    fn test_local_playback_clears_is_url_and_allows_natural_end_advance() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t61_adv1_{}.wav", std::process::id()));
        let t2 = dir.join(format!("mp3tui_t61_adv2_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);
        let _ = std::fs::write(&t2, &wav);

        let trk1 = track_with_path(t1.clone(), "track_1");
        let trk2 = track_with_path(t2.clone(), "track_2");

        app.library = Some(crate::playlist::from_tracks_for_test(vec![trk1, trk2]));
        refresh_view(&mut app);
        app.repeat = RepeatMode::All;

        // 1. Simulate prior URL playback
        app.is_url = true;
        app.track_label = "https://example.com/audio.mp3".to_string();
        app.status = Status::Playing;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                let _ = std::fs::remove_file(&t2);
                return;
            }
        };

        // 2. User plays first library track via normal Enter path (play_selected)
        app.selected = 0;
        play_selected(&mut app, &mut engine);

        // Una pista local debe limpiar `is_url`.
        assert!(!app.is_url, "playing local track must clear is_url");
        assert_eq!(app.current_path.as_ref(), Some(&t1));

        // 3. Track ends naturally: should advance to track_2
        handle_natural_end(&mut app, &mut engine);
        assert_eq!(app.selected, 1, "natural end must auto-advance local track");
        assert_eq!(app.current_path.as_ref(), Some(&t2));

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    #[test]
    fn test_url_natural_end_repeat_off_advances_to_first_library_track() {
        let mut app = test_app();
        app.is_url = true;
        app.current_url = Some((
            "https://example.com/remote.mp3".to_string(),
            "remote.mp3".to_string(),
        ));
        app.status = Status::Playing;
        app.repeat = RepeatMode::Off;
        app.track_label = "https://example.com/remote.mp3".to_string();
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t1_urloff1_{}.wav", std::process::id()));
        let t2 = dir.join(format!("mp3tui_t1_urloff2_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);
        let _ = std::fs::write(&t2, &wav);
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(t1.clone(), "local_1"),
            track_with_path(t2.clone(), "local_2"),
        ]));
        refresh_view(&mut app);

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                let _ = std::fs::remove_file(&t2);
                return;
            }
        };

        handle_natural_end(&mut app, &mut engine);

        assert!(
            !app.is_url,
            "repeat Off tras una URL debe avanzar a la biblioteca local"
        );
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "la biblioteca local entra desde su primera pista"
        );
        assert_eq!(app.selected, 0);
        assert!(
            app.error.is_none(),
            "no debe fallar al arrancar la pista local: {:?}",
            app.error
        );

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    #[test]
    fn test_url_natural_end_repeat_one_dispatches_replay_without_spawning() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        // Sesión URL con posición por encima del mínimo: wav local de 6 s y
        // seek instantáneo (los archivos son seekables y el seek guarda
        // played_frames sincrónicamente, sin esperar reproducción real).
        let t6 = std::env::temp_dir().join(format!(
            "mp3tui_t11_urlok_{}.wav",
            std::process::id()
        ));
        let _ = std::fs::write(&t6, make_test_wav(44100 * 6));
        if engine.play(&t6).is_err() {
            let _ = std::fs::remove_file(&t6);
            return;
        }
        assert!(
            engine.seek(Duration::from_secs(6)).is_ok(),
            "setup: el seek en archivo local debe funcionar"
        );
        assert!(
            engine.position() >= crate::playback::MIN_URL_REPLAY,
            "setup: la posición debe superar el mínimo ({:?})",
            engine.position()
        );

        app.is_url = true;
        app.status = Status::Playing;
        app.repeat = RepeatMode::One;
        let original = "https://www.youtube.com/watch?v=abc123".to_string();
        app.current_url = Some((original.clone(), "Video".to_string()));
        app.track_label = "Video".to_string();

        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(
            dispatched.as_ref().map(|r| r.original_url.as_str()),
            Some(original.as_str()),
            "One con URL larga debe pedir re-lanzar la URL original"
        );
        assert!(
            app.ytdlp_rx.is_none(),
            "el test no debe lanzar yt-dlp: la orden va por el dispatcher"
        );
        assert_ne!(app.status, Status::Resolving, "sin resolución pendiente");
        assert!(
            app.is_url,
            "la sesión sigue siendo de URL"
        );
        assert_eq!(
            app.current_url.as_ref().map(|(u, _)| u.as_str()),
            Some(original.as_str()),
            "la URL original debe conservarse para el próximo replay"
        );

        let _ = std::fs::remove_file(&t6);
    }

    #[test]
    fn test_url_natural_end_repeat_one_too_short_stops_with_message() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        // Motor recién creado: posición 0, por debajo del mínimo.
        assert_eq!(engine.position(), Duration::ZERO);

        app.is_url = true;
        app.status = Status::Playing;
        app.repeat = RepeatMode::One;
        app.current_url = Some((
            "https://www.youtube.com/watch?v=short".to_string(),
            "Short".to_string(),
        ));
        app.track_label = "Short".to_string();

        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(
            dispatched, None,
            "una URL que duró menos del mínimo no se re-lanza"
        );
        assert_eq!(
            app.error.as_deref(),
            Some("La canción terminó enseguida; no se repite"),
            "debe mostrar el aviso de corte"
        );
        assert!(app.ytdlp_rx.is_none(), "sin yt-dlp en el test");
    }

    /// La cola manda sobre el fin de pista: con un ítem encolado se reproduce
    /// ese y no se aplica `repeat`.
    #[test]
    fn test_natural_end_with_queued_item_takes_over_repeat() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        let dir = std::env::temp_dir();
        let actual = dir.join(format!("mp3tui_cola_actual_{}.wav", std::process::id()));
        let siguiente = dir.join(format!("mp3tui_cola_siguiente_{}.wav", std::process::id()));
        std::fs::write(&actual, make_test_wav(4410)).unwrap();
        std::fs::write(&siguiente, make_test_wav(4410)).unwrap();

        app.status = Status::Playing;
        app.repeat = RepeatMode::One;
        app.current_path = Some(actual.clone());
        let mut encolada = Track::from_path(siguiente.clone());
        encolada.title = "La que sigue".to_string();
        app.queue.push(QueueItem::Local(encolada));

        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(
            app.current_path.as_ref(),
            Some(&siguiente),
            "con la cola cargada el fin de pista reproduce el ítem encolado"
        );
        assert_eq!(dispatched, None, "no se re-lanza la pista que terminó");
        assert_eq!(
            app.queue_cursor,
            Some(0),
            "el cursor de la cola queda en el ítem que sonó (S9)"
        );
        assert_eq!(
            app.queue.len(),
            1,
            "el ítem no sale de la cola: sigue visible para poder volver con p"
        );

        let _ = std::fs::remove_file(&actual);
        let _ = std::fs::remove_file(&siguiente);
    }

    /// La cola conserva el orden de encolado también con shuffle activo, y el
    /// cursor avanza en vez de consumir la lista (S9).
    #[test]
    fn test_natural_end_with_queue_respects_enqueue_order_even_with_shuffle() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        let dir = std::env::temp_dir();
        let primera = dir.join(format!("mp3tui_cola_a_{}.wav", std::process::id()));
        let segunda = dir.join(format!("mp3tui_cola_b_{}.wav", std::process::id()));
        std::fs::write(&primera, make_test_wav(4410)).unwrap();
        std::fs::write(&segunda, make_test_wav(4410)).unwrap();

        app.status = Status::Playing;
        app.shuffle = true;
        let mut a = Track::from_path(primera.clone());
        a.title = "Primera".to_string();
        let mut b = Track::from_path(segunda.clone());
        b.title = "Segunda".to_string();
        app.queue.push(QueueItem::Local(a));
        app.queue.push(QueueItem::Local(b));

        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, _request| {});

        assert_eq!(
            app.current_path.as_ref(),
            Some(&primera),
            "con shuffle la cola igual respeta el orden de encolado"
        );
        assert_eq!(
            app.queue_cursor,
            Some(0),
            "el cursor queda en el primero: la cola no se consume"
        );
        assert_eq!(app.queue.len(), 2, "los dos ítems siguen en la cola");
        assert_eq!(
            app.queue.get(1).map(QueueItem::label),
            Some("Segunda"),
            "el segundo ítem espera su turno detrás del cursor"
        );

        let _ = std::fs::remove_file(&primera);
        let _ = std::fs::remove_file(&segunda);
    }

    /// Una URL encolada se resuelve recién al terminar la actual, sin salir de
    /// la cola: el cursor la marca como el ítem sonando.
    #[test]
    fn test_natural_end_with_queued_url_resolves_only_when_its_turn_comes() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        app.status = Status::Playing;
        app.is_url = true;
        app.queue.push(QueueItem::Url {
            url: "https://www.youtube.com/watch?v=encolada".to_string(),
            title: "Encolada".to_string(),
        });

        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(
            dispatched.map(|request| request.original_url),
            Some("https://www.youtube.com/watch?v=encolada".to_string()),
            "la URL encolada se resuelve cuando se termina la actual"
        );
        assert_eq!(app.queue_cursor, Some(0), "el cursor queda en la URL encolada");
        assert_eq!(app.queue.len(), 1, "la URL sigue en la cola");
    }

    /// D2/S6 con el shuffle activo: una pista local que termina encadena con
    /// la URL del historial que le sigue en el pool, pasando por el seam
    /// inyectado `replay` (no se lanza yt-dlp en el test). Con un pool de
    /// exactamente dos elementos el «siguiente» está forzado, así que no
    /// depende de la entropía del orden.
    #[test]
    fn test_shuffle_natural_end_local_chains_to_history_url_via_replay() {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_named("alpha"),
        ]));
        app.history.add(
            "Video de YouTube".to_string(),
            "https://www.youtube.com/watch?v=urlA".to_string(),
        );
        refresh_view(&mut app);
        app.shuffle = true;
        app.repeat = RepeatMode::All;
        app.is_url = false;
        app.current_path = Some(PathBuf::from(r"C:\x\alpha.mp3"));
        app.status = Status::Playing;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(
            dispatched.map(|r| r.original_url),
            Some("https://www.youtube.com/watch?v=urlA".to_string()),
            "con shuffle una pista local encadena a la URL del historial"
        );
    }

    /// D2/S6 con el shuffle activo: una URL que termina encadena a la OTRA
    /// URL del historial, nunca a la misma. Pool de dos URLs → el siguiente
    /// está forzado.
    #[test]
    fn test_shuffle_natural_end_url_chains_to_other_history_url() {
        let mut app = test_app();
        app.history.add(
            "A".to_string(),
            "https://www.youtube.com/watch?v=uA".to_string(),
        );
        app.history.add(
            "B".to_string(),
            "https://www.youtube.com/watch?v=uB".to_string(),
        );
        refresh_view(&mut app);
        app.shuffle = true;
        app.repeat = RepeatMode::All;
        app.is_url = true;
        app.current_url = Some((
            "https://www.youtube.com/watch?v=uA".to_string(),
            "A".to_string(),
        ));
        app.status = Status::Playing;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        let url = dispatched.map(|r| r.original_url);
        assert_eq!(
            url.as_deref(),
            Some("https://www.youtube.com/watch?v=uB"),
            "una URL terminada encadena a la otra URL del historial"
        );
        assert_ne!(
            url.as_deref(),
            Some("https://www.youtube.com/watch?v=uA"),
            "nunca vuelve a la URL que acaba de terminar"
        );
    }

    /// D2/S6: el shuffle no repite la pista que acaba de terminar. Con el
    /// orden forzado `[2, 0, 1]` y una pista local en el pool 0, el siguiente
    /// es determinista (el pool 1) y nunca vuelve a ser la pista que terminó.
    #[test]
    fn test_shuffle_natural_end_never_repeats_the_finished_track() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let pa = dir.join(format!("mp3tui_shuf_a_{}.wav", std::process::id()));
        let pb = dir.join(format!("mp3tui_shuf_b_{}.wav", std::process::id()));
        let pc = dir.join(format!("mp3tui_shuf_c_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        for p in [&pa, &pb, &pc] {
            std::fs::write(p, &wav).unwrap();
        }

        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(pa.clone(), "a"),
            track_with_path(pb.clone(), "b"),
            track_with_path(pc.clone(), "c"),
        ]));
        refresh_view(&mut app);
        app.shuffle = true;
        app.repeat = RepeatMode::Off;
        app.is_url = false;
        app.status = Status::Playing;
        // Orden explícito para que la prueba sea determinista (no entropía).
        app.shuffle_order = vec![2, 0, 1];
        app.current_path = Some(pa.clone());

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                for p in [&pa, &pb, &pc] {
                    let _ = std::fs::remove_file(p);
                }
                return;
            }
        };
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, _request| {});

        assert_ne!(
            app.current_path.as_ref(),
            Some(&pa),
            "el siguiente nunca es la pista que acaba de terminar"
        );
        assert_eq!(
            app.current_path.as_ref(),
            Some(&pb),
            "con el orden [2,0,1] y 'a' en el pool 0, el siguiente es 'b'"
        );

        for p in [&pa, &pb, &pc] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Preservación (S4): con el shuffle APAGADO el fin natural avanza a la
    /// siguiente pista local en orden, nunca a una URL del historial, y
    /// `n`/`p` se comportan igual que antes.
    #[test]
    fn test_shuffle_off_natural_end_and_steps_stay_natural_never_url() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let p1 = dir.join(format!("mp3tui_off1_{}.wav", std::process::id()));
        let p2 = dir.join(format!("mp3tui_off2_{}.wav", std::process::id()));
        let p3 = dir.join(format!("mp3tui_off3_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        for p in [&p1, &p2, &p3] {
            std::fs::write(p, &wav).unwrap();
        }

        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(p1.clone(), "uno"),
            track_with_path(p2.clone(), "dos"),
            track_with_path(p3.clone(), "tres"),
        ]));
        // Una URL del historial adelante de la vista: no debe saltarse nunca.
        app.history.add(
            "Video".to_string(),
            "https://www.youtube.com/watch?v=vX".to_string(),
        );
        refresh_view(&mut app);
        app.repeat = RepeatMode::All;
        app.is_url = false;
        app.status = Status::Playing;
        app.current_path = Some(p1.clone());

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                for p in [&p1, &p2, &p3] {
                    let _ = std::fs::remove_file(p);
                }
                return;
            }
        };
        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(dispatched, None, "sin shuffle el fin natural nunca dispara una URL");
        assert_eq!(
            app.current_path.as_ref(),
            Some(&p2),
            "el fin natural avanza a la siguiente pista local en orden"
        );

        app.current_path = Some(p2.clone());
        play_step(&mut app, &mut engine, 1);
        assert_eq!(app.current_path.as_ref(), Some(&p3), "'n' avanza a la siguiente local");

        app.current_path = Some(p3.clone());
        play_step(&mut app, &mut engine, -1);
        assert_eq!(app.current_path.as_ref(), Some(&p2), "'p' retrocede a la anterior local");

        for p in [&p1, &p2, &p3] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// D2/S6 con shuffle activo: `n`/`p` deben recorrer el mismo pool mixto
    /// que el fin de pista. Con un pool de exactamente dos elementos (una
    /// pista local + una URL del historial) el «siguiente» está forzado; el
    /// paso tiene que despachar la URL por el seam inyectado `replay` en vez
    /// de quedar en silencio.
    #[test]
    fn test_shuffle_step_dispatches_history_url_through_replay() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let wav_path = dir.join(format!("mp3tui_step_url_{}.wav", std::process::id()));
        std::fs::write(&wav_path, make_test_wav(4410)).unwrap();

        let local = track_with_path(wav_path.clone(), "local");
        // RED note: con play_step la URL no se despacha (quedaba en silencio);
        // el seam play_step_with la entrega al closure capturado.

        app.library = Some(crate::playlist::from_tracks_for_test(vec![local]));
        app.history.add(
            "Video de YouTube".to_string(),
            "https://www.youtube.com/watch?v=stepUrl".to_string(),
        );
        refresh_view(&mut app);
        app.shuffle = true;
        app.repeat = RepeatMode::All;
        app.current_path = Some(wav_path.clone());

        let mut captured: Option<UrlReplayRequest> = None;
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured = Some(request);
        });

        assert_eq!(
            captured.map(|r| r.original_url),
            Some("https://www.youtube.com/watch?v=stepUrl".to_string()),
            "con shuffle 'n' encadena la URL del historial por el seam de replay"
        );

        let _ = std::fs::remove_file(wav_path);
    }

    /// «Encolar» (`a`) suma la pista seleccionada de la Biblioteca como
    /// `QueueItem::Local`, sin mover la selección ni la lista.
    #[test]
    fn enqueue_key_appends_the_selected_library_track_to_the_queue() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.selected = 1;

        handle_list_key(theme_menu_key(KeyCode::Char('a')), &mut app, &mut engine);

        assert_eq!(app.queue.len(), 1, "una pista encolada");
        assert_eq!(app.queue.first().map(QueueItem::label), Some("beta"));
        assert_eq!(app.selected, 1, "la selección de la biblioteca no se mueve");
        assert_eq!(app.view.len(), 3, "la lista no cambia");
    }

    /// Una fila del historial se encola como URL (se resuelve cuando le toca
    /// sonar), no como pista local.
    #[test]
    fn enqueue_key_adds_a_history_url_as_a_queue_url_item() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.history.add(
            "Video de YouTube".to_string(),
            "https://www.youtube.com/watch?v=x".to_string(),
        );
        refresh_view(&mut app);
        app.selected = 0;

        handle_list_key(theme_menu_key(KeyCode::Char('a')), &mut app, &mut engine);

        assert_eq!(
            app.queue.first(),
            Some(&QueueItem::Url {
                url: "https://www.youtube.com/watch?v=x".to_string(),
                title: "Video de YouTube".to_string(),
            })
        );
    }

    /// `c` alterna Biblioteca y Cola dentro del panel; cada lista conserva su
    /// propia selección.
    #[test]
    fn queue_panel_key_toggles_lists_and_keeps_two_selections() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        app.selected = 2;
        app.queue.push(QueueItem::Local(track_named("uno")));
        app.queue.push(QueueItem::Local(track_named("dos")));

        handle_list_key(theme_menu_key(KeyCode::Char('c')), &mut app, &mut engine);
        assert!(app.queue_panel, "c muestra la Cola en el panel");
        assert_eq!(app.selected, 2, "la selección de Biblioteca no se toca");

        handle_list_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);
        assert_eq!(app.queue_selected, 1, "↓ mueve la selección de la Cola");
        assert_eq!(app.selected, 2, "y no la de Biblioteca");

        handle_list_key(theme_menu_key(KeyCode::Char('c')), &mut app, &mut engine);
        assert!(!app.queue_panel, "c vuelve a Biblioteca");
        assert_eq!(app.queue_selected, 1, "la Cola conserva su selección");
    }

    /// Lista de la Cola: el resaltado (selección) es el mismo que el de la
    /// Biblioteca, el ítem parado en el cursor de reproducción se marca con
    /// `▶`, lo ya sonado queda atenuado y el estado vacío es explícito (S9).
    #[test]
    fn queue_rows_mark_the_cursor_position_and_dim_the_played_items() {
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("primera")));
        app.queue.push(QueueItem::Url {
            url: "https://x/y".to_string(),
            title: "segunda".to_string(),
        });
        app.queue.push(QueueItem::Local(track_named("tercera")));
        app.queue_selected = 1;
        app.queue_cursor = Some(1);

        let lines = queue_rows(&app, 40, 10);
        let texts: Vec<String> = lines.iter().map(|line| line_text(line)).collect();
        assert_eq!(texts.len(), 3, "una fila por ítem: {texts:?}");
        assert!(texts[1].starts_with('\u{258c}'), "cursor de selección en el seleccionado");
        assert!(texts[0].contains("primera"), "{:?}", texts[0]);
        assert!(texts[2].contains("tercera"), "{:?}", texts[2]);

        assert_eq!(
            texts[0].chars().nth(1),
            Some(' '),
            "lo ya sonado no lleva el marcador de reproducción: {:?}",
            texts[0]
        );
        assert_eq!(
            lines[0].spans[2].style.fg,
            Some(theme().dim),
            "lo ya sonado se muestra atenuado: {:?}",
            texts[0]
        );
        assert_eq!(
            texts[1].chars().nth(1),
            Some('\u{25b6}'),
            "el ítem parado en el cursor de la cola se marca: {:?}",
            texts[1]
        );
        assert_eq!(
            lines[2].spans[2].style.fg,
            Some(theme().text),
            "lo que falta suena normal: {:?}",
            texts[2]
        );

        let empty = queue_rows(&test_app(), 40, 10);
        let empty_text = line_text(&empty[0]);
        assert!(empty_text.contains("vacía"), "estado vacío: {empty_text}");
    }

    /// S9: borrar un ítem que está antes del cursor lo corre para que siga
    /// apuntando al mismo ítem.
    #[test]
    fn queue_delete_before_the_cursor_keeps_it_on_the_same_item() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.explorer_open = true;
        app.queue_panel = true;
        app.queue.push(QueueItem::Local(track_named("uno")));
        app.queue.push(QueueItem::Local(track_named("dos")));
        app.queue.push(QueueItem::Local(track_named("tres")));
        app.queue_cursor = Some(2);
        app.queue_selected = 0;

        handle_list_key(theme_menu_key(KeyCode::Char('d')), &mut app, &mut engine);

        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(labels, vec!["dos", "tres"], "borra el seleccionado");
        assert_eq!(
            app.queue_cursor,
            Some(1),
            "el cursor sigue apuntando al mismo ítem (tres)"
        );
    }

    /// S9: borrar el ítem que está sonando desde la cola apaga el cursor,
    /// porque ya no hay nada de la cola en reproducción.
    #[test]
    fn queue_delete_of_the_playing_item_clears_the_cursor() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.explorer_open = true;
        app.queue_panel = true;
        app.queue.push(QueueItem::Local(track_named("uno")));
        app.queue.push(QueueItem::Local(track_named("dos")));
        app.queue_cursor = Some(1);
        app.queue_selected = 1;

        handle_list_key(theme_menu_key(KeyCode::Char('d')), &mut app, &mut engine);

        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(labels, vec!["uno"], "borra el que estaba sonando");
        assert_eq!(app.queue_cursor, None, "nada de la cola está sonando");
    }

    /// S9: el cursor de la cola nunca queda fuera de rango ni apuntando a un
    /// ítem inexistente, ni siquiera mezclando encolado, borrado, Enter, `n`,
    /// `p` y fin de pista en cualquier orden.
    #[test]
    fn queue_cursor_stays_in_range_through_mixed_operations() {
        let Some(mut engine) = engine_or_skip() else { return };
        let dir = std::env::temp_dir();
        let wav = make_test_wav(4410);
        let mut paths = Vec::new();
        for i in 0..5 {
            let path = dir.join(format!("mp3tui_cursor_{}_{}.wav", std::process::id(), i));
            std::fs::write(&path, &wav).unwrap();
            paths.push(path);
        }
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(
            paths
                .iter()
                .cloned()
                .map(|path| track_with_path(path, "t"))
                .collect(),
        ));
        refresh_view(&mut app);
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        app.queue_panel = true;
        app.status = Status::Playing;

        for i in 0..200usize {
            match i % 7 {
                0 => {
                    app.selected = i % app.view.len().max(1);
                    enqueue_selected(&mut app);
                }
                1 => play_step_with(&mut app, &mut engine, 1, |_app, _engine, _req| {}),
                2 => play_step_with(&mut app, &mut engine, -1, |_app, _engine, _req| {}),
                3 => handle_natural_end_with(&mut app, &mut engine, |_app, _engine, _req| {}),
                4 => {
                    app.queue_selected = (i * 3) % app.queue.len().max(1);
                    delete_selected_queue_item(&mut app);
                }
                5 => {
                    app.queue_selected = (i * 5) % app.queue.len().max(1);
                    play_selected_queue_item(&mut app, &mut engine);
                }
                _ => {
                    app.queue_cursor = match app.queue.len() {
                        0 => None,
                        len => Some((i * 7) % len),
                    };
                }
            }
            if let Some(cursor) = app.queue_cursor {
                assert!(
                    cursor < app.queue.len(),
                    "op {i}: cursor {cursor} fuera de rango (len {})",
                    app.queue.len()
                );
            }
            if app.queue.is_empty() {
                assert_eq!(app.queue_cursor, None, "cola vacía con cursor encendido");
            }
        }

        for path in &paths {
            let _ = std::fs::remove_file(path);
        }
    }

    /// S9: `n` recorre la cola —avanza el cursor una posición y reproduce ese
    /// ítem— antes de caer al pool de la biblioteca.
    #[test]
    fn queue_step_next_advances_the_cursor_and_plays_that_item() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.queue.push(queue_url("uno"));
        app.queue.push(queue_url("dos"));

        let mut captured: Vec<String> = Vec::new();
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured.push(request.original_url);
        });

        assert_eq!(
            captured,
            vec!["https://x/uno".to_string()],
            "n arranca por el primero de la cola"
        );
        assert_eq!(app.queue_cursor, Some(0), "el cursor queda en el ítem que sonó");
        assert_eq!(app.queue.len(), 2, "la cola conserva los ítems");

        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured.push(request.original_url);
        });
        assert_eq!(
            captured,
            vec!["https://x/uno".to_string(), "https://x/dos".to_string()],
            "n vuelve a avanzar el cursor"
        );
        assert_eq!(app.queue_cursor, Some(1));
        assert_eq!(app.queue.len(), 2, "y la cola sigue entera");
    }

    /// S9: `p` retrocede una posición del cursor y reproduce la anterior.
    #[test]
    fn queue_step_prev_goes_back_to_the_previous_queue_item() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.queue.push(queue_url("uno"));
        app.queue.push(queue_url("dos"));
        app.queue.push(queue_url("tres"));
        app.queue_cursor = Some(2);

        let mut captured: Option<UrlReplayRequest> = None;
        play_step_with(&mut app, &mut engine, -1, |_app, _engine, request| {
            captured = Some(request);
        });

        assert_eq!(
            captured.map(|request| request.original_url),
            Some("https://x/dos".to_string()),
            "p reproduce la anterior al cursor"
        );
        assert_eq!(app.queue_cursor, Some(1), "el cursor retrocede");
        assert_eq!(app.queue.len(), 3, "la cola no se consume");
    }

    /// S9: sin nada por delante del cursor, `n` cae al pool de la biblioteca
    /// como hoy (repeat Off avanza a la siguiente) y no mueve el cursor.
    #[test]
    fn queue_step_next_at_the_end_of_the_cursor_falls_back_to_the_pool() {
        let Some(mut engine) = engine_or_skip() else { return };
        let (mut app, t1, t2) = queue_fallback_app();
        app.current_path = Some(t1);
        app.queue.push(queue_url("uno"));
        app.queue_cursor = Some(0);

        let mut captured: Option<UrlReplayRequest> = None;
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured = Some(request);
        });

        assert_eq!(captured, None, "no hay nada después del cursor en la cola");
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "sin nada en la cola, n sigue avanzando la biblioteca como hoy"
        );
        assert_eq!(
            app.queue_cursor,
            Some(0),
            "el cursor de la cola no se mueve al caer al pool"
        );
        assert_eq!(app.queue.len(), 1, "la cola queda intacta");

        let _ = std::fs::remove_file(&t2);
    }

    /// S9: el fin natural con el cursor ya en el final de la cola vuelve al
    /// fallback de S5/S6 (la biblioteca avanza como hoy).
    #[test]
    fn queue_natural_end_with_the_cursor_at_the_end_falls_back_to_the_pool() {
        let (mut app, t1, t2) = queue_fallback_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        app.current_path = Some(t1);
        app.status = Status::Playing;
        app.queue.push(queue_url("uno"));
        app.queue_cursor = Some(0);

        let mut captured: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            captured = Some(request);
        });

        assert_eq!(captured, None, "la cola ya no tiene nada por delante");
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "el fin natural sigue avanzando la biblioteca"
        );
        assert_eq!(app.queue_cursor, Some(0), "el cursor de la cola no se mueve");
        assert_eq!(app.queue.len(), 1, "la cola queda intacta");

        let _ = std::fs::remove_file(&t2);
    }

    /// S13 extendido al fin natural: un ítem local de la cola que no abre en
    /// el fin de pista se saltea con aviso visible y se sigue con el próximo.
    #[test]
    fn queue_natural_end_skips_a_broken_local_with_a_visible_warning() {
        let (mut app, t1, t2) = queue_fallback_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        app.status = Status::Playing;
        app.current_path = Some(t1);
        let roto = if cfg!(windows) {
            PathBuf::from(r"C:\mp3tui_no_existe.wav")
        } else {
            PathBuf::from("/nonexistent/mp3tui_no_existe.wav")
        };
        // Un ítem roto (path inexistente) y detrás una pista real.
        app.queue.push(QueueItem::Local(track_with_path(roto, "roto")));
        app.queue.push(QueueItem::Local(track_with_path(t2.clone(), "buena")));

        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, _request| {});

        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "el fin natural se saltea el ítem roto y sigue con la siguiente"
        );
        assert_eq!(app.queue_cursor, Some(1), "el cursor pasa el ítem roto");
        assert!(
            app.error.as_deref().is_some_and(|msg| msg.contains("se saltea")),
            "el salto deja un aviso visible: {:?}",
            app.error
        );

        let _ = std::fs::remove_file(&t2);
    }

    /// S9: sin cursor de cola todavía, `p` sigue retrocediendo el pool como
    /// hoy (la cola no tiene «anterior» desde donde está parado).
    #[test]
    fn queue_step_prev_without_a_cursor_falls_back_to_the_pool() {
        let Some(mut engine) = engine_or_skip() else { return };
        let (mut app, t1, t2) = queue_fallback_app();
        app.current_path = Some(t2.clone());
        app.queue.push(queue_url("uno"));

        let mut captured: Option<UrlReplayRequest> = None;
        play_step_with(&mut app, &mut engine, -1, |_app, _engine, request| {
            captured = Some(request);
        });

        assert_eq!(captured, None, "sin cursor no hay ítem de cola anterior");
        assert_eq!(app.queue_cursor, None, "el cursor sigue apagado");
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "p sigue retrocediendo la biblioteca como hoy"
        );
        assert_eq!(app.queue.len(), 1, "la cola queda intacta");

        let _ = std::fs::remove_file(&t2);
    }

    /// Enter en la Cola salta y reproduce ya el ítem seleccionado, sin sacarlo
    /// de la cola: queda marcado como el actual para poder volver con `p` (S9).
    #[test]
    fn queue_enter_plays_the_selected_item_and_moves_the_cursor() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mp3tui_cola_enter_{}.wav", std::process::id()));
        std::fs::write(&path, make_test_wav(4410)).unwrap();

        app.explorer_open = true;
        app.queue_panel = true;
        app.queue.push(QueueItem::Local(track_with_path(path.clone(), "elegida")));
        app.queue.push(QueueItem::Local(track_named("queda")));

        handle_list_key(theme_menu_key(KeyCode::Enter), &mut app, &mut engine);

        assert_eq!(app.current_path.as_ref(), Some(&path), "Enter reproduce ya");
        assert_eq!(app.queue_cursor, Some(0), "el cursor queda en el elegido");
        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(
            labels,
            vec!["elegida", "queda"],
            "el ítem reproducido sigue en la cola"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// `d` borra el ítem seleccionado de la Cola y la selección cae en el
    /// siguiente.
    #[test]
    fn queue_delete_key_removes_the_selected_item() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.explorer_open = true;
        app.queue_panel = true;
        app.queue.push(QueueItem::Local(track_named("uno")));
        app.queue.push(QueueItem::Local(track_named("dos")));
        app.queue.push(QueueItem::Local(track_named("tres")));
        app.queue_selected = 1;

        handle_list_key(theme_menu_key(KeyCode::Char('d')), &mut app, &mut engine);

        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(labels, vec!["uno", "tres"], "borra el seleccionado");
        assert_eq!(app.queue_selected, 1, "la selección cae en el siguiente");
    }

    /// PRESERVE: con la lista Biblioteca activa, las flechas siguen moviendo
    /// `selected` (no la selección de la Cola).
    #[test]
    fn library_list_keys_keep_moving_the_library_selection() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = library_with_three();
        app.explorer_open = true;
        app.selected = 0;
        app.queue_selected = 0;

        handle_list_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);
        assert_eq!(app.selected, 1, "↓ mueve la Biblioteca");
        handle_list_key(theme_menu_key(KeyCode::End), &mut app, &mut engine);
        assert_eq!(app.selected, 2, "Fin va a la última de la Biblioteca");
        assert_eq!(app.queue_selected, 0, "la Cola no se toca");
    }

    #[test]
    fn test_local_natural_end_filtered_out_of_view_stops_without_restarting() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t11_hide1_{}.wav", std::process::id()));
        let t2 = dir.join(format!("mp3tui_t11_hide2_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);
        let _ = std::fs::write(&t2, &wav);
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(t1.clone(), "track_1"),
            track_with_path(t2.clone(), "track_2"),
        ]));
        refresh_view(&mut app);
        app.repeat = RepeatMode::Off;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                let _ = std::fs::remove_file(&t2);
                return;
            }
        };

        // La pista 1 está sonando...
        app.selected = 0;
        play_selected(&mut app, &mut engine);
        assert_eq!(app.current_path.as_ref(), Some(&t1));

        // ...y un filtro la deja FUERA de la vista visible.
        app.filter = Some("track_2".to_string());
        refresh_view(&mut app);
        assert_eq!(app.view.len(), 1, "setup: solo track_2 visible");
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "setup: sigue sonando track_1 aunque esté oculto"
        );
        let selected_before = app.selected;

        handle_natural_end(&mut app, &mut engine);

        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "fuera de la vista no debe reiniciar desde la primera pista visible"
        );
        assert_eq!(
            app.selected, selected_before,
            "la selección no debe moverse"
        );
        assert!(app.error.is_none(), "{:?}", app.error);
        assert!(app.ytdlp_rx.is_none(), "sin yt-dlp en el test");

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    #[test]
    fn test_local_natural_end_repeat_off_advances_then_stops_at_the_end() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t1_offadv1_{}.wav", std::process::id()));
        let t2 = dir.join(format!("mp3tui_t1_offadv2_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);
        let _ = std::fs::write(&t2, &wav);
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(t1.clone(), "track_1"),
            track_with_path(t2.clone(), "track_2"),
        ]));
        refresh_view(&mut app);
        app.repeat = RepeatMode::Off;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                let _ = std::fs::remove_file(&t2);
                return;
            }
        };

        app.selected = 0;
        play_selected(&mut app, &mut engine);
        assert_eq!(app.current_path.as_ref(), Some(&t1));

        // En el medio de la lista: Off avanza a la siguiente.
        handle_natural_end(&mut app, &mut engine);
        assert_eq!(app.selected, 1, "Off debe avanzar en el medio");
        assert_eq!(app.current_path.as_ref(), Some(&t2));

        // En la última pista: Off no envuelve, se queda donde está.
        handle_natural_end(&mut app, &mut engine);
        assert_eq!(
            app.selected, 1,
            "Off en la última pista no debe envolver ni mover la selección"
        );
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "Off en la última pista no debe reemplazar la pista actual"
        );
        assert!(app.error.is_none(), "{:?}", app.error);

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    #[test]
    fn test_playing_library_track_invalidates_pending_ytdlp_resolution() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t61_inv1_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);

        let trk1 = track_with_path(t1.clone(), "track_1");

        app.library = Some(crate::playlist::from_tracks_for_test(vec![trk1]));
        refresh_view(&mut app);
        app.selected = 0;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                return;
            }
        };

        // 1. Submit a page URL: resolution is now pending. El resolver se
        // inyecta como doble: captura la orden pero NO lanza yt-dlp ni toca
        // la red desde el test.
        let page_url = "https://youtube.com/watch?v=abc".to_string();
        let mut resolved_url: Option<String> = None;
        submit_url_prompt_with(page_url.clone(), &mut app, &mut engine, |url, _req_id| {
            resolved_url = Some(url);
            let (_tx, rx) = mpsc::channel();
            rx
        });
        assert_eq!(
            resolved_url.as_deref(),
            Some(page_url.as_str()),
            "el resolver debe recibir la URL de la página"
        );
        assert!(app.ytdlp_rx.is_some(), "resolution should be pending");
        let initial_req_id = app.ytdlp_request_id;
        assert_eq!(app.status, Status::Resolving);

        // 2. User plays a library track before resolution finishes
        play_selected(&mut app, &mut engine);

        // La resolución pendiente se descarta y el request se renueva.
        assert!(app.ytdlp_rx.is_none(), "pending resolution must be cleared");
        assert_ne!(app.ytdlp_request_id, initial_req_id, "request id must be bumped");
        assert_ne!(app.status, Status::Resolving, "status must not stay Resolving");

        // 3. Late arrival of old resolution result must be ignored
        let stale_result = Ok(crate::source::ytdlp::Resolved {
            title: "Stale Video".to_string(),
            audio_url: "https://example.com/stale.m4a".to_string(),
        });
        let action = handle_ytdlp_result(&mut app, initial_req_id, stale_result);
        assert_eq!(action, None, "stale ytdlp result must be rejected");

        let _ = std::fs::remove_file(&t1);
    }

    #[test]
    fn test_row_building_with_history_and_local_groups() {
        let mut app = library_with_three();
        app.history.add("Historial Cancion 1".to_string(), "https://example.com/song1.mp3".to_string());
        refresh_view(&mut app);

        assert_eq!(app.view.len(), 4);
        assert_eq!(app.view[0], ViewItem::History(0));
        assert_eq!(app.view[1], ViewItem::Track(0));

        let lines = library_rows(&app, 60, 10);
        let has_history_header = lines.iter().any(|l| {
            l.spans.iter().any(|s| s.content.contains("Agregadas por URL"))
        });
        assert!(has_history_header, "debe contener encabezado 'Agregadas por URL'");

        let has_history_badge = lines.iter().any(|l| {
            l.spans.iter().any(|s| s.content.contains("[URL] "))
        });
        assert!(has_history_badge, "fila de historial debe tener badge '[URL] '");

        let has_history_title = lines.iter().any(|l| {
            l.spans.iter().any(|s| s.content.contains("Historial Cancion 1"))
        });
        assert!(has_history_title, "fila de historial debe mostrar el título");
    }

    #[test]
    fn test_navigation_crossing_boundary_between_history_and_local() {
        let mut app = library_with_three();
        app.history.add("Historial Cancion 1".to_string(), "https://example.com/song1.mp3".to_string());
        refresh_view(&mut app);

        assert_eq!(app.view[0], ViewItem::History(0));
        assert_eq!(app.view[1], ViewItem::Track(0));

        app.selected = 0;
        app.selected = move_selection(app.selected, 1, app.view.len());
        assert_eq!(app.selected, 1);
        assert_eq!(app.view[app.selected], ViewItem::Track(0));

        app.selected = move_selection(app.selected, -1, app.view.len());
        assert_eq!(app.selected, 0);
        assert_eq!(app.view[app.selected], ViewItem::History(0));
    }

    #[test]
    fn test_search_filters_history_by_title_and_local_tracks() {
        let mut app = library_with_three();
        app.history.add("Bohemian Rhapsody".to_string(), "https://example.com/queen.mp3".to_string());
        app.history.add("Yesterday".to_string(), "https://example.com/beatles.mp3".to_string());
        refresh_view(&mut app);

        assert_eq!(app.view.len(), 5);

        app.filter = Some("bohemian".to_string());
        refresh_view(&mut app);
        assert_eq!(app.view.len(), 1);
        assert_eq!(app.view[0], ViewItem::History(1));

        app.filter = Some("beta".to_string());
        refresh_view(&mut app);
        assert_eq!(app.view.len(), 1);
        assert_eq!(app.view[0], ViewItem::Track(1));
    }

    #[test]
    fn test_enter_on_history_row_dispatches_playback() {
        let mut app = library_with_three();
        // URL local vía servidor de loopback: la fila del historial reproduce
        // sin salir a la red externa (el label sigue siendo el filename).
        let url = local_wav_url("stream.mp3", &make_test_wav(4410));
        app.history.add("Historial Cancion".to_string(), url);
        refresh_view(&mut app);

        app.selected = 0;
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        play_selected(&mut app, &mut engine);

        assert_eq!(app.mode, Mode::Player);
        assert!(app.is_url);
        assert_eq!(app.track_label, "stream.mp3");
    }

    #[test]
    fn test_history_recording_happens_only_on_successful_start() {
        let mut app = test_app();
        assert_eq!(app.history.len(), 0);

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        // Reproduce una URL local de loopback: sin red externa, y la entrada
        // registrada debe ser exactamente esa URL.
        let url = local_wav_url("audio.mp3", &make_test_wav(4410));
        start_url_playback(
            &mut app,
            &mut engine,
            url.clone(),
            url.clone(),
            "audio.mp3".to_string(),
        );

        assert_eq!(app.status, Status::Connecting);
        assert_eq!(app.history.len(), 0);
        assert!(app.pending_history.is_some());

        let snapshot_playing = EngineSnapshot {
            is_connecting: false,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        apply_engine_snapshot(&mut app, snapshot_playing);

        assert_eq!(app.status, Status::Playing);
        assert_eq!(app.history.len(), 1);
        assert_eq!(
            app.history.get(0).unwrap().url,
            url,
            "la entrada registrada es la URL local que se reprodujo"
        );
        assert_eq!(app.history.get(0).unwrap().title, "audio.mp3");
        assert!(app.pending_history.is_none());
    }

    #[test]
    fn test_url_playback_error_keeps_history_empty() {
        let mut app = test_app();
        init_url_playback(&mut app, "https://youtube.com/watch?v=123".to_string(), "My Song".to_string());
        assert_eq!(app.status, Status::Connecting);

        let snapshot_error = EngineSnapshot {
            is_connecting: false,
            is_playing: false,
            is_paused: false,
            had_playback_error: true,
        };
        apply_engine_snapshot(&mut app, snapshot_error);
        assert_eq!(app.status, Status::Stopped);
        assert!(app.history.is_empty(), "history must stay empty on playback error");
        assert!(app.pending_history.is_none());
    }

    #[test]
    fn test_url_playback_connecting_keeps_history_empty_until_playing() {
        let mut app = test_app();
        init_url_playback(&mut app, "https://youtube.com/watch?v=123".to_string(), "My Song".to_string());

        let snapshot_connecting = EngineSnapshot {
            is_connecting: true,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        apply_engine_snapshot(&mut app, snapshot_connecting);
        assert_eq!(app.status, Status::Connecting);
        assert!(app.history.is_empty(), "history must be empty while connecting");

        let snapshot_playing = EngineSnapshot {
            is_connecting: false,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        apply_engine_snapshot(&mut app, snapshot_playing);
        assert_eq!(app.status, Status::Playing);
        assert_eq!(app.history.len(), 1, "exactly one history entry recorded on playing");
        assert_eq!(app.history.get(0).unwrap().url, "https://youtube.com/watch?v=123");
        assert_eq!(app.history.get(0).unwrap().title, "My Song");
        assert!(app.pending_history.is_none());
    }

    #[test]
    fn test_superseded_url_playback_is_never_recorded() {
        let mut app = test_app();
        init_url_playback(&mut app, "https://example.com/A.mp3".to_string(), "Track A".to_string());
        init_url_playback(&mut app, "https://example.com/B.mp3".to_string(), "Track B".to_string());

        let snapshot_playing = EngineSnapshot {
            is_connecting: false,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        apply_engine_snapshot(&mut app, snapshot_playing);
        assert_eq!(app.status, Status::Playing);
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history.get(0).unwrap().url, "https://example.com/B.mp3");
        assert!(
            !app.history.entries().iter().any(|e| e.url == "https://example.com/A.mp3"),
            "URL A must never be recorded"
        );
    }

    #[test]
    fn test_ui_app_uses_isolated_temp_history_path() {
        let app = test_app();
        let hist_path = app.history_path.as_ref().expect("history_path must be set");
        assert!(
            hist_path.starts_with(std::env::temp_dir()),
            "history_path must be in temp_dir, got: {}",
            hist_path.display()
        );
        assert_ne!(
            Some(hist_path.clone()),
            crate::history::history_path(),
            "must not be real user path"
        );
    }

    #[test]
    fn test_record_history_preserves_selected_history_item() {
        let mut app = library_with_three();
        app.history.add("Song 1".to_string(), "https://example.com/1.mp3".to_string());
        app.history.add("Song 2".to_string(), "https://example.com/2.mp3".to_string());
        refresh_view(&mut app);

        // View has History(0) ("Song 2") and History(1) ("Song 1")
        assert_eq!(app.view[0], ViewItem::History(0));
        assert_eq!(app.view[1], ViewItem::History(1));
        app.selected = 1; // User selected Song 1 (2nd row)

        record_history_entry(&mut app, "https://example.com/3.mp3", "Song 3");

        // Song 3 was added at top. Song 1 is now at index 2.
        // The selection must remain on Song 1!
        assert_eq!(app.view[app.selected], ViewItem::History(2));
        assert_eq!(app.history.get(2).unwrap().title, "Song 1");
    }

    #[test]
    fn test_record_history_strips_userinfo() {
        let mut app = test_app();
        record_history_entry(&mut app, "http://user:pass@example.com/secret.mp3", "Secret");
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history.get(0).unwrap().url, "http://example.com/secret.mp3");
    }

    #[test]
    fn progress_cells_keeps_today_played_tip_and_gray_track() {
        // Sin secciones precargadas: exactamente el look actual de la barra
        // (Played…, Tip, Empty…), para que lo local no cambie.
        assert_eq!(
            progress_bar_cells(10, 0.5, &[]),
            vec![
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Tip,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Empty,
            ]
        );
    }

    #[test]
    fn progress_cells_paints_buffered_sections_and_keeps_gaps() {
        // Bounds exactamente representables en f32 para evitar que 0.3/0.7
        // toquen la celda del borde (0.3f32 > 0.3 → cubre la celda 3).
        let cells = progress_bar_cells(10, 0.0, &[(0.0, 0.25), (0.75, 1.0)]);
        assert_eq!(
            cells,
            vec![
                BarCellKind::Buffered,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
            ]
        );
    }

    #[test]
    fn progress_cells_played_wins_over_buffered() {
        // Las celdas ya reproducidas conservan su estilo aunque un rango las cubra.
        let cells = progress_bar_cells(10, 0.6, &[(0.0, 1.0)]);
        assert_eq!(
            cells,
            vec![
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Tip,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
            ]
        );
    }

    #[test]
    fn progress_cells_tiny_range_paints_the_cell_it_touches() {
        // Rango diminuto no vacío: pinta al menos la celda que toca (0.505 cae
        // dentro de la celda 50 en un ancho 100). Rango degenerado (e <= s): nada.
        let cells = progress_bar_cells(100, 0.0, &[(0.505, 0.5055)]);
        let mut expected = vec![BarCellKind::Empty; 100];
        expected[50] = BarCellKind::Buffered;
        assert_eq!(cells, expected);

        let cells = progress_bar_cells(100, 0.0, &[(0.5, 0.5)]);
        assert!(
            cells.iter().all(|&k| k == BarCellKind::Empty),
            "un rango vacío no debe pintar ninguna celda"
        );
    }

    #[test]
    fn progress_cells_clamps_and_never_panics() {
        // Ancho 0 → sin celdas; ancho 1 → una sola celda bien resuelta.
        assert!(progress_bar_cells(0, 0.5, &[(0.0, 1.0)]).is_empty());
        assert_eq!(
            progress_bar_cells(1, 0.0, &[(0.0, 1.0)]),
            vec![BarCellKind::Buffered]
        );
        assert_eq!(progress_bar_cells(1, 1.0, &[]), vec![BarCellKind::Played]);

        // Fracciones fuera de [0,1] se clampan.
        assert_eq!(
            progress_bar_cells(10, 1.5, &[(0.0, 1.0)]),
            vec![BarCellKind::Played; 10],
            "played > 1 se clampa a barra llena"
        );
        let cells = progress_bar_cells(10, 0.0, &[(-0.5, 0.05), (0.95, 1.5)]);
        let mut expected = vec![BarCellKind::Empty; 10];
        expected[0] = BarCellKind::Buffered;
        expected[9] = BarCellKind::Buffered;
        assert_eq!(cells, expected, "los rangos se clampan a [0,1]");
    }

    // Seams de test para resolución y listado (no lanzan yt-dlp ni red).
    fn empty_resolve_rx(urls: &std::cell::RefCell<Vec<String>>) -> impl FnMut(String, u64) -> Receiver<YtDlpMessage> + '_ {
        move |url: String, _req_id: u64| {
            urls.borrow_mut().push(url);
            let (_tx, rx) = mpsc::channel();
            rx
        }
    }

    fn empty_list_rx(urls: &std::cell::RefCell<Vec<String>>) -> impl FnMut(String, u64) -> Receiver<YtDlpListMessage> + '_ {
        move |url: String, _req_id: u64| {
            urls.borrow_mut().push(url);
            let (_tx, rx) = mpsc::channel();
            rx
        }
    }

    fn playlist_entry(title: &str, id: &str) -> crate::source::ytdlp::PlaylistEntry {
        crate::source::ytdlp::PlaylistEntry {
            title: title.to_string(),
            page_url: format!("https://www.youtube.com/watch?v={id}"),
        }
    }

    /// S12: al listar una playlist, la primera canción se resuelve enseguida y
    /// el resto entra a la cola AL FINAL de la existente, sin tocar el cursor.
    #[test]
    fn playlist_submit_enqueues_rest_at_end_and_resolves_first() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("previa")));

        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        let playlist_url = "https://www.youtube.com/playlist?list=PLxyz".to_string();
        submit_url_prompt_with_seams(
            playlist_url.clone(),
            &mut app,
            &mut engine,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );

        assert_eq!(app.status, Status::Resolving);
        assert!(app.playlist_rx.is_some(), "arranca el listado de playlist");
        assert!(app.ytdlp_rx.is_none(), "todavía no resuelve ningún video");
        assert_eq!(
            app.current_url.as_ref().map(|(u, _)| u.as_str()),
            Some(playlist_url.as_str()),
            "la sesión guarda la URL de la playlist para la historia"
        );
        assert_eq!(list_calls.borrow().as_slice(), &[playlist_url.as_str()]);

        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req_id = app.ytdlp_request_id;
        handle_playlist_result(
            &mut app,
            req_id,
            Ok(vec![
                playlist_entry("Primera", "a"),
                playlist_entry("Segunda", "b"),
                playlist_entry("Tercera", "c"),
            ]),
            &mut resolve_seam,
        );

        // El resto va al final de la cola existente (D7), sin tocar el cursor.
        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(labels, vec!["previa", "Segunda", "Tercera"]);
        assert_eq!(app.queue_cursor, None, "el cursor de la cola no cambia");
        // La primera se resuelve ahora, con su URL de página.
        assert!(app.ytdlp_rx.is_some());
        assert_eq!(
            resolve_calls.borrow().as_slice(),
            &["https://www.youtube.com/watch?v=a"]
        );
        assert_eq!(app.status, Status::Resolving);
    }

    /// S12: una entrada del listado sin título (video privado/eliminado)
    /// entra a la cola mostrando su URL de página como etiqueta.
    #[test]
    fn playlist_submit_uses_the_page_url_as_label_for_untitled_entries() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let playlist_url = "https://www.youtube.com/playlist?list=PLxyz".to_string();
        submit_url_prompt_with_seams(
            playlist_url.clone(),
            &mut app,
            &mut engine,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req_id = app.ytdlp_request_id;
        handle_playlist_result(
            &mut app,
            req_id,
            Ok(vec![
                playlist_entry("Primera", "a"),
                playlist_entry("", "b"),
                playlist_entry("Tercera", "c"),
            ]),
            &mut resolve_seam,
        );
        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(
            labels,
            vec!["https://www.youtube.com/watch?v=b", "Tercera"],
            "el título vacío se muestra como su URL de página"
        );
    }

    /// S12: una lista vacía avisa visiblemente y NO toca la cola ni arranca la
    /// primera.
    #[test]
    fn playlist_submit_empty_list_warns_and_leaves_queue_untouched() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("previa")));

        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        submit_url_prompt_with_seams(
            "https://www.youtube.com/playlist?list=vacia".to_string(),
            &mut app,
            &mut engine,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );

        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req_id = app.ytdlp_request_id;
        handle_playlist_result(&mut app, req_id, Ok(vec![]), &mut resolve_seam);

        assert_eq!(
            app.queue.iter().map(QueueItem::label).collect::<Vec<_>>(),
            vec!["previa"],
            "la cola queda intacta"
        );
        assert_eq!(app.queue_cursor, None);
        assert_eq!(app.error.as_deref(), Some(PLAYLIST_EMPTY_MSG));
        assert!(app.ytdlp_rx.is_none(), "no arranca la primera");
        assert_eq!(resolve_calls.borrow().len(), 0);
    }

    /// S12: un listado que falla avisa visiblemente y NO toca la cola.
    #[test]
    fn playlist_submit_listing_failure_warns_and_leaves_queue_untouched() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("previa")));

        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        submit_url_prompt_with_seams(
            "https://www.youtube.com/playlist?list=PLfail".to_string(),
            &mut app,
            &mut engine,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );

        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req_id = app.ytdlp_request_id;
        handle_playlist_result(
            &mut app,
            req_id,
            Err(crate::source::ytdlp::YtDlpError::Failed("ERROR: boom".to_string())),
            &mut resolve_seam,
        );

        assert_eq!(
            app.queue.iter().map(QueueItem::label).collect::<Vec<_>>(),
            vec!["previa"],
            "la cola queda intacta"
        );
        assert_eq!(app.error.as_deref(), Some("error al ejecutar yt-dlp: ERROR: boom"));
        assert!(app.ytdlp_rx.is_none());
    }

    /// S12: la resolución de la primera que falla es el error normal de URL
    /// (aviso visible) y la cola queda con lo encolado.
    #[test]
    fn playlist_first_entry_resolution_failure_keeps_queued_rest() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("previa")));

        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        submit_url_prompt_with_seams(
            "https://www.youtube.com/playlist?list=PLx".to_string(),
            &mut app,
            &mut engine,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req_id = app.ytdlp_request_id;
        handle_playlist_result(
            &mut app,
            req_id,
            Ok(vec![playlist_entry("Primera", "a"), playlist_entry("Segunda", "b")]),
            &mut resolve_seam,
        );

        // La primera no resuelve: error visible y la cola queda intacta.
        let outcome = handle_ytdlp_result(
            &mut app,
            req_id,
            Err(crate::source::ytdlp::YtDlpError::Failed("ERROR: nope".to_string())),
        );
        assert_eq!(outcome, None);
        assert!(app.error.is_some());
        assert_eq!(
            app.queue.iter().map(QueueItem::label).collect::<Vec<_>>(),
            vec!["previa", "Segunda"],
            "la cola queda con lo encolado"
        );
        assert_eq!(app.queue_resolving, None, "no es una URL encolada: sin salto");
    }

    /// S12: el arranque por argumento de CLI (boot) pasa por el mismo flujo
    /// unificado y clasifica la playlist como tal.
    #[test]
    fn cli_boot_path_unifies_playlist_flow() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        let playlist_url = "https://www.youtube.com/playlist?list=PLboot".to_string();
        start_boot_url_with(
            &mut app,
            &mut engine,
            playlist_url.clone(),
            crate::source::UrlKind::Playlist,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );

        assert!(app.playlist_rx.is_some());
        assert_eq!(app.status, Status::Resolving);
        assert_eq!(
            app.current_url.as_ref().map(|(u, _)| u.as_str()),
            Some(playlist_url.as_str())
        );
        assert_eq!(list_calls.borrow().as_slice(), &[playlist_url.as_str()]);

        // Y un Page por argumento va por la resolución normal (seam unificado).
        let mut page_app = test_app();
        let page_resolve = std::cell::RefCell::new(Vec::<String>::new());
        let page_list = std::cell::RefCell::new(Vec::<String>::new());
        let page_url = "https://www.youtube.com/watch?v=xyz".to_string();
        start_boot_url_with(
            &mut page_app,
            &mut engine,
            page_url.clone(),
            crate::source::UrlKind::Page,
            empty_resolve_rx(&page_resolve),
            empty_list_rx(&page_list),
        );
        assert!(page_app.ytdlp_rx.is_some());
        assert!(page_app.playlist_rx.is_none());
        assert_eq!(page_resolve.borrow().as_slice(), &[page_url.as_str()]);
        assert!(page_list.borrow().is_empty(), "un Page no lista");
    }

    /// S12: un listado que llega fuera de pedido (request id viejo) se
    /// descarta sin tocar la cola ni arrancar la primera.
    #[test]
    fn stale_playlist_listing_result_is_dropped() {
        let (mut app, t1, _t2) = queue_fallback_app();
        app.ytdlp_request_id = 99;
        app.queue.push(QueueItem::Local(track_named("previa")));

        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        handle_playlist_result(
            &mut app,
            98,
            Ok(vec![playlist_entry("Uno", "1"), playlist_entry("Dos", "2")]),
            &mut resolve_seam,
        );

        assert_eq!(
            app.queue.iter().map(QueueItem::label).collect::<Vec<_>>(),
            vec!["previa"],
            "el listado viejo no encola"
        );
        assert_eq!(app.queue_cursor, None);
        assert_eq!(resolve_calls.borrow().len(), 0, "no arranca la primera");

        let _ = std::fs::remove_file(&t1);
    }

    /// PRESERVE: la URL de página normal sigue resolviéndose en el mismo flujo
    /// de siempre, sin tocar la cola ni el listado.
    #[test]
    fn preserve_single_url_submit_resolves_without_queue_changes() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("unica")));
        let page_url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string();

        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        submit_url_prompt_with(page_url.clone(), &mut app, &mut engine, empty_resolve_rx(&resolve_calls));

        assert!(app.ytdlp_rx.is_some(), "la página se resuelve");
        assert!(app.playlist_rx.is_none(), "no lista");
        assert_eq!(app.status, Status::Resolving);
        assert_eq!(app.queue_cursor, None);
        assert_eq!(
            app.queue.iter().map(QueueItem::label).collect::<Vec<_>>(),
            vec!["unica"],
            "la cola no cambia"
        );
        assert_eq!(resolve_calls.borrow().as_slice(), &[page_url.as_str()]);
    }

    /// DIAGNÓSTICO TEMPORAL: dump del frame con el panel abierto, meta + cola
    /// Regresión del reporte del usuario: con el panel lateral abierto (lista
    /// Biblioteca), `n`/`N` saltan a la siguiente pista y `p` a la anterior,
    /// igual que sin panel (los atajos de reproducción quedan activos sobre la
    /// lista).
    #[test]
    fn list_mode_step_keys_change_tracks_with_the_panel_open() {
        let Some(mut engine) = engine_or_skip() else { return };
        let (mut app, t1, t2) = queue_fallback_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        app.selected = 0;
        play_selected(&mut app, &mut engine);
        assert_eq!(app.current_path.as_ref(), Some(&t1));

        handle_list_key(theme_menu_key(KeyCode::Char('n')), &mut app, &mut engine);
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "n cambia a la siguiente con el panel abierto"
        );

        // Mayúsculas valen igual (el matcher ignora mayúsculas); con repeat
        // Off en la última pista n no envuelve.
        handle_list_key(theme_menu_key(KeyCode::Char('N')), &mut app, &mut engine);
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "N no envuelve con repeat Off en la última"
        );

        handle_list_key(theme_menu_key(KeyCode::Char('p')), &mut app, &mut engine);
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "p retrocede con el panel abierto"
        );

        let _ = std::fs::remove_file(&t2);
    }

    /// Con la solapa Cola abierta, `n` avanza la cola (el cursor) desde donde
    /// está parado, igual que sin panel.
    #[test]
    fn queue_panel_step_key_advances_the_queue_cursor() {
        let Some(mut engine) = engine_or_skip() else { return };
        let (mut app, t1, t2) = queue_fallback_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        app.queue_panel = true;
        app.current_path = Some(t1);
        app.queue.push(QueueItem::Local(track_with_path(t2.clone(), "encolada")));

        handle_list_key(theme_menu_key(KeyCode::Char('n')), &mut app, &mut engine);

        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "n avanza al ítem encolado con la solapa Cola abierta"
        );
        assert_eq!(app.queue_cursor, Some(0), "el cursor avanza");

        let _ = std::fs::remove_file(&t2);
    }

    /// La fila de controles ajusta sus chips al ancho disponible: un chip que
    /// no entra completo se omite entero (nada de «rep» cortado a la mitad),
    /// y cada hit-zone queda dentro del ancho dado.
    #[test]
    fn transport_row_never_truncates_a_chip_or_overflows_the_width() {
        let (line, buttons) = transport_row(true, true, RepeatMode::All, 20);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("repito") && !text.contains("aleat"),
            "chip recortado a la mitad: {text:?}"
        );
        if let Some((_, start, width)) = buttons.last() {
            assert!(start + width <= 20, "hit-zone fuera del ancho: {buttons:?}");
        }

        let (wide, buttons_wide) = transport_row(true, true, RepeatMode::All, 60);
        let wide_text: String = wide.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(buttons_wide.len(), 5, "con ancho 60 entran los cinco botones");
        assert!(wide_text.contains("repetir: todo"), "{wide_text:?}");
    }

    /// T1/D2: el marquee de un título que entra lo deja pegado a la izquierda
    /// con padding; el que no entra recorre una ventana en bucle, avanzando
    /// un carácter por frame.
    #[test]
    fn marquee_window_fits_short_titles_and_cycles_long_ones() {
        assert_eq!(marquee_window("corto", 10, 0), "corto     ", "entra y va a la izquierda");
        assert_eq!(
            marquee_window("corto", 10, 42),
            "corto     ",
            "los frames no mueven un título que entra"
        );

        let title = "una canción super larga";
        let width = 8;
        let zero = marquee_window(title, width, 0);
        assert_eq!(zero.chars().count(), width, "ventana de ancho exacto");
        let one = marquee_window(title, width, 1);
        assert_ne!(
            one, zero,
            "el frame avanza la ventana un carácter"
        );
        let cycle = title.chars().count() + 3; // título + separador
        assert_eq!(
            marquee_window(title, width, cycle),
            zero,
            "el bucle envuelve a la posición inicial"
        );
        assert_eq!(
            marquee_window(title, 0, 7),
            marquee_window(title, 1, 7),
            "ancho 0 se trata como 1 (sin panico)"
        );
    }

    /// T1/S3: con un próximo (cola o pool), el panel muestra la franja
    /// «próxima canción» con el título; sin próximo, no aparece.
    #[test]
    fn next_up_panel_section_shows_the_marquee_and_hides_without_a_next() {
        let Some(engine) = engine_or_skip() else { return };

        // Con la cola cargada: la franja muestra el próximo de la cola.
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        app.queue.push(queue_url("próxima canción"));
        refresh_next_up(&mut app);
        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            text.contains("próxima canción"),
            "el título próximo aparece en el panel: {text}"
        );

        // Con la cola vacía y biblioteca cargada: el próximo del pool.
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        refresh_next_up(&mut app);
        assert_eq!(
            app.next_up.as_deref(),
            Some("alpha"),
            "cola vacía → el primero del pool"
        );

        // Sin nada próximo (cola vacía y pool vacío): la franja no aparece.
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        refresh_next_up(&mut app);
        assert_eq!(app.next_up, None, "sin pool no hay próximo");
        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            !text.contains("próxima canción"),
            "sin próximo la franja no se dibuja: {text}"
        );
    }

    /// S13: una URL encolada que falla al resolver se saltea — el cursor
    /// avanza al próximo ítem de la cola y lo intenta (erst la URL siguiente).
    #[test]
    fn queue_url_failure_skips_to_next_queue_item() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        app.queue.push(queue_url("uno"));
        app.queue.push(queue_url("dos"));

        let mut captured: Vec<String> = Vec::new();
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured.push(request.original_url);
        });
        assert_eq!(captured, vec!["https://x/uno".to_string()]);
        assert_eq!(app.queue_cursor, Some(0));
        assert_eq!(app.queue_resolving, Some(0), "la primera está en resolución");

        // La resolución de "uno" falla: se saltea, se intenta "dos" y el
        // cursor avanza a "dos" (el ítem fallido queda detrás, atenuado).
        skip_failed_queue_item(&mut app, &mut engine, |_app, _engine, request| {
            captured.push(request.original_url);
        });
        assert_eq!(captured, vec!["https://x/uno".to_string(), "https://x/dos".to_string()]);
        assert_eq!(app.queue_cursor, Some(1), "el cursor avanza al próximo ítem");
        assert_eq!(app.queue_resolving, Some(1), "'dos' queda en resolución");
        assert_eq!(app.queue.len(), 2, "el ítem fallido sigue en la cola");
        assert_eq!(
            app.error.as_deref(),
            Some(QUEUE_ITEM_FAILED_MSG),
            "aviso visible al saltear"
        );
    }

    /// S13: cuando la cola se agota tras saltar un ítem fallido, vuelve al
    /// fallback de S5/S6 (el pool de la biblioteca avanza como hoy).
    #[test]
    fn queue_url_failure_exhausted_falls_back_to_pool() {
        let Some(mut engine) = engine_or_skip() else { return };
        let (mut app, t1, t2) = queue_fallback_app();
        app.current_path = Some(t1.clone());
        app.queue.push(queue_url("unica"));

        let mut captured: Vec<String> = Vec::new();
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured.push(request.original_url);
        });
        assert_eq!(captured, vec!["https://x/unica".to_string()]);
        assert_eq!(app.queue_cursor, Some(0));
        assert_eq!(app.queue_resolving, Some(0));

        // La única URL encolada falla y la cola queda agotada: fallback al pool.
        skip_failed_queue_item(&mut app, &mut engine, |_app, _engine, request| {
            captured.push(request.original_url);
        });
        assert_eq!(captured.len(), 1, "no hay más ítems de cola que intentar");
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "la cola agotada cae al pool y avanza la biblioteca (S5/S6)"
        );
        assert_eq!(app.queue_cursor, Some(0), "el cursor queda en el ítem fallido");
        assert_eq!(app.queue.len(), 1);

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    /// S13: un ítem LOCAL de la cola que no arranca se saltea igual, con
    /// aviso, y se intenta el próximo.
    #[test]
    fn queue_local_failure_is_skipped_and_next_attempted() {
        let Some(mut engine) = engine_or_skip() else { return };
        let mut app = test_app();
        // Ruta local inexistente: start_local_playback falla y se saltea.
        let mut missing = track_named("no-existe");
        missing.path = PathBuf::from(r"C:\tmp\no_existe_esta_carpeta\x.mp3");
        app.queue.push(QueueItem::Local(missing));
        app.queue.push(queue_url("siguiente"));

        let mut captured: Vec<String> = Vec::new();
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured.push(request.original_url);
        });

        assert_eq!(
            captured,
            vec!["https://x/siguiente".to_string()],
            "el local fallido se saltea y se intenta la URL siguiente"
        );
        assert_eq!(app.queue_cursor, Some(1), "el cursor pasa el ítem fallido");
    }
}
