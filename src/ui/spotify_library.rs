//! Panel «Mis playlists» de Spotify: un modal con tus playlists
//! (`my_playlists`) y, al elegir una, la lista de sus temas. Se puede
//! reproducir la playlist entera (flujo de encolado de siempre) o un tema
//! suelto, sin volcar nada a la cola por defecto.
//!
//! La red corre en hilos aparte (patrón de `spotify_search`): la UI manda el
//! pedido y recoge el resultado por un channel con `request_id`.

use super::*;

use crate::source::spotify::play::{self, ConnectError, LibrarySource};

/// Aviso cuando el hilo de carga muere sin contestar.
const LIBRARY_ABORTED_MSG: &str = "La carga terminó de forma inesperada; probá de nuevo";

/// Una playlist del usuario lista para mostrar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LibraryPlaylist {
    pub(super) id: String,
    pub(super) name: String,
    /// «Dueño · N canciones», con lo que haya.
    pub(super) detail: Option<String>,
}

/// Un tema de una playlist: `label` = «Artistas - Título».
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LibraryTrack {
    pub(super) id: String,
    pub(super) label: String,
}

/// Qué muestra el panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LibraryView {
    /// Pidiendo las playlists.
    Loading,
    /// Falló la carga de playlists: mensaje listo para mostrar.
    Failed(String),
    /// Lista de playlists (la selección vive en `SpotifyLibrary::selected`).
    Playlists,
    /// Pidiendo los temas de una playlist.
    LoadingTracks(LibraryPlaylist),
    /// Falló la carga de los temas de esa playlist.
    TracksFailed(LibraryPlaylist, String),
    /// Temas de una playlist.
    Tracks {
        playlist: LibraryPlaylist,
        tracks: Vec<LibraryTrack>,
        selected: usize,
    },
}

/// Estado del panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SpotifyLibrary {
    pub(super) playlists: Vec<LibraryPlaylist>,
    /// Playlist seleccionada: se conserva al entrar y volver de sus temas.
    pub(super) selected: usize,
    pub(super) view: LibraryView,
}

/// Lo que devuelve un hilo de carga. Los errores ya son texto para pantalla.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LibraryPayload {
    Playlists(Result<Vec<LibraryPlaylist>, String>),
    Tracks {
        playlist_id: String,
        result: Result<Vec<LibraryTrack>, String>,
    },
}

/// Mensaje del hilo: id de pedido y resultado.
pub(super) type SpotifyLibraryMessage = (u64, LibraryPayload);

/// Playlists de la API a filas del panel (sin las que no tienen id).
pub(super) fn playlists_from(
    playlists: Vec<crate::source::spotify::client::SpotifyPlaylist>,
) -> Vec<LibraryPlaylist> {
    playlists
        .into_iter()
        .filter(|playlist| !playlist.id.is_empty())
        .map(|playlist| LibraryPlaylist {
            detail: playlist_detail(playlist.owner.as_deref(), playlist.tracks_total),
            id: playlist.id,
            name: playlist.name,
        })
        .collect()
}

/// Temas de la API a filas del panel (sin los archivos locales: no tienen id).
pub(super) fn tracks_from(
    tracks: Vec<crate::source::spotify::client::SpotifyTrack>,
) -> Vec<LibraryTrack> {
    tracks
        .iter()
        .filter(|track| !track.id.is_empty())
        .map(|track| LibraryTrack {
            id: track.id.clone(),
            label: play::track_label(track),
        })
        .collect()
}

/// Cuerpo del hilo que carga las playlists, con el cliente inyectado.
pub(super) fn run_load_playlists(
    connect: &dyn Fn() -> Result<Box<dyn LibrarySource>, ConnectError>,
) -> Result<Vec<LibraryPlaylist>, String> {
    play::user_playlists(connect).map(playlists_from)
}

/// Cuerpo del hilo que carga los temas de una playlist.
pub(super) fn run_load_tracks(
    id: &str,
    connect: &dyn Fn() -> Result<Box<dyn LibrarySource>, ConnectError>,
) -> Result<Vec<LibraryTrack>, String> {
    play::user_playlist_tracks(id, connect).map(tracks_from)
}

fn connect_library() -> Result<Box<dyn LibrarySource>, ConnectError> {
    play::connect(crate::config::config()).map(|client| Box::new(client) as Box<dyn LibrarySource>)
}

/// Único punto que lanza la carga de playlists: los tests inyectan un doble.
pub(super) fn spawn_load_playlists(req_id: u64) -> Receiver<SpotifyLibraryMessage> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = run_load_playlists(&connect_library);
        let _ = tx.send((req_id, LibraryPayload::Playlists(result)));
    });
    rx
}

