use super::*;

/// Ventana para confirmar la salida con un segundo Esc.
pub(super) const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuitDecision {
    Arm,
    Quit,
}

/// Doble pulsación para salir: el primer «quit» arma la guarda y muestra el
/// aviso; un segundo dentro de `QUIT_CONFIRM_WINDOW` sale. Cualquier otra tecla
/// (o el vencimiento de la ventana) la desarma. El reloj llega por parámetro
/// para poder testear la ventana sin esperar.
#[derive(Debug, Default)]
pub(super) struct QuitGuard {
    armed_at: Option<Instant>,
    /// Marca si la tecla que se está procesando pasó por la guarda; si no,
    /// `handle_key` la desarma al terminar.
    pub(super) touched: bool,
}

impl QuitGuard {
    pub(super) fn on_quit_key(&mut self, now: Instant) -> QuitDecision {
        self.touched = true;
        if self.is_armed(now) {
            self.armed_at = None;
            QuitDecision::Quit
        } else {
            self.armed_at = Some(now);
            QuitDecision::Arm
        }
    }

    pub(super) fn disarm(&mut self) {
        self.armed_at = None;
    }

    pub(super) fn is_armed(&self, now: Instant) -> bool {
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

pub(super) fn handle_player_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
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
pub(super) fn handle_list_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
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
pub(super) fn handle_search_key(key: KeyEvent, app: &mut App) {
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

pub(super) fn open_search(app: &mut App) {
    // El texto arranca del filtro actual para poder editarlo.
    app.search = Some(app.filter.clone().unwrap_or_default());
    refresh_view(app);
}

fn open_url_prompt(app: &mut App) {
    app.url_input = Some(String::new());
}

pub(super) fn handle_url_prompt_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
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
            let delta = if matches!(key.code, KeyCode::Up) {
                -1
            } else {
                1
            };
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

pub(super) fn open_theme_menu(app: &mut App) {
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

pub(super) fn handle_theme_menu_key(
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
            // aplicar el tema Miku también activa la bailarina (el default
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

pub(super) fn open_bands_menu(app: &mut App) {
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

pub(super) fn handle_bands_menu_key(
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

pub(super) fn open_mode_menu(app: &mut App) {
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

pub(super) fn handle_mode_menu_key(
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

pub(super) fn open_settings_panel(app: &mut App) {
    app.settings_panel = Some(SettingsPanelState { selected: 0 });
}

pub(super) fn handle_settings_panel_key(
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
            let delta = if matches!(key.code, KeyCode::Up) {
                -1
            } else {
                1
            };
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

/// Acción de «back» (Esc) desde la lista: con el panel lateral abierto
/// lo cierra y deja el reproductor de fondo; con el explorador a
/// pantalla completa vuelve al player si hay sesión y si no sale de la app.
pub(super) fn go_back(app: &mut App) {
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
pub(super) fn toggle_explorer(app: &mut App) {
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
