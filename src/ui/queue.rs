use super::*;

pub(super) fn handle_ytdlp_result(
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

/// Resultado del listado `--flat-playlist` de una playlist. La primera
/// entrada se resuelve enseguida (su URL de audio vence) y el resto entra a
/// la cola **al final** de la existente, sin tocar el cursor. Una lista vacía
/// o un listado fallido → aviso visible y la cola queda intacta.
pub(super) fn handle_playlist_result(
    app: &mut App,
    request_id: u64,
    result: Result<crate::source::ytdlp::Collection, crate::source::ytdlp::YtDlpError>,
    resolve_page: &mut impl FnMut(String, u64) -> Receiver<YtDlpMessage>,
) {
    if request_id != app.ytdlp_request_id {
        // Resultado tarde de un listado ya cancelado: se descarta.
        return;
    }
    match result {
        Ok(collection) if collection.entries.is_empty() => {
            app.replace_queue_req = None;
            app.status = Status::Stopped;
            app.error = Some(PLAYLIST_EMPTY_MSG.to_string());
            app.ytdlp_failed = true;
        }
        Ok(collection) => {
            let entries = &collection.entries;
            // Volver a poner una playlist del historial REEMPLAZA la cola; el
            // vaciado espera al listado exitoso para no perderla si falla.
            if app.replace_queue_req.take() == Some(request_id) {
                clear_queue(app);
            }
            record_collection(app, &collection);
            // Las siguientes van al final de la cola existente, sin
            // tocar el cursor de la cola. Cada una se resuelve recién
            // cuando le toque sonar.
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
            app.replace_queue_req = None;
            app.status = Status::Stopped;
            app.error = Some(err.to_string());
            app.ytdlp_failed = true;
        }
    }
}

pub(super) fn init_url_playback(app: &mut App, original_url: String, label: String) {
    cancel_pending_ytdlp(app);
    app.is_url = true;
    app.current_path = None;
    app.track_label = label.clone();
    app.has_player = true;
    app.status = Status::Connecting;
    app.error = None;
    app.current_url = Some((original_url, label));
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

pub(super) fn start_url_playback(
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
    }
}

/// Cliente de Spotify para los hilos de resolución y listado (lee config y
/// tokens; la red ocurre recién al usarlo).
fn connect_spotify() -> Result<
    Box<dyn crate::source::spotify::play::TrackSource>,
    crate::source::spotify::play::ConnectError,
> {
    crate::source::spotify::play::connect(crate::config::config())
        .map(|client| Box::new(client) as Box<dyn crate::source::spotify::play::TrackSource>)
}

/// Único punto del programa que lanza el proceso yt-dlp para resolver una URL
/// de página: devuelve el receiver del resultado. Centralizarlo acá permite
/// que los tests inyecten un doble y nunca toquen el proceso ni la red.
pub(super) fn spawn_ytdlp_resolution(url: String, req_id: u64) -> Receiver<YtDlpMessage> {
    let (tx, rx) = mpsc::channel();
    let url_clone = url.clone();
    thread::spawn(move || {
        let mut cache = crate::source::ytdlp::ResolveCache::new()
            .unwrap_or_else(|| crate::source::ytdlp::ResolveCache::at_path(PathBuf::new()));
        let result = crate::source::spotify::play::resolve_page(
            &url,
            &crate::source::ytdlp::ProcessCommandRunner,
            &mut cache,
            &connect_spotify,
        );
        let _ = tx.send((req_id, url_clone, result));
    });
    rx
}

/// Único punto del programa que lista una playlist de YouTube con yt-dlp
/// `--flat-playlist`: devuelve el receiver del resultado (mismo patrón de
/// seam que [`spawn_ytdlp_resolution`], sin tocar red en los tests).
pub(super) fn spawn_ytdlp_listing(url: String, req_id: u64) -> Receiver<YtDlpListMessage> {
    let (tx, rx) = mpsc::channel();
    let url_clone = url.clone();
    thread::spawn(move || {
        let result = if crate::source::spotify::play::is_collection(&url) {
            crate::source::spotify::play::list_playlist(&url, &connect_spotify)
        } else {
            crate::source::ytdlp::list_flat(&url, &crate::source::ytdlp::ProcessCommandRunner)
                .map(crate::source::ytdlp::Collection::unnamed)
        };
        let _ = tx.send((req_id, url_clone, result));
    });
    rx
}

pub(super) fn submit_url_prompt(input: String, app: &mut App, engine: &mut AudioEngine) {
    submit_url_prompt_with(input, app, engine, spawn_ytdlp_resolution)
}

/// Seam inyectable de [`submit_url_prompt`] (resolución): la rama de página
/// recibe el resolver (`FnOnce(url, req_id) -> receiver`), que en producción
/// es [`spawn_ytdlp_resolution`] y en tests un doble que no lanza yt-dlp.
/// El listado de playlist usa el seam de producción; los tests de playlist
/// usan [`submit_url_prompt_with_seams`] para inyectarlo aislado.
pub(super) fn submit_url_prompt_with(
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
/// hermético a los tests en los dos puntos de entrada.
pub(super) fn submit_url_prompt_with_seams(
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
                app.error = Some(format!(
                    "'{}' no existe o no es un archivo.",
                    path.display()
                ));
            }
        }
        SubmitAction::DirectAudio(url) | SubmitAction::Unknown(url) => {
            let label = url_display_label(&url);
            start_url_playback(app, engine, url.clone(), url, label);
            app.mode = Mode::Player;
        }
        SubmitAction::Page(url)
        | SubmitAction::Playlist(url)
        | SubmitAction::SpotifyTrack(url)
        | SubmitAction::SpotifyPlaylist(url)
        | SubmitAction::SpotifyAlbum(url) => {
            begin_url_flow(app, url, resolve_page, list_playlist);
        }
    }
}