/// Único punto que lanza la carga de los temas de una playlist.
pub(super) fn spawn_load_tracks(playlist_id: String, req_id: u64) -> Receiver<SpotifyLibraryMessage> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = run_load_tracks(&playlist_id, &connect_library);
        let _ = tx.send((req_id, LibraryPayload::Tracks { playlist_id, result }));
    });
    rx
}

/// Abre el panel y pide las playlists en segundo plano.
pub(super) fn open_spotify_library_with(
    app: &mut App,
    spawn_playlists: impl FnOnce(u64) -> Receiver<SpotifyLibraryMessage>,
) {
    app.spotify_library_id = app.spotify_library_id.wrapping_add(1);
    app.spotify_library_rx = Some(spawn_playlists(app.spotify_library_id));
    app.spotify_library = Some(SpotifyLibrary {
        playlists: Vec::new(),
        selected: 0,
        view: LibraryView::Loading,
    });
}

pub(super) fn open_spotify_library(app: &mut App) {
    open_spotify_library_with(app, spawn_load_playlists);
}

/// Descarta la carga en vuelo: su resultado, si llega, ya no coincide.
fn cancel_spotify_library(app: &mut App) {
    app.spotify_library_id = app.spotify_library_id.wrapping_add(1);
    app.spotify_library_rx = None;
}

fn close_spotify_library(app: &mut App) {
    cancel_spotify_library(app);
    app.spotify_library = None;
}

/// Aplica el resultado de un hilo de carga al panel.
pub(super) fn handle_spotify_library_result(
    app: &mut App,
    request_id: u64,
    payload: LibraryPayload,
) {
    if request_id != app.spotify_library_id {
        return;
    }
    let Some(library) = app.spotify_library.as_mut() else {
        return;
    };
    match (payload, &library.view) {
        (LibraryPayload::Playlists(result), LibraryView::Loading) => match result {
            Ok(playlists) => {
                library.playlists = playlists;
                library.selected = 0;
                library.view = LibraryView::Playlists;
            }
            Err(message) => library.view = LibraryView::Failed(message),
        },
        (LibraryPayload::Tracks { playlist_id, result }, LibraryView::LoadingTracks(playlist))
            if playlist.id == playlist_id =>
        {
            let playlist = playlist.clone();
            library.view = match result {
                Ok(tracks) => LibraryView::Tracks {
                    playlist,
                    tracks,
                    selected: 0,
                },
                Err(message) => LibraryView::TracksFailed(playlist, message),
            };
        }
        _ => {}
    }
}

/// Recoge el resultado del hilo de carga sin bloquear (lo llama el event loop
/// en cada frame).
pub(super) fn poll_spotify_library(app: &mut App) {
    let Some(rx) = &app.spotify_library_rx else {
        return;
    };
    match rx.try_recv() {
        Ok((request_id, payload)) => {
            app.spotify_library_rx = None;
            handle_spotify_library_result(app, request_id, payload);
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            app.spotify_library_rx = None;
            if let Some(library) = app.spotify_library.as_mut() {
                library.view = match std::mem::replace(&mut library.view, LibraryView::Loading) {
                    LibraryView::Loading => LibraryView::Failed(LIBRARY_ABORTED_MSG.to_string()),
                    LibraryView::LoadingTracks(playlist) => {
                        LibraryView::TracksFailed(playlist, LIBRARY_ABORTED_MSG.to_string())
                    }
                    other => other,
                };
            }
        }
    }
}

/// Pide los temas de `playlist` y pasa a «cargando temas».
fn load_tracks(
    app: &mut App,
    playlist: LibraryPlaylist,
    spawn_tracks: impl FnOnce(String, u64) -> Receiver<SpotifyLibraryMessage>,
) {
    cancel_spotify_library(app);
    app.spotify_library_rx = Some(spawn_tracks(playlist.id.clone(), app.spotify_library_id));
    if let Some(library) = app.spotify_library.as_mut() {
        library.view = LibraryView::LoadingTracks(playlist);
    }
}

/// Cierra el panel y reproduce por el flujo de URLs: el marcador
/// `spotify:playlist:<id>` / `spotify:track:<id>` ya lo entiende.
fn play_marker_and_close(
    app: &mut App,
    marker: String,
    label: &str,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) {
    close_spotify_library(app);
    play_spotify_marker(app, marker, label, resolve_page, list_playlist);
}

