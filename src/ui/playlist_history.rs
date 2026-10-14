//! Historial de playlists y álbumes de Spotify: registro de lo que se puso
//! (`record_collection`) y un modal para volver a reproducirlo. Reproducirlo
//! de nuevo REEMPLAZA la cola, a diferencia del flujo normal que agrega al
//! final.

use super::*;

use crate::history::{CollectionKind, HistoryEntry};
use crate::source::spotify::play;

/// Nombre de reserva: solo si Spotify no entregó el nombre real y la sesión
/// tampoco traía uno elegido por el usuario.
const FALLBACK_PLAYLIST_NAME: &str = "Playlist de Spotify";
const FALLBACK_ALBUM_NAME: &str = "Álbum de Spotify";

/// Estado del modal del historial.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct HistoryModal {
    pub(super) selected: usize,
    /// Aviso de un intento fallido (p. ej. sin sesión de Spotify).
    pub(super) notice: Option<String>,
}

pub(super) fn open_history_modal(app: &mut App) {
    app.history_modal = Some(HistoryModal::default());
}

/// Registra la playlist o álbum de Spotify que acaba de listarse (la que
/// describe `app.current_url`). Otra cosa (una URL de YouTube) no se registra.
pub(super) fn record_collection(app: &mut App, collection: &crate::source::ytdlp::Collection) {
    let entries = &collection.entries;
    let Some((url, label)) = app.current_url.clone() else {
        return;
    };
    let Some(reference) = crate::source::spotify_ref(&url) else {
        return;
    };
    let (kind, fallback) = match reference.kind {
        crate::source::UrlKind::SpotifyPlaylist => {
            (CollectionKind::Playlist, FALLBACK_PLAYLIST_NAME)
        }
        crate::source::UrlKind::SpotifyAlbum => (CollectionKind::Album, FALLBACK_ALBUM_NAME),
        _ => return,
    };
    // El nombre real que dio Spotify manda; sin él, el que traía la sesión (lo
    // eligió el usuario en el buscador o el panel); y si la etiqueta es solo la
    // de la URL («<id>» o el marcador), el nombre de respaldo.
    let real = collection
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let name = match real {
        Some(name) => name.to_string(),
        None if label.trim().is_empty() || label == url_display_label(&url) => {
            fallback.to_string()
        }
        None => label,
    };
    let titles: Vec<String> = entries.iter().map(|entry| entry.title.clone()).collect();
    app.history
        .add(HistoryEntry::new(kind, reference.id, name, entries.len(), &titles));
    if let Some(path) = &app.history_path
        && let Err(err) = crate::history::save_to(&app.history, path)
    {
        app.error = Some(format!("Error al guardar historial: {err}"));
    }
}