/// Inicio del flujo de URL de página/playlist (unificado para el boot por
/// argumento y el prompt `u`): prepara la sesión de URL, valida el `request
/// id` pendiente y lanza la resolución (página) o el listado `--flat-playlist`
/// (playlist). En la playlist se lista y la primera canción se resuelve
/// enseguida; el resto entra a la cola al final cuando llega el listado.
pub(super) fn begin_url_flow(
    app: &mut App,
    url: String,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) {
    let is_playlist = matches!(
        crate::source::classify(&url),
        crate::source::Input::Url {
            kind: crate::source::UrlKind::Playlist
                | crate::source::UrlKind::SpotifyPlaylist
                | crate::source::UrlKind::SpotifyAlbum,
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
pub(super) fn start_boot_url(
    app: &mut App,
    engine: &mut AudioEngine,
    url: String,
    kind: crate::source::UrlKind,
) {
    start_boot_url_with(
        app,
        engine,
        url,
        kind,
        spawn_ytdlp_resolution,
        spawn_ytdlp_listing,
    )
}

/// Variante testeable de [`start_boot_url`] con los seams inyectables de
/// resolución y listado.
pub(super) fn start_boot_url_with(
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
        crate::source::UrlKind::Page
        | crate::source::UrlKind::Playlist
        | crate::source::UrlKind::SpotifyTrack
        | crate::source::UrlKind::SpotifyPlaylist
        | crate::source::UrlKind::SpotifyAlbum => {
            begin_url_flow(app, url, resolve_page, list_playlist);
        }
    }
}

/// Play / pausa / relanzar al terminar la pista.
pub(super) fn toggle_play(app: &mut App, engine: &mut AudioEngine) {
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
pub(super) fn play_selected(app: &mut App, engine: &mut AudioEngine) {
    let Some(item) = app.view.get(app.selected).copied() else {
        return;
    };
    match item {
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
/// Una pista local va como `QueueItem::Local`.
pub(super) fn enqueue_selected(app: &mut App) {
    let Some(item) = app.view.get(app.selected).copied() else {
        return;
    };
    match item {
        ViewItem::Track(library_index) => {
            if let Some(track) = library_get(app, library_index) {
                app.queue.push(QueueItem::Local(track.clone()));
            }
        }
    }
}

/// Enter en la Cola: mueve el cursor de reproducción al ítem seleccionado y
/// lo reproduce ya (resuelve la URL por el camino normal si es una URL
/// pendiente). El ítem no sale de la cola: queda marcado como actual, para
/// poder volver con `p`.
pub(super) fn play_selected_queue_item(app: &mut App, engine: &mut AudioEngine) {
    if app.queue.is_empty() {
        return;
    }
    let index = app.queue_selected.min(app.queue.len() - 1);
    if play_queue_from(
        app,
        engine,
        index,
        QueueAdvance::Step(1),
        &mut dispatch_url_replay,
    ) {
        app.mode = Mode::Player;
    }
}

/// Posición de la cola que sigue al cursor de reproducción: la de abajo
/// mientras nada de la cola haya sonado todavía. `None` cuando no queda nada
/// por delante (o la cola está vacía): ahí vale el fallback al pool de la
/// biblioteca/historial.
pub(super) fn queue_next_position(app: &App) -> Option<usize> {
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

/// El «próximo» de la franja del panel lateral: el ítem que sigue al
/// cursor de la cola; con la cola agotada (o vacía), el siguiente que sonaría
/// del pool de la biblioteca/historial con repeat/shuffle como están; sin
/// nada → `None` (la franja no aparece). Se refresca una vez por frame en el
/// loop, para que el draw solo lea.
pub(super) fn refresh_next_up(app: &mut App) {
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
    // El siguiente estricto (respeta repeat/shuffle: con repeat Off y en la
    // última pista no hay nada que suene después).
    let target = next_track(
        pool.len(),
        current,
        app.repeat,
        app.shuffle,
        &app.shuffle_order,
        1,
    )
    .map(|target_idx| pool[target_idx])
    .or_else(|| {
        // La franja NO debe desaparecer mientras suena algo y el pool tiene
        // ítems: en el borde (repeat Off, última pista) muestra el primero
        // del recorrido como próximo del panel (el avance real de `n` sigue
        // respetando repeat).
        pool.first().copied()
    });
    let label = match target.and_then(|view_pos| app.view.get(view_pos)) {
        Some(ViewItem::Track(library_index)) => {
            library_get(app, *library_index).map(|track| track.title.clone())
        }
        None => None,
    };
    app.next_up = Some(label.unwrap_or_default());
}

/// Reproduce desde `start` hacia adelante de la cola saltando los ítems
/// locales que no abren: aviso visible y se sigue con el próximo. Si el
/// ítem es una URL se arranca su resolución (asíncrona, esperando su turno,
/// ) y se registra `queue_resolving` para saltearla si falla. Mueve el
/// cursor a la posición que quedó en marcha. Devuelve `false` cuando la cola
/// quedó agotada (entonces vale el fallback de ).
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
                // flujos de reproducción limpian el error al comenzar.
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
                // No abrió: se saltea con aviso.
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

/// Ítem encolado que falló al resolver justo cuando le tocaba el turno:
/// se saltea con aviso visible — el cursor avanza al próximo ítem de la cola y
/// lo intenta (los locales que tampoco abran se saltean igual, con aviso); si
/// la cola se agota, vale el fallback de. El ítem fallido queda en la
/// cola y el cursor lo pasa (queda atenuado al sonar lo que sigue).
pub(super) fn skip_failed_queue_item(
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
            queue_exhausted_pool_fallback(app, engine, step);
        } else {
            queue_exhausted_pool_fallback(app, engine, -1);
        }
    }
    // El aviso queda visible aunque haya arrancado el siguiente.
    app.error = Some(QUEUE_ITEM_FAILED_MSG.to_string());
}

/// Fallback de cuando la cola se agotó tras saltar ítems fallidos:
/// avanza el pool de la biblioteca en `step` (`1` hacia adelante,
/// `-1` hacia atrás). Sin hacer nada si el pool está vacío o no hay destino.
fn queue_exhausted_pool_fallback(
    app: &mut App,
    engine: &mut AudioEngine,
    step: isize,
) {
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
    play_pool_target(app, engine, pool[target_idx]);
}

/// Borra el ítem seleccionado de la Cola y acota la selección al nuevo alto.
/// El cursor de reproducción se acomoda para seguir apuntando al mismo ítem:
/// si se borra el que estaba sonando desde la cola, el cursor vuelve a `None`
/// (nada de la cola está sonando).
pub(super) fn delete_selected_queue_item(app: &mut App) {
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

/// Vacía toda la cola y resetea su cursor y selección. Solo toca la cola: la
/// pista que suena sigue sonando y no se cancela ninguna resolución.
pub(super) fn clear_queue(app: &mut App) {
    app.queue.clear();
    app.queue_cursor = None;
    app.queue_selected = 0;
}

/// Posiciones de la vista del pool de navegación (lo que recorren `n`/`p` y
/// el fin natural): siempre las pistas de la biblioteca, y además las
/// entradas del historial cuando el shuffle está activo, para que una
/// pista local pueda encadenar con un tema de YouTube y viceversa. Con el
/// shuffle apagado el pool queda como antes (solo pistas locales). Regenera
/// el orden de shuffle si la lista cambió desde que se armó (filtro/rescann).
/// Posiciones de la vista que recorren `n`/`p` y el fin natural: SIEMPRE las
/// pistas locales Y las filas de URL del historial en orden de lista (antes
/// las URLs solo entraban al pool con shuffle activo, y desde una URL el
/// siguiente caía en la primera pista local: bug reportado). Con shuffle, el
/// orden de recorrido es `shuffle_order` sobre este pool mixto.
pub(super) fn pool_positions_in_view(app: &mut App) -> Vec<usize> {
    let pool: Vec<usize> = app
        .view
        .iter()
        .enumerate()
        .map(|(pos, _item)| pos)
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
    // Una URL ya no está en la vista (el historial de URLs salió de la
    // biblioteca): cae como antes cuando la URL no estaba en el historial.
    let view_pos = if app.is_url {
        return None;
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
/// del fin natural) por [`play_view_position`]. Devuelve false si la posición
/// no es jugable.
fn play_pool_target(app: &mut App, engine: &mut AudioEngine, target: usize) -> bool {
    match app.view.get(target).copied() {
        Some(ViewItem::Track(_)) => play_view_position(app, engine, target),
        None => false,
    }
}

/// 'n' / 'p': salta a la siguiente/anterior pista de la playlist visible,
/// respetando shuffle y repeat, sin salir del modo actual (en el explorador
/// además mueve la selección). No envuelve con repeat Off/One.
pub(super) fn play_step(app: &mut App, engine: &mut AudioEngine, step: isize) {
    play_step_with(app, engine, step, dispatch_url_replay)
}

/// Idéntico a [`play_step`] pero con el replay de URL inyectable (mismo
/// patrón que [`handle_natural_end`]/[`handle_natural_end_with`]): en
/// producción la URL se resuelve con yt-dlp y en los tests se captura sin
/// lanzar procesos ni tocar la red.
pub(super) fn play_step_with(
    app: &mut App,
    engine: &mut AudioEngine,
    step: isize,
    mut replay: impl FnMut(&mut App, &mut AudioEngine, UrlReplayRequest),
) {
    // La cola manda: `n` y `p` recorren el cursor de la cola en
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
    play_pool_target(app, engine, pool[target_idx]);
}

pub(super) fn toggle_shuffle(app: &mut App) {
    app.shuffle = !app.shuffle;
    if app.shuffle {
        // El orden vive sobre el pool (pistas + historial), igual que el
        // pool que se usa al navegar / reproducir con el shuffle activo.
        app.shuffle_order = shuffle_order(app.view.len());
    }
}

/// Orden pura de relanzamiento de URL que pide el fin natural con repetir
/// «una»: la URL original de la sesión. La ejecuta el dispatcher inyectable
/// de [`handle_natural_end_with`] (en producción, [`dispatch_url_replay`]);
/// los tests la capturan sin lanzar yt-dlp ni tocar la red.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UrlReplayRequest {
    pub(super) original_url: String,
}

/// Fin natural de la pista (transición Playing → Ended): decide el repeat
/// sin esperar que el usuario apriete espacio. La regla única de avance vive
/// en [`natural_end_action`] y vale igual para pistas locales y para URL.
pub(super) fn handle_natural_end(app: &mut App, engine: &mut AudioEngine) {
    handle_natural_end_with(app, engine, dispatch_url_replay)
}

/// Efecto real del replay de URL: re-lanza la URL ORIGINAL por el camino
/// normal de URL (las páginas se resuelven de nuevo con yt-dlp porque las
/// URLs resueltas vencen).
pub(super) fn dispatch_url_replay(
    app: &mut App,
    engine: &mut AudioEngine,
    request: UrlReplayRequest,
) {
    submit_url_prompt(request.original_url, app, engine);
}

/// Seam inyectable del fin natural: el replay de URL pasa por `replay` en
/// lugar de disparar yt-dlp directamente, de modo que un test puede capturar
/// la orden y verificarla sin procesos ni red.
pub(super) fn handle_natural_end_with(
    app: &mut App,
    engine: &mut AudioEngine,
    mut replay: impl FnMut(&mut App, &mut AudioEngine, UrlReplayRequest),
) {
    // La cola manda: mientras quede algo después del cursor, el fin de pista
    // avanza el cursor a esa posición y la reproduce, sin aplicar `repeat` ni
    // `shuffle`. Agotada la cola, valen las reglas de siempre.
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
                    replay(app, engine, UrlReplayRequest { original_url });
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
                play_pool_target(app, engine, target);
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