/// Teclas del panel con los efectos externos inyectados.
pub(super) fn handle_spotify_library_key_with(
    key: KeyEvent,
    app: &mut App,
    spawn_playlists: impl FnOnce(u64) -> Receiver<SpotifyLibraryMessage>,
    spawn_tracks: impl FnOnce(String, u64) -> Receiver<SpotifyLibraryMessage>,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) {
    let Some(library) = app.spotify_library.as_ref() else {
        return;
    };
    let view = library.view.clone();
    let selected_playlist = library.playlists.get(library.selected).cloned();
    let playlists_len = library.playlists.len();
    let playlists_selected = library.selected;
    let code = key.code;
    let keys = &config::config().keys;
    let wants_close = code == KeyCode::Esc || config::pressed(&keys.back, code);
    let wants_select = config::pressed(&keys.select, code);
    let wants_play_all = config::pressed(&keys.play_pause, code);
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

    match view {
        LibraryView::Loading => {
            if wants_close {
                close_spotify_library(app);
            }
        }
        LibraryView::Failed(_) => {
            if wants_close {
                close_spotify_library(app);
            } else if wants_select || config::pressed(&keys.spotify_library, code) {
                open_spotify_library_with(app, spawn_playlists);
            }
        }
        LibraryView::Playlists => {
            if wants_close {
                close_spotify_library(app);
            } else if let Some(delta) = delta {
                if let Some(library) = app.spotify_library.as_mut() {
                    library.selected = move_selection(playlists_selected, delta, playlists_len);
                }
            } else if wants_select {
                if let Some(playlist) = selected_playlist {
                    load_tracks(app, playlist, spawn_tracks);
                }
            } else if wants_play_all && let Some(playlist) = selected_playlist {
                play_marker_and_close(
                    app,
                    format!("spotify:playlist:{}", playlist.id),
                    &playlist.name,
                    resolve_page,
                    list_playlist,
                );
            }
        }
        LibraryView::LoadingTracks(_) => {
            if wants_close {
                cancel_spotify_library(app);
                if let Some(library) = app.spotify_library.as_mut() {
                    library.view = LibraryView::Playlists;
                }
            }
        }
        LibraryView::TracksFailed(playlist, _) => {
            if wants_close {
                if let Some(library) = app.spotify_library.as_mut() {
                    library.view = LibraryView::Playlists;
                }
            } else if wants_select {
                load_tracks(app, playlist, spawn_tracks);
            }
        }
        LibraryView::Tracks {
            playlist,
            tracks,
            selected,
        } => {
            if wants_close {
                if let Some(library) = app.spotify_library.as_mut() {
                    library.view = LibraryView::Playlists;
                }
            } else if let Some(delta) = delta {
                let moved = move_selection(selected, delta, tracks.len());
                if let Some(LibraryView::Tracks { selected, .. }) =
                    app.spotify_library.as_mut().map(|l| &mut l.view)
                {
                    *selected = moved;
                }
            } else if wants_select {
                if let Some(track) = tracks.get(selected) {
                    play_marker_and_close(
                        app,
                        play::track_marker(&track.id),
                        &track.label,
                        resolve_page,
                        list_playlist,
                    );
                }
            } else if wants_play_all {
                play_marker_and_close(
                    app,
                    format!("spotify:playlist:{}", playlist.id),
                    &playlist.name,
                    resolve_page,
                    list_playlist,
                );
            }
        }
    }
}

pub(super) fn handle_spotify_library_key(key: KeyEvent, app: &mut App) {
    handle_spotify_library_key_with(
        key,
        app,
        spawn_load_playlists,
        spawn_load_tracks,
        spawn_ytdlp_resolution,
        spawn_ytdlp_listing,
    );
}

// --- render ----------------------------------------------------------------

/// Filas visibles de la lista del panel.
const LIBRARY_MODAL_MAX_ROWS: usize = 10;

