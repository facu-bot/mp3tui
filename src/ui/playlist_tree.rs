//! Árbol de playlists de Spotify: la columna central de la vista de lista.
//!
//! Una fila por playlist (`my_playlists`), colapsable. Expandir una playlist
//! pide sus canciones (`playlist_tracks`) UNA vez y de forma perezosa; colapsar
//! las oculta pero conserva lo cargado. Cada fila lleva un indicador de
//! pública / privada, con `?` cuando Spotify no informa el campo.
//!
//! La red corre en hilos aparte (patrón de `spotify_search`): la UI manda el
//! pedido y recoge el resultado por un channel, etiquetado con la generación
//! del árbol para descartar lo que llega tarde. Los efectos externos (lanzar
//! hilos, resolver, listar) entran por parámetros, así los tests no tocan red.

use super::*;

use crate::source::spotify::client::{SpotifyPlaylist, SpotifyTrack};
use crate::source::spotify::play::{self, ConnectError, LibrarySource};

/// Aviso cuando el hilo de carga muere sin contestar.
const TREE_ABORTED_MSG: &str = "La carga terminó de forma inesperada; probá de nuevo";

/// Una canción de una playlist: `label` = «Artistas - Título».
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TreeTrack {
    pub(super) id: String,
    pub(super) label: String,
}

/// Canciones de una playlist según se hayan pedido o no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TrackState {
    NotLoaded,
    Loading,
    Loaded(Vec<TreeTrack>),
    /// Mensaje listo para mostrar.
    Failed(String),
}

/// Una playlist del árbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TreePlaylist {
    pub(super) id: String,
    pub(super) name: String,
    /// «Dueño · N canciones», con lo que haya.
    pub(super) detail: Option<String>,
    /// `None` cuando Spotify no informa el campo.
    pub(super) public: Option<bool>,
    pub(super) collaborative: bool,
    pub(super) expanded: bool,
    pub(super) tracks: TrackState,
}

/// Estado de la carga de la lista de playlists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TreeLoad {
    /// Todavía no se pidió (el árbol se carga al enfocarlo).
    Idle,
    Loading,
    /// Mensaje listo para mostrar.
    Failed(String),
    Ready,
}

/// Fila visible del árbol (la selección indexa estas filas).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TreeRow {
    Playlist(usize),
    /// Canción `j` de la playlist `i`.
    Track(usize, usize),
    /// Estado bajo la playlist `i` expandida: cargando, error o vacía.
    Status(usize),
}

/// Lo que devuelve un hilo de carga. Los errores ya son texto para pantalla.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TreePayload {
    Playlists(Result<Vec<TreePlaylist>, String>),
    Tracks {
        playlist_id: String,
        result: Result<Vec<TreeTrack>, String>,
    },
}

/// Mensaje del hilo: generación del árbol y resultado.
pub(super) type TreeMessage = (u64, TreePayload);

pub(super) type TreeSender = mpsc::Sender<TreeMessage>;

#[derive(Debug)]
pub(super) struct PlaylistTree {
    pub(super) load: TreeLoad,
    pub(super) playlists: Vec<TreePlaylist>,
    /// Índice dentro de [`PlaylistTree::rows`].
    pub(super) selected: usize,
    /// Se incrementa al (re)cargar: un resultado con otra generación se tira.
    pub(super) generation: u64,
    /// Playlists cuyas canciones se están pidiendo para ENCOLARLAS (Espacio)
    /// apenas lleguen.
    pub(super) pending_enqueue: Vec<String>,
    tx: TreeSender,
    rx: Receiver<TreeMessage>,
}

impl PlaylistTree {
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            load: TreeLoad::Idle,
            playlists: Vec::new(),
            selected: 0,
            generation: 0,
            pending_enqueue: Vec::new(),
            tx,
            rx,
        }
    }

    /// Filas visibles, en orden: cada playlist y, si está expandida, sus
    /// canciones (o una fila de estado mientras no las hay).
    pub(super) fn rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        if self.load != TreeLoad::Ready {
            return rows;
        }
        for (i, playlist) in self.playlists.iter().enumerate() {
            rows.push(TreeRow::Playlist(i));
            if !playlist.expanded {
                continue;
            }
            match &playlist.tracks {
                TrackState::Loaded(tracks) if !tracks.is_empty() => {
                    rows.extend((0..tracks.len()).map(|j| TreeRow::Track(i, j)));
                }
                _ => rows.push(TreeRow::Status(i)),
            }
        }
        rows
    }

    /// ¿Hay un pedido de red en vuelo? (lista de playlists o canciones).
    pub(super) fn is_loading(&self) -> bool {
        self.load == TreeLoad::Loading
            || self
                .playlists
                .iter()
                .any(|playlist| playlist.tracks == TrackState::Loading)
    }

    /// Fila bajo la selección.
    pub(super) fn selected_row(&self) -> Option<TreeRow> {
        self.rows().get(self.selected).copied()
    }

    /// Deja `selected` dentro de las filas visibles.
    fn clamp_selection(&mut self) {
        let len = self.rows().len();
        self.selected = self.selected.min(len.saturating_sub(1));
    }
}

// --- API → filas -----------------------------------------------------------

/// Playlists de la API a nodos del árbol (sin las que no tienen id).
pub(super) fn playlists_from(playlists: Vec<SpotifyPlaylist>) -> Vec<TreePlaylist> {
    playlists
        .into_iter()
        .filter(|playlist| !playlist.id.is_empty())
        .map(|playlist| TreePlaylist {
            detail: playlist_detail(playlist.owner.as_deref(), playlist.tracks_total),
            id: playlist.id,
            name: playlist.name,
            public: playlist.public,
            collaborative: playlist.collaborative,
            expanded: false,
            tracks: TrackState::NotLoaded,
        })
        .collect()
}

/// Canciones de la API a filas (sin los archivos locales: no tienen id).
pub(super) fn tracks_from(tracks: Vec<SpotifyTrack>) -> Vec<TreeTrack> {
    tracks
        .iter()
        .filter(|track| !track.id.is_empty())
        .map(|track| TreeTrack {
            id: track.id.clone(),
            label: play::track_label(track),
        })
        .collect()
}

// --- red (en hilos) --------------------------------------------------------

/// Corre `job` y convierte un pánico en un error para pantalla: el árbol no
/// se queda en «Cargando…» para siempre si el hilo muere.
fn run_guarded<T>(job: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
        .unwrap_or_else(|_| Err(TREE_ABORTED_MSG.to_string()))
}

/// Cuerpo del hilo que carga las playlists, con el cliente inyectado.
pub(super) fn run_load_playlists(
    connect: &dyn Fn() -> Result<Box<dyn LibrarySource>, ConnectError>,
) -> Result<Vec<TreePlaylist>, String> {
    play::user_playlists(connect).map(playlists_from)
}

/// Cuerpo del hilo que carga las canciones de una playlist.
pub(super) fn run_load_tracks(
    id: &str,
    connect: &dyn Fn() -> Result<Box<dyn LibrarySource>, ConnectError>,
) -> Result<Vec<TreeTrack>, String> {
    play::user_playlist_tracks(id, connect).map(tracks_from)
}

fn connect_library() -> Result<Box<dyn LibrarySource>, ConnectError> {
    play::connect(crate::config::config()).map(|client| Box::new(client) as Box<dyn LibrarySource>)
}

/// Único punto que lanza la carga de playlists: los tests inyectan un doble.
pub(super) fn spawn_load_playlists(generation: u64, tx: TreeSender) {
    thread::spawn(move || {
        let result = run_guarded(|| run_load_playlists(&connect_library));
        let _ = tx.send((generation, TreePayload::Playlists(result)));
    });
}

/// Único punto que lanza la carga de las canciones de una playlist.
pub(super) fn spawn_load_tracks(playlist_id: String, generation: u64, tx: TreeSender) {
    thread::spawn(move || {
        let result = run_guarded(|| run_load_tracks(&playlist_id, &connect_library));
        let _ = tx.send((
            generation,
            TreePayload::Tracks {
                playlist_id,
                result,
            },
        ));
    });
}

// --- estado ----------------------------------------------------------------