/// Prueba la sesión de Spotify sin red: lee `client_id` y el archivo de
/// tokens (el mismo chequeo que hace el hilo antes de listar).
fn check_session() -> Result<(), String> {
    play::connect(config::config())
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// Teclas del modal con los efectos externos inyectados: `session` comprueba
/// la sesión y `resolve_page` / `list_playlist` arrancan la reproducción.
pub(super) fn handle_history_key_with(
    key: KeyEvent,
    app: &mut App,
    session: impl FnOnce() -> Result<(), String>,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) {
    let Some(modal) = app.history_modal.as_ref() else {
        return;
    };
    let selected = modal.selected;
    let len = app.history.len();
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

    if code == KeyCode::Esc || config::pressed(&keys.back, code) {
        app.history_modal = None;
    } else if let Some(delta) = delta {
        if let Some(modal) = app.history_modal.as_mut() {
            modal.selected = move_selection(selected, delta, len);
            modal.notice = None;
        }
    } else if config::pressed(&keys.select, code)
        && let Some(entry) = app.history.get(selected).cloned()
    {
        match session() {
            Err(message) => {
                if let Some(modal) = app.history_modal.as_mut() {
                    modal.notice = Some(message);
                }
            }
            Ok(()) => {
                app.history_modal = None;
                replay_entry(app, &entry, resolve_page, list_playlist);
            }
        }
    }
}

/// Vuelve a poner la colección: arranca el flujo de siempre y marca que el
/// listado, al llegar, REEMPLAZA la cola en vez de sumarse a ella.
fn replay_entry(
    app: &mut App,
    entry: &HistoryEntry,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) {
    play_spotify_marker(app, entry.marker(), &entry.name, resolve_page, list_playlist);
    app.replace_queue_req = Some(app.ytdlp_request_id);
}

pub(super) fn handle_history_key(key: KeyEvent, app: &mut App) {
    handle_history_key_with(
        key,
        app,
        check_session,
        spawn_ytdlp_resolution,
        spawn_ytdlp_listing,
    );
}

// --- render ----------------------------------------------------------------

const HISTORY_MODAL_MAX_ROWS: usize = 10;

/// Filas del modal: por entrada, «Nombre - N canciones» y debajo los nombres
/// de algunas canciones; o el aviso de que todavía no hay nada.
pub(super) fn history_modal_lines(
    history: &crate::history::History,
    modal: &HistoryModal,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let colors = theme();
    let dim = Style::default().fg(colors.dim);
    let text = Style::default().fg(colors.text);
    if history.is_empty() {
        return vec![Line::from(Span::styled(
            " Todavía no pusiste ninguna playlist",
            dim,
        ))];
    }
    let per_page = (max_rows / 2).max(1);
    let offset = scroll_offset(modal.selected, history.len(), per_page);
    let mut lines = Vec::new();
    for (index, entry) in history.entries().iter().enumerate().skip(offset).take(per_page) {
        let is_selected = index == modal.selected;
        let title_style = if is_selected {
            Style::default()
                .bg(colors.select_bg)
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            text
        };
        let cursor = if is_selected { CURSOR } else { " " };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{cursor} "),
                if is_selected { title_style } else { dim },
            ),
            Span::styled(
                truncate_chars(&entry.title_line(), width.saturating_sub(2)),
                title_style,
            ),
        ]));
        lines.push(Line::from(Span::styled(
            truncate_chars(&format!("  {}", entry.sample_tracks.join(" · ")), width),
            dim,
        )));
    }
    if let Some(notice) = &modal.notice {
        lines.push(Line::from(Span::styled(
            format!(" {notice}"),
            Style::default().fg(colors.error),
        )));
    }
    lines
}