/// Filas de una lista seleccionable: etiqueta y, si cabe, un detalle atenuado.
fn list_rows<'a>(
    items: impl Iterator<Item = (&'a str, Option<&'a str>)>,
    len: usize,
    selected: usize,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let colors = theme();
    let dim = Style::default().fg(colors.dim);
    let text = Style::default().fg(colors.text);
    let offset = scroll_offset(selected, len, max_rows);
    let mut lines = Vec::new();
    for (index, (label, detail)) in items.enumerate().skip(offset).take(max_rows) {
        let is_selected = index == selected;
        let row_style = if is_selected {
            Style::default()
                .bg(colors.select_bg)
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            text
        };
        let cursor = if is_selected { CURSOR } else { " " };
        let available = width.saturating_sub(2);
        let label = truncate_chars(label, available);
        let remaining = available.saturating_sub(label.chars().count());
        let mut spans = vec![
            Span::styled(format!("{cursor} "), if is_selected { row_style } else { dim }),
            Span::styled(label, row_style),
        ];
        if let Some(detail) = detail
            && remaining > 3
        {
            spans.push(Span::styled(
                truncate_chars(&format!("  {detail}"), remaining),
                if is_selected { row_style } else { dim },
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Título del recuadro según la vista.
fn library_title(library: &SpotifyLibrary) -> String {
    match &library.view {
        LibraryView::LoadingTracks(playlist)
        | LibraryView::TracksFailed(playlist, _)
        | LibraryView::Tracks { playlist, .. } => format!(" {} ", playlist.name),
        _ => " Mis playlists de Spotify ".to_string(),
    }
}

/// Filas del panel: estado en la primera y, con datos, la lista.
pub(super) fn spotify_library_lines(
    library: &SpotifyLibrary,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let colors = theme();
    let dim = Style::default().fg(colors.dim);
    let status = |text: String, color| Line::from(Span::styled(text, Style::default().fg(color)));
    match &library.view {
        LibraryView::Loading => vec![status(" Cargando tus playlists…".into(), colors.warn)],
        LibraryView::Failed(message) => vec![status(format!(" {message}"), colors.error)],
        LibraryView::Playlists => {
            let summary = match library.playlists.len() {
                0 => " No tenés playlists en tu cuenta".to_string(),
                1 => " 1 playlist".to_string(),
                n => format!(" {n} playlists"),
            };
            let mut lines = vec![Line::from(Span::styled(truncate_chars(&summary, width), dim))];
            lines.extend(list_rows(
                library
                    .playlists
                    .iter()
                    .map(|p| (p.name.as_str(), p.detail.as_deref())),
                library.playlists.len(),
                library.selected,
                width,
                max_rows,
            ));
            lines
        }
        LibraryView::LoadingTracks(_) => vec![status(" Cargando temas…".into(), colors.warn)],
        LibraryView::TracksFailed(_, message) => {
            vec![status(format!(" {message}"), colors.error)]
        }
        LibraryView::Tracks {
            tracks, selected, ..
        } => {
            let summary = match tracks.len() {
                0 => " Esta playlist no tiene temas reproducibles".to_string(),
                1 => " 1 tema".to_string(),
                n => format!(" {n} temas"),
            };
            let mut lines = vec![Line::from(Span::styled(truncate_chars(&summary, width), dim))];
            lines.extend(list_rows(
                tracks.iter().map(|t| (t.label.as_str(), None)),
                tracks.len(),
                *selected,
                width,
                max_rows,
            ));
            lines
        }
    }
}

/// Modal centrado del panel, al estilo del de búsqueda.
pub(super) fn render_spotify_library(frame: &mut Frame<'_>, library: &SpotifyLibrary) {
    let area = frame.area();
    let width = 72.min(area.width.saturating_sub(4)).max(20);
    let inner_width = usize::from(width.saturating_sub(2)).max(1);
    let max_rows = LIBRARY_MODAL_MAX_ROWS
        .min(usize::from(area.height).saturating_sub(6))
        .max(1);
    let lines = spotify_library_lines(library, inner_width, max_rows);
    let rows: usize = lines
        .iter()
        .map(|line| {
            let chars: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            chars.max(1).div_ceil(inner_width)
        })
        .sum();
    let height = u16::try_from(rows + 2)
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(2))
        .max(3);

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height).intersection(area);

    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::default().fg(theme().accent))
        .title(Span::styled(
            library_title(library),
            Style::default()
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

/// Pie de atajos del panel, según la vista.
pub(super) fn spotify_library_footer(library: &SpotifyLibrary) -> Vec<Line<'static>> {
    let keys = &config::config().keys;
    match &library.view {
        LibraryView::Loading | LibraryView::LoadingTracks(_) => vec![
            Line::from(" Cargando desde Spotify… "),
            Line::from(format!(" {}: cancelar ", key_labels(&keys.back))),
        ],
        LibraryView::Failed(_) | LibraryView::TracksFailed(..) => vec![
            Line::from(" No se pudo cargar: el motivo está en el recuadro "),
            Line::from(format!(
                " {}: reintentar   {}: {} ",
                key_labels(&keys.select),
                key_labels(&keys.back),
                if matches!(library.view, LibraryView::Failed(_)) {
                    "cerrar"
                } else {
                    "volver"
                },
            )),
        ],
        LibraryView::Playlists => vec![Line::from(format!(
            " {}/{}: mover   {}: ver temas   {}: reproducir toda   {}: cerrar ",
            key_labels(&keys.move_up),
            key_labels(&keys.move_down),
            key_labels(&keys.select),
            key_labels(&keys.play_pause),
            key_labels(&keys.back),
        ))],
        LibraryView::Tracks { .. } => vec![Line::from(format!(
            " {}/{}: mover   {}: reproducir tema   {}: reproducir toda   {}: volver ",
            key_labels(&keys.move_up),
            key_labels(&keys.move_down),
            key_labels(&keys.select),
            key_labels(&keys.play_pause),
            key_labels(&keys.back),
        ))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::spotify::client::{SpotifyError, SpotifyPlaylist, SpotifyTrack};
    use crate::source::spotify::play::LibrarySource;
    use crate::ui::tests_common::*;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn pl(id: &str, name: &str) -> LibraryPlaylist {
        LibraryPlaylist {
            id: id.to_string(),
            name: name.to_string(),
            detail: Some("yo · 2 canciones".to_string()),
        }
    }

    fn tr(id: &str, label: &str) -> LibraryTrack {
        LibraryTrack {
            id: id.to_string(),
            label: label.to_string(),
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

    fn api_playlist(id: &str, name: &str, owner: Option<&str>, total: Option<u64>) -> SpotifyPlaylist {
        SpotifyPlaylist {
            id: id.to_string(),
            name: name.to_string(),
            owner: owner.map(str::to_string),
            tracks_total: total,
        }
    }

    /// Carga de playlists que anota el id y devuelve un receiver ya resuelto
    /// con `payload`.
    fn spawn_playlists_ok(
        log: &RefCell<Vec<u64>>,
        payload: LibraryPayload,
    ) -> impl FnOnce(u64) -> Receiver<SpotifyLibraryMessage> + '_ {
        move |req_id| {
            log.borrow_mut().push(req_id);
            let (tx, rx) = mpsc::channel();
            tx.send((req_id, payload)).unwrap();
            rx
        }
    }

    fn silent_tracks(
        log: &RefCell<Vec<(String, u64)>>,
    ) -> impl FnOnce(String, u64) -> Receiver<SpotifyLibraryMessage> + '_ {
        move |id, req_id| {
            log.borrow_mut().push((id, req_id));
            let (_tx, rx) = mpsc::channel();
            rx
        }
    }

    fn no_playlists(_: u64) -> Receiver<SpotifyLibraryMessage> {
        panic!("no debía cargar playlists");
    }

    fn no_tracks(id: String, _: u64) -> Receiver<SpotifyLibraryMessage> {
        panic!("no debía cargar temas de {id}");
    }

    fn no_resolve(url: String, _: u64) -> Receiver<YtDlpMessage> {
        panic!("no debía resolver {url}");
    }

    fn no_list(url: String, _: u64) -> Receiver<YtDlpListMessage> {
        panic!("no debía listar {url}");
    }

    fn press(app: &mut App, code: KeyCode) {
        handle_spotify_library_key_with(
            key(code),
            app,
            no_playlists,
            no_tracks,
            no_resolve,
            no_list,
        );
    }

    fn app_with_playlists() -> App {
        let mut app = test_app();
        app.spotify_library = Some(SpotifyLibrary {
            playlists: vec![pl("p1", "Rock"), pl("p2", "Jazz")],
            selected: 0,
            view: LibraryView::Playlists,
        });
        app
    }

    fn app_with_tracks() -> App {
        let mut app = app_with_playlists();
        app.spotify_library.as_mut().unwrap().view = LibraryView::Tracks {
            playlist: pl("p1", "Rock"),
            tracks: vec![tr("t1", "A - Uno"), tr("t2", "B - Dos")],
            selected: 0,
        };
        app
    }

    fn view(app: &App) -> &LibraryView {
        &app.spotify_library.as_ref().unwrap().view
    }

    // --- abrir y cargar playlists ---

    #[test]
    fn opening_the_panel_starts_loading_in_the_background() {
        let mut app = test_app();
        let log = RefCell::new(Vec::new());
        open_spotify_library_with(
            &mut app,
            spawn_playlists_ok(&log, LibraryPayload::Playlists(Ok(vec![pl("p1", "Rock")]))),
        );
        assert_eq!(view(&app), &LibraryView::Loading);
        assert_eq!(log.borrow().as_slice(), &[app.spotify_library_id]);
        assert!(app.spotify_library_rx.is_some());
    }

    #[test]
    fn polling_fills_the_playlists_and_selects_the_first() {
        let mut app = test_app();
        let log = RefCell::new(Vec::new());
        open_spotify_library_with(
            &mut app,
            spawn_playlists_ok(
                &log,
                LibraryPayload::Playlists(Ok(vec![pl("p1", "Rock"), pl("p2", "Jazz")])),
            ),
        );
        poll_spotify_library(&mut app);
        let library = app.spotify_library.as_ref().unwrap();
        assert_eq!(library.view, LibraryView::Playlists);
        assert_eq!(library.playlists.len(), 2);
        assert_eq!(library.selected, 0);
        assert!(app.spotify_library_rx.is_none());
    }

    #[test]
    fn api_playlists_and_tracks_map_to_rows_skipping_the_ones_without_id() {
        let rows = playlists_from(vec![
            api_playlist("p1", "Mix", Some("yo"), Some(1)),
            api_playlist("", "Sin id", None, None),
        ]);
        assert_eq!(
            rows,
            vec![LibraryPlaylist {
                id: "p1".into(),
                name: "Mix".into(),
                detail: Some("yo · 1 canción".into()),
            }]
        );
        let tracks = tracks_from(vec![
            api_track("t1", "Song", &["A", "B"]),
            api_track("", "Local", &["X"]),
        ]);
        assert_eq!(tracks, vec![tr("t1", "A, B - Song")]);
    }

    // --- seleccionar una playlist y cargar sus temas ---

    #[test]
    fn selecting_a_playlist_loads_its_tracks_without_blocking() {
        let mut app = app_with_playlists();
        press(&mut app, KeyCode::Down);
        let log = RefCell::new(Vec::new());
        handle_spotify_library_key_with(
            key(KeyCode::Enter),
            &mut app,
            no_playlists,
            silent_tracks(&log),
            no_resolve,
            no_list,
        );
        assert_eq!(view(&app), &LibraryView::LoadingTracks(pl("p2", "Jazz")));
        assert_eq!(log.borrow().as_slice(), &[("p2".to_string(), app.spotify_library_id)]);

        let req_id = app.spotify_library_id;
        handle_spotify_library_result(
            &mut app,
            req_id,
            LibraryPayload::Tracks {
                playlist_id: "p2".into(),
                result: Ok(vec![tr("t1", "A - Uno")]),
            },
        );
        assert_eq!(
            view(&app),
            &LibraryView::Tracks {
                playlist: pl("p2", "Jazz"),
                tracks: vec![tr("t1", "A - Uno")],
                selected: 0,
            }
        );
    }

    #[test]
    fn stale_or_mismatched_results_are_ignored() {
        let mut app = app_with_playlists();
        let log = RefCell::new(Vec::new());
        handle_spotify_library_key_with(
            key(KeyCode::Enter),
            &mut app,
            no_playlists,
            silent_tracks(&log),
            no_resolve,
            no_list,
        );
        let id = app.spotify_library_id;
        // Id viejo.
        handle_spotify_library_result(
            &mut app,
            id.wrapping_sub(1),
            LibraryPayload::Tracks { playlist_id: "p1".into(), result: Ok(vec![tr("t", "x")]) },
        );
        // Otra playlist.
        handle_spotify_library_result(
            &mut app,
            id,
            LibraryPayload::Tracks { playlist_id: "p2".into(), result: Ok(vec![tr("t", "x")]) },
        );
        // Playlists llegando cuando ya estamos en temas.
        handle_spotify_library_result(&mut app, id, LibraryPayload::Playlists(Ok(vec![])));
        assert_eq!(view(&app), &LibraryView::LoadingTracks(pl("p1", "Rock")));
    }

    #[test]
    fn esc_while_loading_tracks_goes_back_and_drops_the_late_result() {
        let mut app = app_with_playlists();
        let log = RefCell::new(Vec::new());
        handle_spotify_library_key_with(
            key(KeyCode::Enter),
            &mut app,
            no_playlists,
            silent_tracks(&log),
            no_resolve,
            no_list,
        );
        let old_id = app.spotify_library_id;
        press(&mut app, KeyCode::Esc);
        assert_eq!(view(&app), &LibraryView::Playlists);
        handle_spotify_library_result(
            &mut app,
            old_id,
            LibraryPayload::Tracks { playlist_id: "p1".into(), result: Ok(vec![tr("t", "x")]) },
        );
        assert_eq!(view(&app), &LibraryView::Playlists);
    }

    #[test]
    fn navigation_moves_the_selection_and_esc_steps_back_then_closes() {
        let mut app = app_with_tracks();
        press(&mut app, KeyCode::Down);
        match view(&app) {
            LibraryView::Tracks { selected, .. } => assert_eq!(*selected, 1),
            other => panic!("{other:?}"),
        }
        press(&mut app, KeyCode::Esc);
        assert_eq!(view(&app), &LibraryView::Playlists);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.spotify_library.as_ref().unwrap().selected, 1);
        press(&mut app, KeyCode::Esc);
        assert!(app.spotify_library.is_none());
    }

    // --- reproducir ---

    #[test]
    fn space_on_a_tracks_view_plays_the_whole_playlist_through_the_url_flow() {
        let mut app = app_with_tracks();
        let resolve_calls = RefCell::new(Vec::<String>::new());
        let list_calls = RefCell::new(Vec::<String>::new());
        handle_spotify_library_key_with(
            key(KeyCode::Char(' ')),
            &mut app,
            no_playlists,
            no_tracks,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        assert!(app.spotify_library.is_none(), "el panel se cierra");
        assert_eq!(list_calls.borrow().as_slice(), &["spotify:playlist:p1"]);
        assert!(resolve_calls.borrow().is_empty());
        assert!(app.playlist_rx.is_some());
        assert_eq!(app.track_label, "Rock");
        assert!(app.queue.is_empty(), "no se vuelca nada a la cola por defecto");
    }

    #[test]
    fn space_on_the_playlist_list_plays_that_playlist_without_loading_tracks() {
        let mut app = app_with_playlists();
        press(&mut app, KeyCode::Down);
        let resolve_calls = RefCell::new(Vec::<String>::new());
        let list_calls = RefCell::new(Vec::<String>::new());
        handle_spotify_library_key_with(
            key(KeyCode::Char(' ')),
            &mut app,
            no_playlists,
            no_tracks,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        assert_eq!(list_calls.borrow().as_slice(), &["spotify:playlist:p2"]);
        assert_eq!(app.track_label, "Jazz");
    }

    #[test]
    fn enter_on_a_track_plays_only_that_track() {
        let mut app = app_with_tracks();
        press(&mut app, KeyCode::Down);
        let resolve_calls = RefCell::new(Vec::<String>::new());
        let list_calls = RefCell::new(Vec::<String>::new());
        handle_spotify_library_key_with(
            key(KeyCode::Enter),
            &mut app,
            no_playlists,
            no_tracks,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        assert!(app.spotify_library.is_none());
        assert_eq!(resolve_calls.borrow().as_slice(), &["spotify:track:t2"]);
        assert!(list_calls.borrow().is_empty());
        assert!(app.ytdlp_rx.is_some());
        assert_eq!(app.track_label, "B - Dos");
        assert!(app.queue.is_empty(), "no se vuelca nada a la cola");
    }

    #[test]
    fn enter_on_an_empty_tracks_view_does_nothing() {
        let mut app = app_with_playlists();
        app.spotify_library.as_mut().unwrap().view = LibraryView::Tracks {
            playlist: pl("p1", "Rock"),
            tracks: vec![],
            selected: 0,
        };
        press(&mut app, KeyCode::Enter);
        assert!(app.spotify_library.is_some());
    }

    // --- errores ---

    fn connect_err(err: ConnectError) -> impl Fn() -> Result<Box<dyn LibrarySource>, ConnectError> {
        move || Err(err.clone())
    }

    #[test]
    fn missing_session_and_client_id_show_a_clear_message() {
        let msg = run_load_playlists(&connect_err(ConnectError::NotLoggedIn)).unwrap_err();
        assert!(msg.contains("mp3tui --spotify-login"), "{msg}");
        let msg = run_load_tracks(
            "p1",
            &connect_err(ConnectError::ClientId(crate::source::spotify::MissingClientId)),
        )
        .unwrap_err();
        assert!(msg.contains("MP3TUI_SPOTIFY_CLIENT_ID"), "{msg}");
    }

    struct Limited;
    impl LibrarySource for Limited {
        fn my_playlists(&self) -> Result<Vec<SpotifyPlaylist>, SpotifyError> {
            Err(SpotifyError::RateLimited { retry_after_secs: Some(12) })
        }
        fn playlist_tracks(&self, _: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
            Err(SpotifyError::RateLimited { retry_after_secs: None })
        }
    }

    #[test]
    fn rate_limit_is_reported_on_screen_for_both_loads() {
        let connect = || Ok(Box::new(Limited) as Box<dyn LibrarySource>);
        let msg = run_load_playlists(&connect).unwrap_err();
        assert!(msg.contains("limitó la tasa") && msg.contains("12"), "{msg}");
        assert!(run_load_tracks("p", &connect).unwrap_err().contains("limitó la tasa"));
    }

    #[test]
    fn load_threads_use_the_injected_client_and_map_rows() {
        struct Fixed(Arc<Mutex<Vec<String>>>);
        impl LibrarySource for Fixed {
            fn my_playlists(&self) -> Result<Vec<SpotifyPlaylist>, SpotifyError> {
                self.0.lock().unwrap().push("my".into());
                Ok(vec![api_playlist("p1", "Mix", Some("yo"), Some(3))])
            }
            fn playlist_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
                self.0.lock().unwrap().push(format!("tracks:{id}"));
                Ok(vec![api_track("t1", "Song", &["A"])])
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&seen);
        let connect = move || Ok(Box::new(Fixed(Arc::clone(&shared))) as Box<dyn LibrarySource>);
        assert_eq!(run_load_playlists(&connect).unwrap()[0].name, "Mix");
        assert_eq!(run_load_tracks("p1", &connect).unwrap(), vec![tr("t1", "A - Song")]);
        assert_eq!(seen.lock().unwrap().as_slice(), &["my", "tracks:p1"]);
    }

    #[test]
    fn failed_view_shows_the_message_and_enter_retries() {
        let mut app = test_app();
        let log = RefCell::new(Vec::new());
        open_spotify_library_with(
            &mut app,
            spawn_playlists_ok(
                &log,
                LibraryPayload::Playlists(Err("no hay sesión de Spotify: corré `mp3tui --spotify-login`".into())),
            ),
        );
        poll_spotify_library(&mut app);
        assert!(matches!(view(&app), LibraryView::Failed(m) if m.contains("--spotify-login")));
        let lines = spotify_library_lines(app.spotify_library.as_ref().unwrap(), 70, 8);
        assert!(text_of(&lines).contains("--spotify-login"));

        let retry_log = RefCell::new(Vec::new());
        handle_spotify_library_key_with(
            key(KeyCode::Enter),
            &mut app,
            spawn_playlists_ok(&retry_log, LibraryPayload::Playlists(Ok(vec![pl("p1", "Rock")]))),
            no_tracks,
            no_resolve,
            no_list,
        );
        assert_eq!(view(&app), &LibraryView::Loading);
        assert_eq!(retry_log.borrow().len(), 1);
        poll_spotify_library(&mut app);
        assert_eq!(view(&app), &LibraryView::Playlists);
    }

    #[test]
    fn a_dead_loader_thread_becomes_a_visible_error() {
        let mut app = test_app();
        open_spotify_library_with(&mut app, |_| {
            let (_tx, rx) = mpsc::channel();
            rx
        });
        // El sender se soltó al salir del closure: el canal quedó desconectado.
        poll_spotify_library(&mut app);
        assert!(matches!(view(&app), LibraryView::Failed(_)), "{:?}", view(&app));
    }

    // --- teclas, config y render ---

    #[test]
    fn the_configured_key_opens_the_panel_from_the_list_and_the_player() {
        let keys = &config::config().keys;
        assert!(config::pressed(&keys.spotify_library, KeyCode::Char('l')));
        assert!(!config::pressed(&keys.spotify_search, KeyCode::Char('l')));
    }

    #[test]
    fn modal_swallows_keys_so_player_shortcuts_do_not_leak() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = app_with_playlists();
        dispatch_key(key(KeyCode::Char('q')), &mut app, &mut engine);
        dispatch_key(key(KeyCode::Char('u')), &mut app, &mut engine);
        assert!(!app.should_quit);
        assert!(app.spotify_search.is_none());
        assert!(app.spotify_library.is_some());
    }

    fn text_of(lines: &[Line<'_>]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn rows_show_name_owner_and_count_and_fit_the_width() {
        let app = app_with_playlists();
        let lines = spotify_library_lines(app.spotify_library.as_ref().unwrap(), 40, 8);
        let text = text_of(&lines);
        assert!(text.contains("2 playlists"), "{text}");
        assert!(text.contains("Rock") && text.contains("yo · 2 canciones"), "{text}");
        for line in &lines {
            assert!(line_width(line) <= 40, "{}", line_text(line));
        }
    }

    #[test]
    fn footers_name_the_keys_and_fit_78_columns() {
        for app in [app_with_playlists(), app_with_tracks()] {
            let mut app = app;
            let lines = footer_lines(&app, false);
            let text = text_of(&lines);
            assert!(text.contains("espacio: reproducir toda"), "{text}");
            assert!(text.contains("Enter:"), "{text}");
            for line in &lines {
                assert!(line_width(line) <= 78, "{}", line_text(line));
            }
            app.spotify_library.as_mut().unwrap().view = LibraryView::Failed("x".into());
            for line in footer_lines(&app, false) {
                assert!(line_width(&line) <= 78);
            }
        }
    }

    #[test]
    fn modal_renders_every_view_even_on_tiny_terminals() {
        let views = [
            LibraryView::Loading,
            LibraryView::Failed("un error largo ".repeat(20)),
            LibraryView::Playlists,
            LibraryView::LoadingTracks(pl("p1", "Rock")),
            LibraryView::TracksFailed(pl("p1", "Rock"), "429".into()),
            LibraryView::Tracks {
                playlist: pl("p1", "Rock"),
                tracks: (0..30).map(|i| tr(&format!("t{i}"), &format!("A - Tema {i}"))).collect(),
                selected: 25,
            },
        ];
        for view in views {
            for (w, h) in [(100, 30), (40, 8), (23, 10), (10, 2), (1, 1), (0, 0)] {
                let mut library = app_with_playlists().spotify_library.unwrap();
                library.view = view.clone();
                let backend = ratatui::backend::TestBackend::new(w, h);
                let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
                terminal
                    .draw(|frame| render_spotify_library(frame, &library))
                    .unwrap_or_else(|e| panic!("{view:?} {w}x{h}: {e}"));
            }
        }
    }

    #[test]
    fn regular_screens_advertise_the_panel_key_on_the_footer_border() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let app = test_app();
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
        terminal.draw(|frame| render(frame, &app, &engine)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(screen.contains("l: playlists"), "falta la pista del borde");
    }
}