/// Pide las playlists (o las vuelve a pedir) y pasa a «cargando».
pub(super) fn start_loading_playlists_with(
    app: &mut App,
    spawn_playlists: impl FnOnce(u64, TreeSender),
) {
    let tree = &mut app.playlist_tree;
    tree.generation = tree.generation.wrapping_add(1);
    tree.load = TreeLoad::Loading;
    tree.playlists.clear();
    tree.pending_enqueue.clear();
    tree.selected = 0;
    spawn_playlists(tree.generation, tree.tx.clone());
}

/// Abre la vista de lista (si hay player), pone el foco en el árbol y, si
/// todavía no se cargó o falló, pide las playlists.
pub(super) fn focus_playlist_tree_with(
    app: &mut App,
    spawn_playlists: impl FnOnce(u64, TreeSender),
) {
    if app.has_player && !app.explorer_open {
        toggle_explorer(app);
    }
    app.tree_focus = true;
    app.queue_panel = false;
    if matches!(app.playlist_tree.load, TreeLoad::Idle | TreeLoad::Failed(_)) {
        start_loading_playlists_with(app, spawn_playlists);
    }
}

pub(super) fn focus_playlist_tree(app: &mut App) {
    focus_playlist_tree_with(app, spawn_load_playlists);
}

/// Aplica el resultado de un hilo de carga al árbol.
pub(super) fn handle_tree_result(app: &mut App, generation: u64, payload: TreePayload) {
    if generation != app.playlist_tree.generation {
        return;
    }
    // Canciones que había que encolar al llegar (Espacio sobre una playlist).
    let mut to_enqueue: Option<Result<Vec<TreeTrack>, String>> = None;
    let tree = &mut app.playlist_tree;
    match payload {
        TreePayload::Playlists(result) if tree.load == TreeLoad::Loading => match result {
            Ok(playlists) => {
                tree.playlists = playlists;
                tree.selected = 0;
                tree.load = TreeLoad::Ready;
            }
            Err(message) => tree.load = TreeLoad::Failed(message),
        },
        TreePayload::Tracks {
            playlist_id,
            result,
        } => {
            let loading = tree.playlists.iter_mut().find(|playlist| {
                playlist.id == playlist_id && playlist.tracks == TrackState::Loading
            });
            if let Some(playlist) = loading {
                playlist.tracks = match &result {
                    Ok(tracks) => TrackState::Loaded(tracks.clone()),
                    Err(message) => TrackState::Failed(message.clone()),
                };
                if let Some(at) = tree
                    .pending_enqueue
                    .iter()
                    .position(|id| *id == playlist_id)
                {
                    tree.pending_enqueue.remove(at);
                    to_enqueue = Some(result);
                }
            }
        }
        TreePayload::Playlists(_) => {}
    }
    tree.clamp_selection();
    match to_enqueue {
        Some(Ok(tracks)) => enqueue_tracks(app, &tracks),
        Some(Err(message)) => app.error = Some(message),
        None => {}
    }
}

/// Mensaje cuando se intenta encolar una playlist sin canciones reproducibles.
const NOTHING_TO_QUEUE_MSG: &str = "La playlist no tiene canciones reproducibles";

/// Suma las canciones al final de la cola como marcadores `spotify:track:<id>`
/// (se resuelven recién cuando les toca sonar, como cualquier ítem de URL).
fn enqueue_tracks(app: &mut App, tracks: &[TreeTrack]) {
    if tracks.is_empty() {
        app.error = Some(NOTHING_TO_QUEUE_MSG.to_string());
        return;
    }
    app.queue.extend(tracks.iter().map(|track| QueueItem::Url {
        url: play::track_marker(&track.id),
        title: track.label.clone(),
    }));
}

/// Nada suena ni está en pausa: encolar una playlist la hace arrancar.
fn nothing_playing(app: &App) -> bool {
    matches!(app.status, Status::Ended | Status::Stopped)
}

/// Recoge los resultados de los hilos sin bloquear (lo llama el event loop en
/// cada frame).
pub(super) fn poll_playlist_tree(app: &mut App) {
    while let Ok((generation, payload)) = app.playlist_tree.rx.try_recv() {
        app.dirty = true;
        handle_tree_result(app, generation, payload);
    }
}

/// Marca la playlist `index` como expandida y, si no tiene sus canciones,
/// las pide (una llamada por playlist abierta).
fn expand_playlist(
    app: &mut App,
    index: usize,
    spawn_tracks: impl FnOnce(String, u64, TreeSender),
) {
    let tree = &mut app.playlist_tree;
    let generation = tree.generation;
    let tx = tree.tx.clone();
    let Some(playlist) = tree.playlists.get_mut(index) else {
        return;
    };
    playlist.expanded = true;
    if matches!(
        playlist.tracks,
        TrackState::NotLoaded | TrackState::Failed(_)
    ) {
        playlist.tracks = TrackState::Loading;
        spawn_tracks(playlist.id.clone(), generation, tx);
    }
    tree.clamp_selection();
}

/// Pliega la playlist `index` y deja la selección sobre ella.
fn collapse_playlist(app: &mut App, index: usize) {
    let tree = &mut app.playlist_tree;
    if let Some(playlist) = tree.playlists.get_mut(index) {
        playlist.expanded = false;
    }
    if let Some(row) = tree
        .rows()
        .iter()
        .position(|row| *row == TreeRow::Playlist(index))
    {
        tree.selected = row;
    }
}

/// Teclas del árbol con los efectos externos inyectados. Devuelve `true` si la
/// tecla era del árbol; el resto (pausa, pista siguiente, salir…) sigue su
/// camino normal.
pub(super) fn handle_tree_key_with(
    key: KeyEvent,
    app: &mut App,
    spawn_playlists: impl FnOnce(u64, TreeSender),
    spawn_tracks: impl FnOnce(String, u64, TreeSender),
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) -> bool {
    let code = key.code;
    let keys = &config::config().keys;
    let delta = if config::pressed(&keys.move_up, code) {
        Some(-1)
    } else if config::pressed(&keys.move_down, code) {
        Some(1)
    } else if config::pressed(&keys.page_up, code) {
        Some(-PAGE_STEP)
    } else if config::pressed(&keys.page_down, code) {
        Some(PAGE_STEP)
    } else if config::pressed(&keys.home, code) {
        Some(isize::MIN)
    } else if config::pressed(&keys.end, code) {
        Some(isize::MAX)
    } else {
        None
    };
    let wants_select = config::pressed(&keys.select, code);
    // En la lista la tecla de pausa es la acción de la fila: encolar.
    let wants_enqueue = config::pressed(&keys.play_pause, code);
    if delta.is_none() && !wants_select && !wants_enqueue {
        return false;
    }

    // Sin lista (sin cargar, cargando o fallada) solo Enter hace algo: reintentar.
    if app.playlist_tree.load != TreeLoad::Ready {
        if wants_select && matches!(app.playlist_tree.load, TreeLoad::Failed(_)) {
            start_loading_playlists_with(app, spawn_playlists);
        }
        return true;
    }

    let row = app.playlist_tree.selected_row();
    if let Some(delta) = delta {
        let tree = &mut app.playlist_tree;
        tree.selected = move_selection(tree.selected, delta, tree.rows().len());
        return true;
    }
    let Some(row) = row else {
        return true;
    };
    match (row, wants_select) {
        // Enter sobre una playlist: plegar / desplegar.
        (TreeRow::Playlist(index), true) => toggle_playlist(app, index, spawn_tracks),
        // Enter sobre una canción: reproducirla.
        (TreeRow::Track(index, track), true) => {
            if let TrackState::Loaded(tracks) = &app.playlist_tree.playlists[index].tracks
                && let Some(track) = tracks.get(track)
            {
                let (marker, label) = (play::track_marker(&track.id), track.label.clone());
                play_spotify_marker(app, marker, &label, resolve_page, list_playlist);
            }
        }
        // Enter sobre «falló»: reintenta esa playlist.
        (TreeRow::Status(index), true) => {
            if matches!(
                app.playlist_tree.playlists[index].tracks,
                TrackState::Failed(_)
            ) {
                expand_playlist(app, index, spawn_tracks);
            }
        }
        // Espacio sobre una playlist: encolarla entera (o arrancarla si no
        // suena nada).
        (TreeRow::Playlist(index), false) => {
            enqueue_playlist(app, index, spawn_tracks, resolve_page, list_playlist);
        }
        // Espacio sobre una canción: encolarla.
        (TreeRow::Track(index, track), false) => {
            if let TrackState::Loaded(tracks) = &app.playlist_tree.playlists[index].tracks
                && let Some(track) = tracks.get(track).cloned()
            {
                enqueue_tracks(app, &[track]);
            }
        }
        (TreeRow::Status(_), false) => {}
    }
    true
}