/// Modal centrado del historial, al estilo de los otros modales.
pub(super) fn render_history_modal(
    frame: &mut Frame<'_>,
    history: &crate::history::History,
    modal: &HistoryModal,
) {
    let area = frame.area();
    let width = 72.min(area.width.saturating_sub(4)).max(20);
    let inner_width = usize::from(width.saturating_sub(2)).max(1);
    let max_rows = HISTORY_MODAL_MAX_ROWS
        .min(usize::from(area.height).saturating_sub(6))
        .max(2);
    let lines = history_modal_lines(history, modal, inner_width, max_rows);
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
            " Playlists reproducidas ",
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

/// Pie de atajos del modal.
pub(super) fn history_modal_footer(app: &App) -> Vec<Line<'static>> {
    let keys = &config::config().keys;
    if app.history.is_empty() {
        return vec![Line::from(format!(" {}: cerrar ", key_labels(&keys.back)))];
    }
    vec![Line::from(format!(
        " {}/{}: mover   {}: reproducir de nuevo (reemplaza la cola)   {}: cerrar ",
        key_labels(&keys.move_up),
        key_labels(&keys.move_down),
        key_labels(&keys.select),
        key_labels(&keys.back),
    ))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::ytdlp::PlaylistEntry;
    use crate::ui::tests_common::*;
    use std::cell::RefCell;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn entries(n: usize) -> Vec<PlaylistEntry> {
        (1..=n)
            .map(|i| PlaylistEntry {
                title: format!("Artista - Tema {i}"),
                page_url: format!("spotify:track:t{i}"),
            })
            .collect()
    }

    fn saved(kind: CollectionKind, id: &str, name: &str, n: usize) -> HistoryEntry {
        let titles: Vec<String> = entries(n).into_iter().map(|e| e.title).collect();
        HistoryEntry::new(kind, id.to_string(), name.to_string(), n, &titles)
    }

    fn no_resolve(url: String, _: u64) -> Receiver<YtDlpMessage> {
        panic!("no debía resolver {url}");
    }

    fn no_list(url: String, _: u64) -> Receiver<YtDlpListMessage> {
        panic!("no debía listar {url}");
    }

    fn app_with_history() -> App {
        let mut app = test_app();
        app.history.add(saved(CollectionKind::Playlist, "p1", "Rock", 7));
        app.history.add(saved(CollectionKind::Album, "a1", "Disco", 1));
        open_history_modal(&mut app);
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        handle_history_key_with(key(code), app, || Ok(()), no_resolve, no_list);
    }

    // --- registro ---

    #[test]
    fn listing_a_spotify_playlist_records_name_count_and_first_five_tracks() {
        let mut app = test_app();
        app.current_url = Some(("spotify:playlist:p9".to_string(), "Mi mezcla".to_string()));
        record_collection(&mut app, &Collection::unnamed(entries(8)));

        let saved = app.history.get(0).expect("registró la playlist");
        assert_eq!(saved.kind, CollectionKind::Playlist);
        assert_eq!(saved.id, "p9");
        assert_eq!(saved.name, "Mi mezcla");
        assert_eq!(saved.track_count, 8);
        assert_eq!(saved.sample_tracks.len(), 5);
        assert_eq!(saved.sample_tracks[0], "Artista - Tema 1");
    }

    #[test]
    fn an_album_url_is_recorded_as_an_album() {
        let mut app = test_app();
        let url = "https://open.spotify.com/album/alb7?si=x".to_string();
        app.current_url = Some((url.clone(), "Disco".to_string()));
        record_collection(&mut app, &Collection::unnamed(entries(3)));
        let saved = app.history.get(0).unwrap();
        assert_eq!(saved.kind, CollectionKind::Album);
        assert_eq!(saved.id, "alb7");
        assert_eq!(saved.marker(), "spotify:album:alb7");
    }

    #[test]
    fn a_pasted_url_without_a_known_name_gets_a_readable_fallback_name() {
        let mut app = test_app();
        let url = "https://open.spotify.com/playlist/p3".to_string();
        app.current_url = Some((url.clone(), url_display_label(&url)));
        record_collection(&mut app, &Collection::unnamed(entries(2)));
        assert_eq!(app.history.get(0).unwrap().name, "Playlist de Spotify");
    }

    fn named(name: &str, n: usize) -> Collection {
        Collection {
            name: Some(name.to_string()),
            entries: entries(n),
        }
    }

    #[test]
    fn the_real_name_from_spotify_is_what_gets_saved() {
        // URL pegada: la etiqueta es solo la de la URL, pero Spotify dio el nombre.
        let mut app = test_app();
        let url = "https://open.spotify.com/playlist/p3?si=x".to_string();
        app.current_url = Some((url.clone(), url_display_label(&url)));
        record_collection(&mut app, &named("Éxitos de los 90", 4));
        assert_eq!(app.history.get(0).unwrap().name, "Éxitos de los 90");
        assert_eq!(app.history.get(0).unwrap().track_count, 4);

        // Un álbum, igual.
        let mut app = test_app();
        app.current_url = Some(("spotify:album:a1".to_string(), "spotify:album:a1".to_string()));
        record_collection(&mut app, &named("Discovery", 2));
        assert_eq!(app.history.get(0).unwrap().name, "Discovery");
        assert_eq!(app.history.get(0).unwrap().kind, CollectionKind::Album);
    }

    #[test]
    fn the_real_name_wins_over_the_session_label_and_the_fallback_only_applies_without_one() {
        let mut app = test_app();
        app.current_url = Some(("spotify:playlist:p1".to_string(), "Etiqueta vieja".to_string()));
        record_collection(&mut app, &named("Nombre actual", 2));
        assert_eq!(app.history.get(0).unwrap().name, "Nombre actual");

        // Sin nombre de Spotify queda el de la sesión.
        record_collection(&mut app, &Collection::unnamed(entries(2)));
        assert_eq!(app.history.get(0).unwrap().name, "Etiqueta vieja");

        // Un nombre en blanco cuenta como ausente: respaldo si la etiqueta es de la URL.
        let mut app = test_app();
        let url = "https://open.spotify.com/playlist/p3".to_string();
        app.current_url = Some((url.clone(), url_display_label(&url)));
        record_collection(&mut app, &named("   ", 2));
        assert_eq!(app.history.get(0).unwrap().name, "Playlist de Spotify");
    }

    #[test]
    fn handle_playlist_result_carries_the_real_name_into_the_history() {
        let mut app = test_app();
        let url = "spotify:playlist:p9".to_string();
        app.current_url = Some((url.clone(), url_display_label(&url)));
        let calls = RefCell::new(Vec::new());
        let mut resolve = empty_resolve_rx(&calls);
        let req = app.ytdlp_request_id;
        handle_playlist_result(&mut app, req, Ok(named("Mi playlist real", 3)), &mut resolve);
        assert_eq!(app.history.get(0).map(|e| e.name.as_str()), Some("Mi playlist real"));
        // Y el nombre persiste en disco.
        let (reloaded, warning) = crate::history::load_from(app.history_path.as_ref().unwrap());
        assert!(warning.is_none());
        assert_eq!(reloaded.get(0).map(|e| e.name.as_str()), Some("Mi playlist real"));
        let _ = std::fs::remove_file(app.history_path.as_ref().unwrap());
    }

    #[test]
    fn youtube_playlists_and_single_tracks_are_not_recorded() {
        for url in [
            "https://www.youtube.com/playlist?list=PL1",
            "spotify:track:t1",
        ] {
            let mut app = test_app();
            app.current_url = Some((url.to_string(), "x".to_string()));
            record_collection(&mut app, &Collection::unnamed(entries(2)));
            assert!(app.history.is_empty(), "{url}");
        }
        let mut app = test_app();
        record_collection(&mut app, &Collection::unnamed(entries(2)));
        assert!(app.history.is_empty(), "sin current_url no hay nada que registrar");
    }

    #[test]
    fn playing_the_same_playlist_again_does_not_duplicate_it_and_persists() {
        let mut app = test_app();
        let path = app.history_path.clone().unwrap();
        app.current_url = Some(("spotify:playlist:p1".to_string(), "Rock".to_string()));
        record_collection(&mut app, &Collection::unnamed(entries(2)));
        record_collection(&mut app, &Collection::unnamed(entries(4)));
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history.get(0).unwrap().track_count, 4);
        let (reloaded, warning) = crate::history::load_from(&path);
        assert!(warning.is_none());
        assert_eq!(reloaded, app.history);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn handle_playlist_result_records_the_collection_it_just_listed() {
        let mut app = test_app();
        app.current_url = Some(("spotify:album:a1".to_string(), "Disco".to_string()));
        let calls = RefCell::new(Vec::new());
        let mut resolve = empty_resolve_rx(&calls);
        let req = app.ytdlp_request_id;
        handle_playlist_result(&mut app, req, Ok(Collection::unnamed(entries(3))), &mut resolve);
        assert_eq!(app.history.get(0).map(|e| e.name.as_str()), Some("Disco"));
        assert_eq!(app.history.get(0).map(|e| e.track_count), Some(3));
        // Un listado que falla no registra nada.
        let mut app = test_app();
        app.current_url = Some(("spotify:album:a1".to_string(), "Disco".to_string()));
        let req = app.ytdlp_request_id;
        handle_playlist_result(
            &mut app,
            req,
            Err(crate::source::ytdlp::YtDlpError::Failed("x".into())),
            &mut resolve,
        );
        assert!(app.history.is_empty());
    }

    // --- modal ---

    #[test]
    fn the_modal_lists_entries_with_name_count_and_some_tracks() {
        let app = app_with_history();
        let lines = history_modal_lines(&app.history, app.history_modal.as_ref().unwrap(), 70, 10);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts[0].contains("Disco - 1 canción"), "{texts:?}");
        assert!(texts[1].contains("Artista - Tema 1"), "{texts:?}");
        assert!(texts[2].contains("Rock - 7 canciones"), "{texts:?}");
        // Se ven los primeros nombres (la fila se recorta al ancho) y nunca más
        // de los 5 guardados.
        assert!(texts[3].contains("Tema 1") && texts[3].contains("Tema 2"), "{texts:?}");
        assert!(!texts[3].contains("Tema 6"), "solo las primeras 5: {texts:?}");
        for line in &lines {
            assert!(line_width(line) <= 70, "{}", line_text(line));
        }
    }

    #[test]
    fn an_empty_history_says_so() {
        let mut app = test_app();
        open_history_modal(&mut app);
        let lines = history_modal_lines(&app.history, app.history_modal.as_ref().unwrap(), 70, 10);
        assert!(line_text(&lines[0]).contains("Todavía no pusiste ninguna playlist"));
        // Enter sin entradas no hace nada ni rompe; Esc cierra.
        press(&mut app, KeyCode::Enter);
        assert!(app.history_modal.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.history_modal.is_none());
    }

    #[test]
    fn arrows_and_pages_move_the_selection_within_bounds() {
        let mut app = test_app();
        for i in 0..30 {
            app.history.add(saved(CollectionKind::Playlist, &format!("p{i}"), &format!("L{i}"), 2));
        }
        open_history_modal(&mut app);
        let selected = |app: &App| app.history_modal.as_ref().unwrap().selected;
        press(&mut app, KeyCode::Down);
        assert_eq!(selected(&app), 1);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Up);
        assert_eq!(selected(&app), 0, "no baja de la primera");
        press(&mut app, KeyCode::PageDown);
        assert_eq!(selected(&app), PAGE_STEP as usize);
        press(&mut app, KeyCode::End);
        assert_eq!(selected(&app), app.history.len() - 1);
        press(&mut app, KeyCode::Home);
        assert_eq!(selected(&app), 0);
    }

    #[test]
    fn enter_replays_the_entry_through_the_list_flow_and_flags_a_queue_replacement() {
        let mut app = app_with_history();
        app.queue.push(QueueItem::Local(track_named("vieja")));
        press(&mut app, KeyCode::Down); // "Rock" (la segunda, más antigua)
        let resolve_calls = RefCell::new(Vec::<String>::new());
        let list_calls = RefCell::new(Vec::<String>::new());
        handle_history_key_with(
            key(KeyCode::Enter),
            &mut app,
            || Ok(()),
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        assert!(app.history_modal.is_none(), "el modal se cierra");
        assert_eq!(list_calls.borrow().as_slice(), &["spotify:playlist:p1"]);
        assert!(resolve_calls.borrow().is_empty());
        assert_eq!(app.track_label, "Rock");
        assert_eq!(app.replace_queue_req, Some(app.ytdlp_request_id));
        // Todavía no se tocó la cola: espera al listado.
        assert_eq!(app.queue.len(), 1);
    }

    #[test]
    fn an_album_entry_replays_as_an_album() {
        let mut app = app_with_history();
        let resolve_calls = RefCell::new(Vec::<String>::new());
        let list_calls = RefCell::new(Vec::<String>::new());
        handle_history_key_with(
            key(KeyCode::Enter),
            &mut app,
            || Ok(()),
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        assert_eq!(list_calls.borrow().as_slice(), &["spotify:album:a1"]);
    }

    #[test]
    fn the_replayed_listing_replaces_the_queue_instead_of_appending() {
        let mut app = app_with_history();
        app.queue.push(QueueItem::Local(track_named("vieja")));
        app.queue.push(QueueItem::Local(track_named("otra")));
        app.queue_cursor = Some(1);
        app.queue_selected = 1;
        let resolve_calls = RefCell::new(Vec::<String>::new());
        let list_calls = RefCell::new(Vec::<String>::new());
        handle_history_key_with(
            key(KeyCode::Enter),
            &mut app,
            || Ok(()),
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );

        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req = app.ytdlp_request_id;
        handle_playlist_result(&mut app, req, Ok(Collection::unnamed(entries(3))), &mut resolve_seam);

        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(labels, vec!["Artista - Tema 2", "Artista - Tema 3"]);
        assert_eq!(app.queue_cursor, None);
        assert_eq!(app.queue_selected, 0);
        assert_eq!(app.replace_queue_req, None);
        assert_eq!(resolve_calls.borrow().as_slice(), &["spotify:track:t1"]);
    }

    #[test]
    fn a_failed_replay_listing_keeps_the_current_queue() {
        let mut app = app_with_history();
        app.queue.push(QueueItem::Local(track_named("vieja")));
        let resolve_calls = RefCell::new(Vec::<String>::new());
        let list_calls = RefCell::new(Vec::<String>::new());
        handle_history_key_with(
            key(KeyCode::Enter),
            &mut app,
            || Ok(()),
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req = app.ytdlp_request_id;
        handle_playlist_result(
            &mut app,
            req,
            Err(crate::source::ytdlp::YtDlpError::Failed("429".into())),
            &mut resolve_seam,
        );
        assert_eq!(app.queue.len(), 1, "la cola no se pierde si el listado falla");
        assert_eq!(app.replace_queue_req, None);
    }

    #[test]
    fn a_normal_playlist_listing_still_appends_to_the_queue() {
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("previa")));
        app.current_url = Some(("spotify:playlist:p1".to_string(), "Rock".to_string()));
        let calls = RefCell::new(Vec::<String>::new());
        let mut resolve_seam = empty_resolve_rx(&calls);
        let req = app.ytdlp_request_id;
        handle_playlist_result(&mut app, req, Ok(Collection::unnamed(entries(3))), &mut resolve_seam);
        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(labels, vec!["previa", "Artista - Tema 2", "Artista - Tema 3"]);
    }

    #[test]
    fn without_a_spotify_session_the_modal_stays_open_and_says_to_log_in() {
        let mut app = app_with_history();
        app.queue.push(QueueItem::Local(track_named("vieja")));
        handle_history_key_with(
            key(KeyCode::Enter),
            &mut app,
            || Err(crate::source::spotify::play::ConnectError::NotLoggedIn.to_string()),
            no_resolve,
            no_list,
        );
        let modal = app.history_modal.as_ref().expect("sigue abierto");
        let notice = modal.notice.clone().expect("con aviso");
        assert!(notice.contains("mp3tui --spotify-login"), "{notice}");
        assert_eq!(app.replace_queue_req, None);
        assert_eq!(app.queue.len(), 1, "la cola queda intacta");
        let lines = history_modal_lines(&app.history, modal, 70, 10);
        assert!(lines.iter().map(line_text).any(|t| t.contains("--spotify-login")));
    }

    #[test]
    fn the_configured_key_is_free_and_modal_swallows_other_keys() {
        let keys = &config::config().keys;
        assert!(config::pressed(&keys.playlist_history, KeyCode::Char('h')));
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = app_with_history();
        dispatch_key(key(KeyCode::Char('q')), &mut app, &mut engine);
        assert!(!app.should_quit);
        assert!(app.history_modal.is_some());
        // Y la tecla abre el modal desde el player y desde la lista.
        for mode in [Mode::Player, Mode::Browser] {
            let mut app = test_app();
            app.mode = mode;
            app.has_player = mode == Mode::Player;
            dispatch_key(key(KeyCode::Char('h')), &mut app, &mut engine);
            assert!(app.history_modal.is_some(), "{mode:?}");
        }
    }

    #[test]
    fn the_library_view_no_longer_lists_url_history() {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![track_named("alpha")]));
        app.history.add(saved(CollectionKind::Playlist, "p1", "Rock", 2));
        refresh_view(&mut app);
        assert_eq!(app.view.len(), 1, "solo las pistas locales");
    }

    #[test]
    fn modal_renders_in_every_state_even_on_tiny_terminals() {
        let full = app_with_history();
        let empty = {
            let mut app = test_app();
            open_history_modal(&mut app);
            app
        };
        let mut with_notice = app_with_history();
        with_notice.history_modal.as_mut().unwrap().notice = Some("x ".repeat(80));
        for app in [&full, &empty, &with_notice] {
            for (w, h) in [(100, 30), (40, 8), (23, 10), (10, 2), (1, 1), (0, 0)] {
                let backend = ratatui::backend::TestBackend::new(w, h);
                let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
                terminal
                    .draw(|frame| {
                        render_history_modal(frame, &app.history, app.history_modal.as_ref().unwrap())
                    })
                    .unwrap_or_else(|e| panic!("{w}x{h}: {e}"));
            }
        }
        for line in history_modal_footer(&full) {
            assert!(line_width(&line) <= 78, "{}", line_text(&line));
        }
    }
}