/// Enter sobre una playlist: la despliega (pidiendo sus canciones si faltan)
/// o la pliega, y deja la selección sobre ella.
fn toggle_playlist(
    app: &mut App,
    index: usize,
    spawn_tracks: impl FnOnce(String, u64, TreeSender),
) {
    if app.playlist_tree.playlists[index].expanded {
        collapse_playlist(app, index);
    } else {
        expand_playlist(app, index, spawn_tracks);
    }
}

/// Espacio sobre una playlist: si no suena nada arranca con ella por el flujo
/// de siempre; si no, suma TODAS sus canciones al final de la cola. Las que
/// todavía no se cargaron se piden y se encolan al llegar.
fn enqueue_playlist(
    app: &mut App,
    index: usize,
    spawn_tracks: impl FnOnce(String, u64, TreeSender),
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) {
    let playlist = &app.playlist_tree.playlists[index];
    if nothing_playing(app) {
        let (marker, label) = (
            format!("spotify:playlist:{}", playlist.id),
            playlist.name.clone(),
        );
        play_spotify_marker(app, marker, &label, resolve_page, list_playlist);
        return;
    }
    match &playlist.tracks {
        TrackState::Loaded(tracks) => {
            let tracks = tracks.clone();
            enqueue_tracks(app, &tracks);
        }
        TrackState::Loading | TrackState::NotLoaded | TrackState::Failed(_) => {
            let id = playlist.id.clone();
            let needs_request = !matches!(playlist.tracks, TrackState::Loading);
            if !app.playlist_tree.pending_enqueue.contains(&id) {
                app.playlist_tree.pending_enqueue.push(id.clone());
            }
            if needs_request {
                let tree = &mut app.playlist_tree;
                tree.playlists[index].tracks = TrackState::Loading;
                spawn_tracks(id, tree.generation, tree.tx.clone());
            }
        }
    }
}

pub(super) fn handle_tree_key(key: KeyEvent, app: &mut App) -> bool {
    handle_tree_key_with(
        key,
        app,
        spawn_load_playlists,
        spawn_load_tracks,
        spawn_ytdlp_resolution,
        spawn_ytdlp_listing,
    )
}

// --- render ----------------------------------------------------------------

/// Filas del panel: estado cuando no hay lista y, con datos, la ventana de
/// filas que sigue a la selección. Ninguna fila excede `width`.
pub(super) fn playlist_tree_lines(
    tree: &PlaylistTree,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    let colors = theme();
    let dim = Style::default().fg(colors.dim);
    // Estados sin lista: sin recortar, el panel los parte en varias líneas.
    let status =
        |text: String, color| vec![Line::from(Span::styled(text, Style::default().fg(color)))];
    match &tree.load {
        TreeLoad::Idle => {
            let key = key_labels(&config::config().keys.spotify_library);
            return vec![Line::from(Span::styled(
                format!(" {key}: cargar tus playlists de Spotify"),
                dim,
            ))];
        }
        TreeLoad::Loading => return status(" Cargando tus playlists…".into(), colors.warn),
        TreeLoad::Failed(message) => return status(format!(" {message}"), colors.error),
        TreeLoad::Ready if tree.playlists.is_empty() => {
            return status(" No tenés playlists en tu cuenta".into(), colors.dim);
        }
        TreeLoad::Ready => {}
    }

    let rows = tree.rows();
    let offset = scroll_offset(tree.selected, rows.len(), height);
    rows.iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .flat_map(|(index, row)| tree_row_lines(tree, *row, index == tree.selected, width))
        .take(height)
        .collect()
}

/// Indicador de visibilidad: `pub` / `priv` / `?`, más `colab` si es
/// colaborativa.
fn visibility_tag(playlist: &TreePlaylist) -> String {
    let base = match playlist.public {
        Some(true) => "pub",
        Some(false) => "priv",
        None => "?",
    };
    if playlist.collaborative {
        format!("{base} colab")
    } else {
        base.to_string()
    }
}

/// Máximo de líneas que ocupa el mensaje de error bajo una playlist.
const STATUS_ERROR_LINES: usize = 3;

fn tree_row_lines(
    tree: &PlaylistTree,
    row: TreeRow,
    selected: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let colors = theme();
    let row_style = if selected {
        Style::default()
            .bg(colors.select_bg)
            .fg(colors.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(colors.text)
    };
    let dim = if selected {
        row_style
    } else {
        Style::default().fg(colors.dim)
    };
    let cursor = if selected { CURSOR } else { " " };

    match row {
        TreeRow::Playlist(index) => {
            let playlist = &tree.playlists[index];
            let arrow = if playlist.expanded { '▾' } else { '▸' };
            let prefix = format!("{cursor}{arrow} ");
            let tag = visibility_tag(playlist);
            let available = width.saturating_sub(prefix.chars().count());
            // La etiqueta va pegada a la derecha si sobra lugar para el nombre.
            let tag_width = tag.chars().count() + 1;
            let show_tag = available >= tag_width + 6;
            let name_width = if show_tag {
                available - tag_width
            } else {
                available
            };
            let name = truncate_chars(&playlist.name, name_width);
            let pad = name_width.saturating_sub(name.chars().count());
            let mut spans = vec![Span::styled(prefix, dim), Span::styled(name, row_style)];
            if show_tag {
                let color = match playlist.public {
                    Some(true) => colors.ok,
                    Some(false) => colors.dim,
                    None => colors.warn,
                };
                spans.push(Span::styled(" ".repeat(pad + 1), row_style));
                spans.push(Span::styled(
                    tag,
                    if selected {
                        row_style
                    } else {
                        Style::default().fg(color)
                    },
                ));
            }
            vec![Line::from(spans)]
        }
        TreeRow::Track(index, track) => {
            let label = match &tree.playlists[index].tracks {
                TrackState::Loaded(tracks) => tracks[track].label.as_str(),
                _ => "",
            };
            let prefix = format!("{cursor}   ");
            let available = width.saturating_sub(prefix.chars().count());
            vec![Line::from(vec![
                Span::styled(prefix, dim),
                Span::styled(truncate_chars(label, available), row_style),
            ])]
        }
        TreeRow::Status(index) => {
            let (text, color) = match &tree.playlists[index].tracks {
                TrackState::Failed(message) => (message.clone(), colors.error),
                TrackState::Loaded(_) => ("Sin canciones reproducibles".to_string(), colors.dim),
                _ => ("Cargando canciones…".to_string(), colors.warn),
            };
            let style = if selected {
                row_style
            } else {
                Style::default().fg(color)
            };
            let available = width.saturating_sub(4).max(1);
            // Un error largo se parte en hasta `STATUS_ERROR_LINES` líneas.
            let chars: Vec<char> = text.chars().collect();
            let mut pieces: Vec<String> = chars
                .chunks(available)
                .take(STATUS_ERROR_LINES)
                .map(|piece| piece.iter().collect())
                .collect();
            if chars.len() > available * STATUS_ERROR_LINES
                && let Some(last) = pieces.last_mut()
            {
                *last = truncate_chars(&format!("{last}…"), available);
            }
            pieces
                .into_iter()
                .enumerate()
                .map(|(n, piece)| {
                    let prefix = if n == 0 {
                        format!("{cursor}   ")
                    } else {
                        "    ".to_string()
                    };
                    Line::from(vec![Span::styled(prefix, dim), Span::styled(piece, style)])
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::spotify::client::SpotifyError;
    use crate::ui::tests_common::*;
    use std::cell::RefCell;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn api_playlist(
        id: &str,
        name: &str,
        public: Option<bool>,
        collaborative: bool,
    ) -> SpotifyPlaylist {
        SpotifyPlaylist {
            id: id.to_string(),
            name: name.to_string(),
            owner: Some("yo".to_string()),
            tracks_total: Some(2),
            public,
            collaborative,
        }
    }

    fn api_track(id: &str, title: &str, artists: &[&str]) -> SpotifyTrack {
        SpotifyTrack {
            id: id.to_string(),
            title: title.to_string(),
            artists: artists.iter().map(|a| a.to_string()).collect(),
            album: None,
            duration_ms: 1000,
            isrc: None,
        }
    }

    fn tr(id: &str, label: &str) -> TreeTrack {
        TreeTrack {
            id: id.to_string(),
            label: label.to_string(),
        }
    }

    fn node(id: &str, name: &str, public: Option<bool>) -> TreePlaylist {
        TreePlaylist {
            id: id.to_string(),
            name: name.to_string(),
            detail: None,
            public,
            collaborative: false,
            expanded: false,
            tracks: TrackState::NotLoaded,
        }
    }

    /// App con el árbol ya cargado: Rock (p1, pública), Jazz (p2, privada),
    /// Mix (p3, sin dato).
    fn app_with_tree() -> App {
        let mut app = test_app();
        app.playlist_tree.load = TreeLoad::Ready;
        app.playlist_tree.playlists = vec![
            node("p1", "Rock", Some(true)),
            node("p2", "Jazz", Some(false)),
            node("p3", "Mix", None),
        ];
        app.tree_focus = true;
        app
    }

    fn expand_with_tracks(app: &mut App, index: usize, tracks: Vec<TreeTrack>) {
        let playlist = &mut app.playlist_tree.playlists[index];
        playlist.expanded = true;
        playlist.tracks = TrackState::Loaded(tracks);
    }

    fn no_playlists(_: u64, _: TreeSender) {
        panic!("no debía cargar playlists");
    }

    fn no_tracks(id: String, _: u64, _: TreeSender) {
        panic!("no debía cargar canciones de {id}");
    }

    fn no_resolve(url: String, _: u64) -> Receiver<YtDlpMessage> {
        panic!("no debía resolver {url}");
    }

    fn no_list(url: String, _: u64) -> Receiver<YtDlpListMessage> {
        panic!("no debía listar {url}");
    }

    /// Tecla del árbol sin ningún efecto externo permitido.
    fn press(app: &mut App, code: KeyCode) -> bool {
        handle_tree_key_with(key(code), app, no_playlists, no_tracks, no_resolve, no_list)
    }

    fn text_of(lines: &[Line<'_>]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    // --- mapeo de la API ---

    #[test]
    fn api_playlists_keep_visibility_and_skip_the_ones_without_id() {
        let rows = playlists_from(vec![
            api_playlist("p1", "Pub", Some(true), false),
            api_playlist("p2", "Colab", Some(false), true),
            api_playlist("p3", "Sin dato", None, false),
            api_playlist("", "Sin id", None, false),
        ]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].public, Some(true));
        assert_eq!(rows[1].public, Some(false));
        assert!(rows[1].collaborative);
        assert_eq!(rows[2].public, None);
        assert!(rows.iter().all(|p| !p.expanded));
        assert!(rows.iter().all(|p| p.tracks == TrackState::NotLoaded));
        assert_eq!(rows[0].detail.as_deref(), Some("yo · 2 canciones"));
    }

    #[test]
    fn api_tracks_map_to_labels_skipping_the_ones_without_id() {
        let rows = tracks_from(vec![
            api_track("t1", "Uno", &["A", "B"]),
            api_track("", "Local", &["Z"]),
        ]);
        assert_eq!(rows, vec![tr("t1", "A, B - Uno")]);
    }

    // --- filas visibles ---

    #[test]
    fn rows_are_empty_until_the_playlists_are_loaded() {
        let tree = PlaylistTree::new();
        assert_eq!(tree.load, TreeLoad::Idle);
        assert!(tree.rows().is_empty());
    }

    #[test]
    fn collapsed_playlists_show_one_row_each_and_expanded_ones_show_their_tracks() {
        let mut app = app_with_tree();
        assert_eq!(
            app.playlist_tree.rows(),
            vec![
                TreeRow::Playlist(0),
                TreeRow::Playlist(1),
                TreeRow::Playlist(2)
            ]
        );
        expand_with_tracks(&mut app, 1, vec![tr("t1", "A - Uno"), tr("t2", "B - Dos")]);
        assert_eq!(
            app.playlist_tree.rows(),
            vec![
                TreeRow::Playlist(0),
                TreeRow::Playlist(1),
                TreeRow::Track(1, 0),
                TreeRow::Track(1, 1),
                TreeRow::Playlist(2),
            ]
        );
    }

    #[test]
    fn an_expanded_playlist_without_tracks_yet_shows_a_status_row() {
        let mut app = app_with_tree();
        for state in [
            TrackState::Loading,
            TrackState::Failed("boom".into()),
            TrackState::Loaded(Vec::new()),
        ] {
            let playlist = &mut app.playlist_tree.playlists[0];
            playlist.expanded = true;
            playlist.tracks = state;
            assert_eq!(
                app.playlist_tree.rows()[..2],
                [TreeRow::Playlist(0), TreeRow::Status(0)]
            );
        }
    }

    // --- cargar las playlists ---

    #[test]
    fn focusing_the_tree_starts_loading_in_the_background_once() {
        let mut app = test_app();
        let calls = RefCell::new(Vec::new());
        focus_playlist_tree_with(&mut app, |generation, _| {
            calls.borrow_mut().push(generation)
        });
        assert!(app.tree_focus);
        assert!(!app.queue_panel);
        assert_eq!(app.playlist_tree.load, TreeLoad::Loading);
        assert_eq!(calls.borrow().as_slice(), &[app.playlist_tree.generation]);

        // Ya cargando (o cargado): enfocar de nuevo no repite el pedido.
        focus_playlist_tree_with(&mut app, no_playlists);
        app.playlist_tree.load = TreeLoad::Ready;
        focus_playlist_tree_with(&mut app, no_playlists);
    }

    #[test]
    fn focusing_a_failed_tree_retries_the_load() {
        let mut app = test_app();
        app.playlist_tree.load = TreeLoad::Failed("sin sesión".into());
        let calls = RefCell::new(0);
        focus_playlist_tree_with(&mut app, |_, _| *calls.borrow_mut() += 1);
        assert_eq!(*calls.borrow(), 1);
        assert_eq!(app.playlist_tree.load, TreeLoad::Loading);
    }

    #[test]
    fn focusing_opens_the_list_view_when_there_is_a_player() {
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        assert!(!app.explorer_open);
        focus_playlist_tree_with(&mut app, |_, _| {});
        assert!(app.explorer_open, "la vista de lista se abre");
        assert!(app.tree_focus);
    }

    #[test]
    fn the_loaded_playlists_arrive_through_the_channel_without_blocking() {
        let mut app = test_app();
        let tx = app.playlist_tree.tx.clone();
        focus_playlist_tree_with(&mut app, |generation, _| {
            tx.send((
                generation,
                TreePayload::Playlists(Ok(vec![node("p1", "Rock", Some(true))])),
            ))
            .unwrap();
        });
        assert_eq!(app.playlist_tree.load, TreeLoad::Loading);
        poll_playlist_tree(&mut app);
        assert_eq!(app.playlist_tree.load, TreeLoad::Ready);
        assert_eq!(app.playlist_tree.playlists.len(), 1);
        assert_eq!(app.playlist_tree.selected, 0);
        // Sin nada pendiente, otra vuelta no cambia nada.
        poll_playlist_tree(&mut app);
        assert_eq!(app.playlist_tree.load, TreeLoad::Ready);
    }

    #[test]
    fn a_failed_load_keeps_the_message_for_the_screen() {
        let mut app = test_app();
        focus_playlist_tree_with(&mut app, |_, _| {});
        let generation = app.playlist_tree.generation;
        handle_tree_result(
            &mut app,
            generation,
            TreePayload::Playlists(Err("no hay sesión".into())),
        );
        assert_eq!(
            app.playlist_tree.load,
            TreeLoad::Failed("no hay sesión".into())
        );
    }

    #[test]
    fn results_from_an_older_generation_are_discarded() {
        let mut app = test_app();
        focus_playlist_tree_with(&mut app, |_, _| {});
        let stale = app.playlist_tree.generation;
        // Reintento: la generación avanza y lo anterior ya no vale.
        app.playlist_tree.load = TreeLoad::Failed("x".into());
        focus_playlist_tree_with(&mut app, |_, _| {});
        assert_ne!(stale, app.playlist_tree.generation);
        handle_tree_result(
            &mut app,
            stale,
            TreePayload::Playlists(Ok(vec![node("p9", "Viejo", None)])),
        );
        assert_eq!(app.playlist_tree.load, TreeLoad::Loading);
        assert!(app.playlist_tree.playlists.is_empty());
    }

    // --- errores del cliente, de punta a punta (sin red) ---

    struct FakeLibrary {
        playlists: Result<Vec<SpotifyPlaylist>, SpotifyError>,
        tracks: Result<Vec<SpotifyTrack>, SpotifyError>,
    }

    impl LibrarySource for FakeLibrary {
        fn my_playlists(&self) -> Result<Vec<SpotifyPlaylist>, SpotifyError> {
            self.playlists.clone()
        }
        fn playlist_tracks(&self, _: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
            self.tracks.clone()
        }
    }

    fn fake_connect(
        library: FakeLibrary,
    ) -> impl Fn() -> Result<Box<dyn LibrarySource>, ConnectError> {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(Some(library)));
        move || {
            let library = shared.lock().unwrap().take().expect("un solo uso");
            Ok(Box::new(library) as Box<dyn LibrarySource>)
        }
    }

    #[test]
    fn the_load_bodies_map_the_client_results_to_tree_rows() {
        let connect = fake_connect(FakeLibrary {
            playlists: Ok(vec![api_playlist("p1", "Rock", Some(false), false)]),
            tracks: Ok(vec![api_track("t1", "Uno", &["A"])]),
        });
        let playlists = run_load_playlists(&connect).unwrap();
        assert_eq!(playlists[0].public, Some(false));

        let connect = fake_connect(FakeLibrary {
            playlists: Ok(Vec::new()),
            tracks: Ok(vec![api_track("t1", "Uno", &["A"])]),
        });
        assert_eq!(
            run_load_tracks("p1", &connect).unwrap(),
            vec![tr("t1", "A - Uno")]
        );
    }

    #[test]
    fn no_session_no_client_id_and_rate_limit_become_clear_messages() {
        use crate::source::spotify::MissingClientId;

        let no_session =
            || -> Result<Box<dyn LibrarySource>, ConnectError> { Err(ConnectError::NotLoggedIn) };
        let message = run_load_playlists(&no_session).unwrap_err();
        assert!(message.contains("mp3tui --spotify-login"), "{message}");

        let no_client = || -> Result<Box<dyn LibrarySource>, ConnectError> {
            Err(ConnectError::ClientId(MissingClientId))
        };
        let message = run_load_playlists(&no_client).unwrap_err();
        assert!(message.contains("client_id"), "{message}");

        let limited = fake_connect(FakeLibrary {
            playlists: Err(SpotifyError::RateLimited {
                retry_after_secs: Some(7),
            }),
            tracks: Ok(Vec::new()),
        });
        let message = run_load_playlists(&limited).unwrap_err();
        assert!(message.contains("limitó la tasa"), "{message}");
        assert!(message.contains('7'), "{message}");
    }

    #[test]
    fn a_panicking_load_becomes_an_error_instead_of_hanging_the_panel() {
        let result: Result<(), String> = run_guarded(|| panic!("boom"));
        assert_eq!(result, Err(TREE_ABORTED_MSG.to_string()));
        assert_eq!(run_guarded(|| Ok::<_, String>(3)), Ok(3));
    }

    // --- expandir y colapsar (Enter sobre una playlist) ---

    fn space() -> KeyCode {
        KeyCode::Char(' ')
    }

    /// Tecla con un seam de canciones que anota los pedidos.
    fn press_logging_tracks(
        app: &mut App,
        code: KeyCode,
        calls: &RefCell<Vec<(String, u64)>>,
    ) -> bool {
        handle_tree_key_with(
            key(code),
            app,
            no_playlists,
            |id, generation, _| calls.borrow_mut().push((id, generation)),
            no_resolve,
            no_list,
        )
    }

    #[test]
    fn enter_on_a_playlist_expands_it_and_requests_its_tracks_once() {
        let mut app = app_with_tree();
        app.playlist_tree.selected = 1;
        let calls = RefCell::new(Vec::new());
        let generation = app.playlist_tree.generation;
        assert!(press_logging_tracks(&mut app, KeyCode::Enter, &calls));
        assert_eq!(calls.borrow().as_slice(), &[("p2".to_string(), generation)]);
        assert!(app.playlist_tree.playlists[1].expanded);
        assert_eq!(app.playlist_tree.playlists[1].tracks, TrackState::Loading);
        // Las otras playlists no se tocan: la carga es perezosa.
        assert_eq!(app.playlist_tree.playlists[0].tracks, TrackState::NotLoaded);
        assert_eq!(app.playlist_tree.playlists[2].tracks, TrackState::NotLoaded);
        // Enter no reproduce nada: sigue sin sonar ni listar.
        assert!(app.current_url.is_none());
    }

    #[test]
    fn enter_again_collapses_and_the_next_expand_reuses_the_loaded_tracks() {
        let mut app = app_with_tree();
        expand_with_tracks(&mut app, 0, vec![tr("t1", "A - Uno")]);
        app.playlist_tree.selected = 0;
        assert!(press(&mut app, KeyCode::Enter), "sin pedir nada");
        assert!(!app.playlist_tree.playlists[0].expanded);
        assert_eq!(app.playlist_tree.rows().len(), 3);
        // Cargadas quedan: volver a expandir no hace ni un pedido más.
        assert!(press(&mut app, KeyCode::Enter));
        assert!(app.playlist_tree.playlists[0].expanded);
        assert_eq!(app.playlist_tree.rows().len(), 4);
    }

    #[test]
    fn collapsing_keeps_the_selection_on_the_playlist() {
        let mut app = app_with_tree();
        expand_with_tracks(&mut app, 1, vec![tr("t1", "A - Uno")]);
        app.playlist_tree.selected = 1; // Jazz
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.playlist_tree.selected_row(), Some(TreeRow::Playlist(1)));
    }

    #[test]
    fn left_and_right_no_longer_fold_the_tree_they_belong_to_the_column_focus() {
        let mut app = app_with_tree();
        expand_with_tracks(&mut app, 0, vec![tr("t1", "A - Uno")]);
        app.playlist_tree.selected = 0;
        assert!(!press(&mut app, KeyCode::Left));
        assert!(!press(&mut app, KeyCode::Right));
        assert!(app.playlist_tree.playlists[0].expanded, "nada se plegó");
        app.playlist_tree.playlists[0].expanded = false;
        assert!(!press(&mut app, KeyCode::Right));
        assert!(!app.playlist_tree.playlists[0].expanded, "nada se desplegó");
    }

    #[test]
    fn the_tracks_arrive_and_show_indented_under_their_playlist() {
        let mut app = app_with_tree();
        app.playlist_tree.selected = 0;
        let calls = RefCell::new(Vec::new());
        press_logging_tracks(&mut app, KeyCode::Enter, &calls);
        let generation = app.playlist_tree.generation;
        handle_tree_result(
            &mut app,
            generation,
            TreePayload::Tracks {
                playlist_id: "p1".into(),
                result: Ok(vec![tr("t1", "A - Uno"), tr("t2", "B - Dos")]),
            },
        );
        assert_eq!(
            app.playlist_tree.playlists[0].tracks,
            TrackState::Loaded(vec![tr("t1", "A - Uno"), tr("t2", "B - Dos")])
        );
        let text = text_of(&playlist_tree_lines(&app.playlist_tree, 40, 10));
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows[0].contains("Rock"), "{text}");
        assert!(rows[1].contains("A - Uno"), "{text}");
        let indent =
            |row: &str, needle: &str| row.chars().take_while(|c| !needle.starts_with(*c)).count();
        assert!(
            indent(rows[1], "A - Uno") > indent(rows[0], "Rock"),
            "{text}"
        );
    }

    #[test]
    fn a_track_failure_stays_under_its_playlist_and_enter_retries_it() {
        let mut app = app_with_tree();
        app.playlist_tree.selected = 0;
        let calls = RefCell::new(Vec::new());
        press_logging_tracks(&mut app, KeyCode::Enter, &calls);
        let generation = app.playlist_tree.generation;
        handle_tree_result(
            &mut app,
            generation,
            TreePayload::Tracks {
                playlist_id: "p1".into(),
                result: Err("Spotify limitó la tasa".into()),
            },
        );
        let text = text_of(&playlist_tree_lines(&app.playlist_tree, 40, 10));
        assert!(text.contains("limitó la tasa"), "{text}");

        // Enter sobre la fila de estado reintenta esa playlist.
        app.playlist_tree.selected = 1;
        let retries = RefCell::new(Vec::new());
        press_logging_tracks(&mut app, KeyCode::Enter, &retries);
        assert_eq!(retries.borrow().len(), 1);
        assert_eq!(retries.borrow()[0].0, "p1");
        assert_eq!(app.playlist_tree.playlists[0].tracks, TrackState::Loading);
    }

    // --- navegación ---

    #[test]
    fn navigation_keys_move_the_selection_over_the_visible_rows() {
        let mut app = app_with_tree();
        expand_with_tracks(&mut app, 0, vec![tr("t1", "A - Uno")]);
        // Filas: Rock, t1, Jazz, Mix.
        assert!(press(&mut app, KeyCode::Down));
        assert_eq!(app.playlist_tree.selected, 1);
        assert!(press(&mut app, KeyCode::End));
        assert_eq!(app.playlist_tree.selected, 3);
        assert!(press(&mut app, KeyCode::Up));
        assert_eq!(app.playlist_tree.selected, 2);
        assert!(press(&mut app, KeyCode::Home));
        assert_eq!(app.playlist_tree.selected, 0);
        assert!(press(&mut app, KeyCode::PageDown));
        assert_eq!(
            app.playlist_tree.selected, 3,
            "la página se frena en la última fila"
        );
        assert!(press(&mut app, KeyCode::PageUp));
        assert_eq!(app.playlist_tree.selected, 0);
    }

    #[test]
    fn keys_the_tree_does_not_own_are_left_alone() {
        let mut app = app_with_tree();
        for code in [
            KeyCode::Char('n'),
            KeyCode::Char('p'),
            KeyCode::Tab,
            KeyCode::Esc,
            KeyCode::Left,
            KeyCode::Right,
        ] {
            assert!(!press(&mut app, code), "{code:?}");
        }
    }

    // --- reproducir (Enter sobre una canción) ---

    #[test]
    fn enter_on_a_track_plays_that_track_through_the_existing_url_flow() {
        let mut app = app_with_tree();
        expand_with_tracks(&mut app, 0, vec![tr("t1", "A - Uno"), tr("t2", "B - Dos")]);
        app.playlist_tree.selected = 2; // t2
        let resolved = RefCell::new(Vec::new());
        assert!(handle_tree_key_with(
            key(KeyCode::Enter),
            &mut app,
            no_playlists,
            no_tracks,
            |url, _| {
                resolved.borrow_mut().push(url);
                mpsc::channel().1
            },
            no_list,
        ));
        assert_eq!(
            resolved.borrow().as_slice(),
            &["spotify:track:t2".to_string()]
        );
        assert_eq!(app.track_label, "B - Dos");
        assert!(app.queue.is_empty(), "reproducir no encola");
    }

    #[test]
    fn playing_a_track_does_not_close_the_list_view() {
        let mut app = app_with_tree();
        expand_with_tracks(&mut app, 0, vec![tr("t1", "A - Uno")]);
        app.playlist_tree.selected = 1;
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        handle_tree_key_with(
            key(KeyCode::Enter),
            &mut app,
            no_playlists,
            no_tracks,
            |_, _| mpsc::channel().1,
            no_list,
        );
        assert!(app.explorer_open);
        assert!(app.tree_focus);
    }

    // --- encolar (Espacio) ---

    fn queued(app: &App) -> Vec<(String, String)> {
        app.queue
            .iter()
            .map(|item| match item {
                QueueItem::Url { url, title } => (url.clone(), title.clone()),
                other => panic!("se esperaba una URL en la cola: {}", other.label()),
            })
            .collect()
    }

    #[test]
    fn space_on_a_playlist_with_nothing_playing_starts_it_through_the_url_flow() {
        let mut app = app_with_tree();
        app.status = Status::Ended;
        app.playlist_tree.selected = 1;
        let listed = RefCell::new(Vec::new());
        assert!(handle_tree_key_with(
            key(space()),
            &mut app,
            no_playlists,
            no_tracks,
            no_resolve,
            |url, _| {
                listed.borrow_mut().push(url);
                mpsc::channel().1
            },
        ));
        assert_eq!(
            listed.borrow().as_slice(),
            &["spotify:playlist:p2".to_string()]
        );
        assert_eq!(app.track_label, "Jazz");
        assert_eq!(
            app.current_url,
            Some(("spotify:playlist:p2".to_string(), "Jazz".to_string()))
        );
    }

    #[test]
    fn space_on_a_loaded_playlist_while_playing_appends_all_its_tracks_to_the_queue() {
        let mut app = app_with_tree();
        app.status = Status::Playing;
        app.queue.push(QueueItem::Url {
            url: "spotify:track:antes".into(),
            title: "Antes".into(),
        });
        expand_with_tracks(&mut app, 0, vec![tr("t1", "A - Uno"), tr("t2", "B - Dos")]);
        app.playlist_tree.selected = 0;
        assert!(press(&mut app, space()), "sin tocar la red ni lo que suena");
        assert_eq!(
            queued(&app),
            vec![
                ("spotify:track:antes".to_string(), "Antes".to_string()),
                ("spotify:track:t1".to_string(), "A - Uno".to_string()),
                ("spotify:track:t2".to_string(), "B - Dos".to_string()),
            ]
        );
        assert_eq!(app.status, Status::Playing);
        assert!(app.current_url.is_none(), "no arrancó otra reproducción");
    }

    #[test]
    fn space_on_an_unloaded_playlist_loads_its_tracks_and_queues_them_when_they_arrive() {
        let mut app = app_with_tree();
        app.status = Status::Paused;
        app.playlist_tree.selected = 2; // Mix
        let calls = RefCell::new(Vec::new());
        assert!(press_logging_tracks(&mut app, space(), &calls));
        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(calls.borrow()[0].0, "p3");
        assert!(
            !app.playlist_tree.playlists[2].expanded,
            "encolar no despliega la playlist"
        );
        assert!(app.queue.is_empty(), "todavía no llegaron");

        // Otro Espacio mientras carga no repite el pedido ni duplica la cola.
        assert!(press(&mut app, space()));

        let generation = app.playlist_tree.generation;
        handle_tree_result(
            &mut app,
            generation,
            TreePayload::Tracks {
                playlist_id: "p3".into(),
                result: Ok(vec![tr("t1", "A - Uno"), tr("t2", "B - Dos")]),
            },
        );
        assert_eq!(
            queued(&app),
            vec![
                ("spotify:track:t1".to_string(), "A - Uno".to_string()),
                ("spotify:track:t2".to_string(), "B - Dos".to_string()),
            ]
        );
        // Quedan cargadas: un segundo Espacio las encola sin pedir nada.
        press(&mut app, space());
        assert_eq!(app.queue.len(), 4);
    }

    #[test]
    fn a_failed_load_for_a_queued_playlist_shows_the_error_and_queues_nothing() {
        let mut app = app_with_tree();
        app.status = Status::Playing;
        app.playlist_tree.selected = 0;
        let calls = RefCell::new(Vec::new());
        press_logging_tracks(&mut app, space(), &calls);
        let generation = app.playlist_tree.generation;
        handle_tree_result(
            &mut app,
            generation,
            TreePayload::Tracks {
                playlist_id: "p1".into(),
                result: Err("Spotify limitó la tasa de peticiones".into()),
            },
        );
        assert!(app.queue.is_empty());
        assert_eq!(
            app.error.as_deref(),
            Some("Spotify limitó la tasa de peticiones")
        );
        // Un reintento posterior no encola por su cuenta lo que ya falló.
        assert!(app.playlist_tree.pending_enqueue.is_empty());
    }

    #[test]
    fn space_on_an_empty_playlist_says_so() {
        let mut app = app_with_tree();
        app.status = Status::Playing;
        expand_with_tracks(&mut app, 0, Vec::new());
        app.playlist_tree.playlists[0].expanded = false;
        app.playlist_tree.selected = 0;
        press(&mut app, space());
        assert!(app.queue.is_empty());
        assert!(app.error.is_some());
    }

    #[test]
    fn space_on_a_track_appends_it_to_the_end_of_the_queue_without_playing_it() {
        let mut app = app_with_tree();
        expand_with_tracks(&mut app, 0, vec![tr("t1", "A - Uno"), tr("t2", "B - Dos")]);
        app.playlist_tree.selected = 2; // t2
        for status in [Status::Playing, Status::Ended] {
            app.queue.clear();
            app.status = status;
            assert!(press(&mut app, space()), "sin resolver ni listar");
            assert_eq!(
                queued(&app),
                vec![("spotify:track:t2".to_string(), "B - Dos".to_string())]
            );
        }
    }

    #[test]
    fn space_on_a_status_row_does_nothing() {
        let mut app = app_with_tree();
        app.playlist_tree.playlists[0].expanded = true;
        app.playlist_tree.playlists[0].tracks = TrackState::Loading;
        app.playlist_tree.selected = 1; // fila de estado
        assert!(press(&mut app, space()));
        assert!(app.queue.is_empty());
    }

    // --- estados sin datos ---

    #[test]
    fn enter_on_a_failed_tree_retries_and_other_states_ignore_the_keys() {
        let mut app = test_app();
        app.tree_focus = true;
        app.playlist_tree.load = TreeLoad::Failed("sin sesión".into());
        let calls = RefCell::new(0);
        assert!(handle_tree_key_with(
            key(KeyCode::Enter),
            &mut app,
            |_, _| *calls.borrow_mut() += 1,
            no_tracks,
            no_resolve,
            no_list,
        ));
        assert_eq!(*calls.borrow(), 1);
        assert_eq!(app.playlist_tree.load, TreeLoad::Loading);

        // Cargando: las teclas del árbol se consumen pero no hacen nada (ni
        // reintentan ni caen sobre la lista de locales).
        assert!(press(&mut app, KeyCode::Enter));
        assert!(press(&mut app, KeyCode::Down));
        assert_eq!(app.playlist_tree.load, TreeLoad::Loading);
    }

    // --- render ---

    // --- teclas de la app: `l` y `v` ---

    fn player_app() -> App {
        let mut app = app_with_tree();
        app.tree_focus = false;
        app.mode = Mode::Player;
        app.has_player = true;
        app
    }

    #[test]
    fn l_opens_the_list_view_and_focuses_the_tree_from_the_player_view() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = player_app();
        assert!(!app.explorer_open);
        let l = config::config().keys.spotify_library[0];
        handle_player_key(key(l), &mut app, &mut engine);
        assert!(app.explorer_open, "se abre la vista de lista");
        assert_eq!(focused_panel(&app), ListPanel::Playlists);
    }

    #[test]
    fn l_focuses_the_tree_from_the_list_view_and_from_the_bare_browser() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let l = config::config().keys.spotify_library[0];
        let mut app = player_app();
        app.explorer_open = true;
        app.queue_panel = true;
        handle_list_key(key(l), &mut app, &mut engine);
        assert_eq!(focused_panel(&app), ListPanel::Playlists);
        assert!(!app.queue_panel);

        // Sin player no hay vista alterna: la lista ya es la pantalla.
        let mut bare = app_with_tree();
        bare.tree_focus = false;
        handle_list_key(key(l), &mut bare, &mut engine);
        assert_eq!(focused_panel(&bare), ListPanel::Playlists);
    }

    #[test]
    fn v_toggles_between_the_list_view_and_the_player_view() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let v = config::config().keys.player_view[0];
        let mut app = player_app();

        handle_player_key(key(v), &mut app, &mut engine);
        assert!(app.explorer_open, "v abre la vista de lista");
        handle_list_key(key(v), &mut app, &mut engine);
        assert!(!app.explorer_open, "v vuelve al reproductor");
        assert!(!app.tree_focus, "al volver, el foco vuelve a Locales");
        assert_eq!(app.mode, Mode::Player);
    }

    #[test]
    fn with_the_tree_focused_enqueue_and_delete_do_not_touch_other_lists() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = player_app();
        app.explorer_open = true;
        app.tree_focus = true;
        app.queue.push(QueueItem::Local(track_named("uno")));
        for code in ['a', 'd'] {
            handle_list_key(key(KeyCode::Char(code)), &mut app, &mut engine);
        }
        assert_eq!(app.queue.len(), 1, "ni encola ni borra");
    }

    // --- layout y pantalla ---

    fn screen_text_sized(app: &App, engine: &AudioEngine, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
        terminal.draw(|frame| render(frame, app, engine)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_list_view_shows_locales_playlists_and_queue_side_by_side() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = player_app();
        app.explorer_open = true;
        app.tree_focus = true;
        app.track_label = "Daft Punk - One More Time".into();
        app.queue.push(QueueItem::Local(track_named("en_cola")));
        let screen = screen_text_sized(&app, &engine, 120, 36);
        let top: Vec<&str> = screen.lines().collect();

        // Línea compacta con la canción actual arriba de todo.
        assert!(top[0].contains("One More Time"), "{}", top[0]);
        // Los tres títulos van en la misma fila, en este orden.
        let border = top[1];
        let (locales, playlists, cola) = (
            border.find("Locales"),
            border.find("Playlists"),
            border.find("Cola"),
        );
        assert!(
            matches!((locales, playlists, cola), (Some(a), Some(b), Some(c)) if a < b && b < c),
            "{border}"
        );
        // Las filas del árbol (con su indicador) y la cola se ven.
        assert!(
            screen.contains("Rock") && screen.contains("pub"),
            "{screen}"
        );
        assert!(
            screen.contains("Jazz") && screen.contains("priv"),
            "{screen}"
        );
        assert!(screen.contains("en_cola"), "{screen}");
        // El pie conserva los atajos y suma la tecla de vista.
        assert!(screen.contains("v: reproductor"), "{screen}");
        assert!(screen.contains("plegar playlist"), "{screen}");
    }

    #[test]
    fn a_narrow_terminal_shows_only_the_focused_column() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = player_app();
        app.explorer_open = true;
        app.tree_focus = true;
        let screen = screen_text_sized(&app, &engine, 50, 30);
        assert!(screen.contains("Rock"), "{screen}");
        assert!(!screen.contains("Locales"), "{screen}");
        assert!(!screen.contains(" Cola"), "{screen}");
    }

    #[test]
    fn the_bare_browser_without_a_player_also_gets_the_three_columns() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = app_with_tree();
        app.mode = Mode::Browser;
        app.has_player = false;
        let screen = screen_text_sized(&app, &engine, 120, 36);
        assert!(screen.contains("Locales"), "{screen}");
        assert!(screen.contains("Playlists"), "{screen}");
        assert!(screen.contains("Cola"), "{screen}");
        assert!(screen.contains("Rock"), "{screen}");
    }

    #[test]
    fn the_list_view_has_no_player_mouse_zones_but_the_player_view_does() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = player_app();
        app.explorer_open = true;
        screen_text_sized(&app, &engine, 120, 36);
        let hit = app.hit.borrow().clone();
        assert!(hit.player.is_none(), "la rueda no cambia el volumen");
        assert!(hit.buttons.is_empty() && hit.progress.is_none() && hit.volume.is_none());
        drop(hit);

        app.explorer_open = false;
        screen_text_sized(&app, &engine, 120, 36);
        assert!(app.hit.borrow().player.is_some());
    }

    #[test]
    fn the_player_view_is_untouched_by_the_tree() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let app = player_app();
        let screen = screen_text_sized(&app, &engine, 100, 30);
        assert!(screen.contains("Volumen"), "{screen}");
        assert!(!screen.contains("Playlists"), "{screen}");
        assert!(screen.contains("Tab/v: lista"), "{screen}");
    }

    #[test]
    fn the_compact_now_playing_line_shows_state_title_and_progress() {
        let line = now_playing_line(
            "Daft Punk - One More Time",
            Status::Playing,
            Duration::from_secs(83),
            Some(Duration::from_secs(240)),
            60,
        );
        let text = line_text(&line);
        assert!(text.contains(PLAYING), "{text}");
        assert!(text.contains("Daft Punk - One More Time"), "{text}");
        assert!(text.ends_with("1:23 / 4:00"), "{text}");
        assert!(line.width() <= 60, "{text}");

        // Sin duración conocida solo el tiempo; título largo cede ancho.
        let line = now_playing_line(
            &"x".repeat(100),
            Status::Paused,
            Duration::from_secs(5),
            None,
            30,
        );
        assert!(line.width() <= 30, "{}", line_text(&line));
        assert!(line_text(&line).ends_with("0:05"), "{}", line_text(&line));

        let idle = line_text(&now_playing_line(
            "",
            Status::Ended,
            Duration::ZERO,
            None,
            30,
        ));
        assert!(idle.contains('—'), "{idle}");
    }

    fn footer_text(app: &App) -> String {
        footer_lines(app, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_footer_explains_the_keys_of_the_focused_column() {
        let mut app = player_app();
        app.explorer_open = true;
        let local = footer_text(&app);
        assert!(local.contains("espacio: encolar"), "{local}");
        assert!(local.contains("Enter: reproducir"), "{local}");

        app.tree_focus = true;
        let tree = footer_text(&app);
        assert!(
            tree.contains("Enter: plegar playlist / reproducir canción"),
            "{tree}"
        );
        assert!(tree.contains("espacio: encolar"), "{tree}");

        app.tree_focus = false;
        app.queue_panel = true;
        let queue = footer_text(&app);
        assert!(queue.contains("borrar"), "{queue}");
        assert!(!queue.contains("espacio"), "{queue}");

        for panel in [(false, false), (true, false), (false, true)] {
            app.tree_focus = panel.0;
            app.queue_panel = panel.1;
            for line in footer_lines(&app, false) {
                assert!(line.width() <= 78, "{}", line_text(&line));
            }
        }
    }

    #[test]
    fn no_footer_announces_player_controls_where_they_do_not_work() {
        let mut app = player_app();
        // Lista (con player y sin player) y todas las columnas.
        for (mode, explorer) in [(Mode::Player, true), (Mode::Browser, false)] {
            app.mode = mode;
            app.explorer_open = explorer;
            for panel in [(false, false), (true, false), (false, true)] {
                app.tree_focus = panel.0;
                app.queue_panel = panel.1;
                let text = footer_text(&app);
                for hint in ["pausa", "±5s", "n/p", "vel", "shuffle", "repeat"] {
                    assert!(!text.contains(hint), "{mode:?} {panel:?} '{hint}':\n{text}");
                }
                assert!(text.contains("←/→: columna"), "{text}");
            }
        }
        // La vista Reproductor sí los anuncia.
        app.mode = Mode::Player;
        app.explorer_open = false;
        app.tree_focus = false;
        let player = footer_text(&app);
        for hint in ["pausa", "±5s", "pista", "shuffle", "repeat"] {
            assert!(player.contains(hint), "'{hint}':\n{player}");
        }
    }

    #[test]
    fn the_focused_column_has_an_accent_border_and_the_others_a_dim_one() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = player_app();
        app.explorer_open = true;
        let border_colors = |app: &App| {
            let backend = ratatui::backend::TestBackend::new(120, 36);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, app, &engine)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            // Esquina superior izquierda de cada columna (fila 1, bajo la
            // línea de reproducción).
            [0u16, 40, 80].map(|x| buffer[(x, 1)].fg)
        };
        let (accent, dim) = (theme().accent, theme().dim);
        assert_eq!(border_colors(&app), [accent, dim, dim], "Locales");
        app.tree_focus = true;
        assert_eq!(border_colors(&app), [dim, accent, dim], "Playlists");
        app.tree_focus = false;
        app.queue_panel = true;
        assert_eq!(border_colors(&app), [dim, dim, accent], "Cola");
    }

    #[test]
    fn each_state_without_playlists_has_its_own_message() {
        let mut tree = PlaylistTree::new();
        let idle = text_of(&playlist_tree_lines(&tree, 40, 10));
        assert!(idle.contains("l"), "{idle}");
        assert!(idle.contains("playlists"), "{idle}");

        tree.load = TreeLoad::Loading;
        assert!(text_of(&playlist_tree_lines(&tree, 40, 10)).contains("Cargando"));

        tree.load =
            TreeLoad::Failed("no hay sesión de Spotify: corré `mp3tui --spotify-login`".into());
        let failed = text_of(&playlist_tree_lines(&tree, 80, 10));
        assert!(failed.contains("mp3tui --spotify-login"), "{failed}");

        tree.load = TreeLoad::Ready;
        let empty = text_of(&playlist_tree_lines(&tree, 40, 10));
        assert!(empty.contains("No tenés playlists"), "{empty}");
    }

    #[test]
    fn every_row_shows_public_private_or_unknown() {
        let mut app = app_with_tree();
        app.playlist_tree.playlists[0].collaborative = true;
        let text = text_of(&playlist_tree_lines(&app.playlist_tree, 40, 10));
        let rows: Vec<&str> = text.lines().collect();
        assert!(
            rows[0].contains("Rock") && rows[0].contains("pub"),
            "{text}"
        );
        assert!(rows[0].contains("colab"), "{text}");
        assert!(
            rows[1].contains("Jazz") && rows[1].contains("priv"),
            "{text}"
        );
        assert!(rows[2].contains("Mix") && rows[2].contains('?'), "{text}");
        assert!(
            !rows[2].contains("pub") && !rows[2].contains("priv"),
            "{text}"
        );
    }

    #[test]
    fn a_long_track_error_wraps_under_its_playlist_without_exceeding_the_width() {
        let mut app = app_with_tree();
        let playlist = &mut app.playlist_tree.playlists[0];
        playlist.expanded = true;
        playlist.tracks =
            TrackState::Failed("Spotify limitó la tasa de peticiones; reintentar en 30 s".into());
        let lines = playlist_tree_lines(&app.playlist_tree, 24, 10);
        let text = text_of(&lines);
        assert!(lines.iter().all(|line| line.width() <= 24), "{text}");
        assert!(text.contains("limitó"), "{text}");
        assert!(text.contains("30 s"), "el mensaje entero se lee:\n{text}");
    }

    #[test]
    fn the_arrow_tells_collapsed_from_expanded() {
        let mut app = app_with_tree();
        expand_with_tracks(&mut app, 1, vec![tr("t1", "A - Uno")]);
        let text = text_of(&playlist_tree_lines(&app.playlist_tree, 40, 10));
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows[0].contains('▸'), "{text}");
        assert!(rows[1].contains('▾'), "{text}");
    }

    #[test]
    fn rows_never_exceed_the_width_and_the_window_follows_the_selection() {
        let mut app = test_app();
        app.playlist_tree.load = TreeLoad::Ready;
        app.playlist_tree.playlists = (0..30)
            .map(|i| {
                node(
                    &format!("p{i}"),
                    &format!("Playlist con nombre largo número {i}"),
                    Some(true),
                )
            })
            .collect();
        app.playlist_tree.selected = 25;
        for width in [12, 20, 26, 60] {
            let lines = playlist_tree_lines(&app.playlist_tree, width, 6);
            assert!(lines.len() <= 6, "{} filas", lines.len());
            assert!(
                lines.iter().all(|line| line.width() <= width),
                "ancho {width}: {}",
                text_of(&lines)
            );
        }
        let text = text_of(&playlist_tree_lines(&app.playlist_tree, 60, 6));
        assert!(
            text.contains("número 25"),
            "la ventana sigue la selección:\n{text}"
        );
        assert!(!text.contains("número 0 "), "{text}");
    }
}
