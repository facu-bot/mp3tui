use super::*;

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
pub(super) fn transport_row(
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
        (
            Button::Shuffle,
            "aleatorio".to_string(),
            toggle(shuffle),
            "     ",
        ),
        (
            Button::Repeat,
            repeat_label,
            toggle(repeat.label().is_some()),
            "  ",
        ),
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
pub(super) fn bar_geometry(label: &str, suffix: &str, total_width: usize) -> Option<(u16, u16)> {
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

/// Filas del sub-modal de selección: una por opción; ● marca la opción
/// activa al abrir y la selección va resaltada. Compartido por los tres
/// selectores (Temas, Bandas y Modo): cada wrapper resuelve sus filas como
/// (etiqueta, es-actual).
fn menu_lines(
    rows: impl IntoIterator<Item = (String, bool)>,
    selected: usize,
) -> Vec<Line<'static>> {
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
            Style::default()
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
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
pub(super) fn theme_menu_lines(menu: &ThemeMenuState) -> Vec<Line<'static>> {
    let selected = menu.selected;
    menu_lines(
        config::builtin_themes()
            .iter()
            .map(|(id, _)| (id.to_string(), *id == menu.original_name)),
        selected,
    )
}

pub(super) fn render(frame: &mut Frame<'_>, app: &App, engine: &AudioEngine) {
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
            Style::default()
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(prompt_rect);
    frame.render_widget(block, prompt_rect);

    let display_text = format!(" {input}▌");
    let line = Line::from(Span::styled(
        display_text,
        Style::default().fg(theme().text),
    ));
    frame.render_widget(Paragraph::new(vec![line]), inner);
}

/// Modal centrado de temas, al estilo del prompt de URL.
pub(super) fn render_theme_menu(frame: &mut Frame<'_>, menu: &ThemeMenuState) {
    render_menu(frame, " Temas ", theme_menu_lines(menu));
}

/// Modal centrado del panel, con el mismo estilo del modal de temas:
/// fondo limpio + borde acentuado con título.
pub(super) fn render_settings_panel(
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
            Style::default()
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(
        Paragraph::new(settings_panel_lines(panel, mouse_enabled)),
        inner,
    );
}

/// Filas del panel («Mouse» muestra el estado vivo; el resto fijas): la
/// selección va resaltada como en el menú de temas.
pub(super) fn settings_panel_lines(
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
        Style::default()
            .fg(theme().accent)
            .add_modifier(Modifier::BOLD),
    )
}

/// Iconos Unicode en los estados de reproducción.
pub(super) fn status_parts(status: Status) -> (&'static str, Color) {
    match status {
        Status::Playing => ("▶  Reproduciendo", theme().ok),
        Status::Paused => ("⏸  Pausado", theme().warn),
        Status::Ended => ("⏹  Fin de pista", theme().dim),
        Status::Stopped => ("⏹  Detenido", theme().error),
        Status::Resolving => ("⏳  Resolviendo con yt-dlp…", theme().accent),
        Status::Connecting => ("⏳  Conectando…", theme().accent),
    }
}

pub(super) fn status_line(status: Status, repeat: RepeatMode, shuffle: bool) -> Line<'static> {
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

pub(super) fn error_line(app: &App) -> Line<'static> {
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
pub(super) fn player_areas(area: Rect, explorer_open: bool) -> (Rect, Rect, Rect, Option<Rect>) {
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
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(upper);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(4)])
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

fn render_player_info(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    engine: &AudioEngine,
    focused: bool,
) {
    // Borde acentuado cuando el player tiene el foco.
    let border_color = if focused { theme().accent } else { theme().dim };
    let block = Block::bordered()
        .border_style(Style::default().fg(border_color))
        .title(accent_title("mp3tui"));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Buscar metadatos del track actual en la biblioteca.
    let track_meta: Option<(String, Option<String>)> = app
        .current_path
        .as_ref()
        .and_then(|path| app.library.as_ref()?.index_of(path))
        .and_then(|idx| app.library.as_ref()?.get(idx))
        .and_then(|track| {
            track
                .artist
                .as_ref()
                .map(|a| (a.clone(), track.album.clone()))
        });

    // La fila de velocidad aparece solo cuando no es 1.0x: en velocidad
    // normal el panel conserva su forma y el espacio no muestra ruido.
    let show_speed = (engine.speed() - 1.0).abs() > f32::EPSILON;
    let show_meta = track_meta.is_some();
    // La cola solo se anuncia cuando queda algo por delante del cursor; si no,
    // no agrega ruido.
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
            spans.push(Span::styled(
                album.clone(),
                Style::default().fg(theme().dim),
            ));
        }
        info_lines.push(Line::from(spans));
    }

    info_lines.push(status_line(app.status, app.repeat, app.shuffle));

    // Lo próximo que suena: el ítem que sigue al cursor de la cola, solo
    // cuando queda algo por delante. El título usa `QueueItem::label`
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
    let (controls, buttons) = transport_row(
        engine.is_playing(),
        app.shuffle,
        app.repeat,
        controls_area.width,
    );
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
            hit.buttons
                .push((Rect::new(x, controls_area.y, width, 1), button));
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
pub(super) enum BarCellKind {
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
pub(super) fn progress_bar_cells(
    width: usize,
    played: f64,
    buffered: &[(f32, f32)],
) -> Vec<BarCellKind> {
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
    let bar_chars =
        bar_geometry(label, suffix, total_width).map_or(0, |(_, width)| usize::from(width));

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
    spans.push(Span::styled(
        suffix.to_string(),
        Style::default().fg(theme().text),
    ));

    Line::from(spans)
}

/// Fila "Velocidad: 1.25x" del panel de info (solo se renderiza si != 1.0x).
fn speed_line(engine: &AudioEngine) -> Line<'static> {
    Line::from(vec![
        Span::styled("Velocidad: ", Style::default().fg(theme().dim)),
        Span::styled(
            speed_label(engine.speed()),
            Style::default().fg(theme().accent),
        ),
    ])
}

pub(super) fn speed_label(speed: f32) -> String {
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
    // La sección «Próxima canción» va DENTRO del rectángulo del panel,
    // en su último tramo: lista → separador → etiqueta → título (marquee), y
    // el borde de abajo del panel cierra todo el bloque. Solo existe en el
    // PANEL lateral (sesión de audio con Tab abierto): en el explorador a
    // pantalla completa sin player no hay panel y no se dibuja. Sin próximo
    // no agrega ruido, igual que la línea del reproductor.
    let has_next = app.has_player
        && matches!(
            &app.next_up,
            Some(title) if !title.is_empty()
        );
    let list_area = if has_next {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(inner);
        let (list_area, section_area) = (split[0], split[1]);
        render_next_up_section(
            frame,
            section_area,
            app.next_up.as_deref().unwrap_or_default(),
            app.ui_frame,
        );
        list_area
    } else {
        inner
    };
    frame.render_widget(block, area);

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

/// Tramo final del panel lateral: separador fino, etiqueta «Próxima
/// canción» (tenue, como `Siguiente:` del reproductor) y el título en marquee,
/// ocupando TODO el ancho del panel; el borde de abajo del bloque lo cierra.
fn render_next_up_section(frame: &mut Frame<'_>, area: Rect, title: &str, frame_clock: u64) {
    let width = area.width as usize;
    let divider = Line::from(Span::styled(
        "─".repeat(width),
        Style::default().fg(theme().dim),
    ));
    let label = Line::from(Span::styled(
        "Próxima canción",
        Style::default().fg(theme().dim),
    ));
    let song = Line::from(Span::styled(
        marquee_window(title, width, frame_clock as usize),
        Style::default().fg(theme().text),
    ));
    frame.render_widget(Paragraph::new(vec![divider, label, song]), area);
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
    let has_history = app
        .view
        .iter()
        .any(|item| matches!(item, ViewItem::History(_)));
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
        previous = app
            .view
            .get(from - 1)
            .and_then(|item| item_group(app, item));
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
    while start < app.selected && rows_between(app, start, app.selected, grouped) > height {
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
            Style::default()
                .bg(bg)
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().ok),
            Style::default()
                .bg(bg)
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .bg(bg)
                .fg(theme().text)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            Style::default()
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
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
pub(super) fn library_rows(app: &App, width: u16, height: u16) -> Vec<Line<'static>> {
    let height = height as usize;
    if height == 0 {
        return Vec::new();
    }
    let grouped = view_is_grouped(app);
    let start = visible_start(app, height, grouped);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
    let mut previous: Option<&str> = None;
    if grouped && start > 0 {
        previous = app
            .view
            .get(start - 1)
            .and_then(|item| item_group(app, item));
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
                lines.push(history_row_line(entry, is_selected, is_playing, width));
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

/// Ventana de marquee para un título de `width` celdas: si entra,
/// pegado a la izquierda (con padding a `width`); si no, recorre una ventana
/// de `width` celdas sobre `título + separador + título` en bucle continuo,
/// avanzando un carácter por frame.
/// Paso del marquee: un carácter cada estos frames (~150 ms a ~60 draws/s),
/// para que el título se alcance a leer y el desplazamiento sea constante sin
/// importar el tamaño del terminal.
pub(super) const MARQUEE_FRAMES_PER_CHAR: usize = 8;

pub(super) fn marquee_window(text: &str, width: usize, frame: usize) -> String {
    let width = width.max(1);
    let chars = text.chars().count();
    if chars <= width {
        return format!("{text:<width$}");
    }
    let sep = "   ";
    let cycle: Vec<char> = format!("{text}{sep}{text}{sep}").chars().collect();
    let step = (frame / MARQUEE_FRAMES_PER_CHAR) % (chars + sep.chars().count());
    cycle.iter().skip(step).take(width).collect()
}

/// Estado de una fila de la cola respecto del cursor de reproducción: lo
/// ya sonado se atenúa, la posición donde está parado se marca y lo que falta
/// queda normal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum QueueRowState {
    Played,
    Current,
    Pending,
}

/// Lista de la Cola del panel: el mismo resaltado de selección que la
/// Biblioteca (`queue_selected`), más el marcador `▶` del ítem parado en el
/// cursor de reproducción y los ya sonados atenuados. La cola vacía muestra un
/// estado explícito.
pub(super) fn queue_rows(app: &App, width: u16, height: u16) -> Vec<Line<'static>> {
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
            Style::default()
                .bg(bg)
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().ok),
            Style::default().bg(bg).fg(theme().text),
        )
    } else {
        let text = match state {
            QueueRowState::Played => theme().dim,
            QueueRowState::Current | QueueRowState::Pending => theme().text,
        };
        (
            Style::default()
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
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
        Style::default()
            .fg(theme().accent)
            .add_modifier(Modifier::BOLD),
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
pub(super) fn library_title_line(app: &App, folder: &str, highlight: Color) -> Line<'static> {
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
pub(super) fn library_title(app: &App, folder: &str) -> String {
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
pub(super) fn library_folder_label(app: &App) -> String {
    match app.roots.as_slice() {
        [root] => folder_label(root),
        roots => format!("{} carpetas", roots.len()),
    }
}

/// Una fila de la biblioteca con título en `theme().text` y
/// artista/álbum en `theme().dim`. Ocupa exactamente `width` celdas.
pub(super) fn row_line(
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
            Style::default()
                .bg(bg)
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().ok),
            Style::default()
                .bg(bg)
                .fg(theme().text)
                .add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().dim),
            Style::default().bg(bg).fg(theme().dim),
        )
    } else {
        (
            Style::default()
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
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

pub(super) fn format_duration(duration: Duration) -> String {
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
fn render_visualizer(
    frame: &mut Frame<'_>,
    area: Rect,
    analyzer: &SpectrumAnalyzer,
    focused: bool,
) {
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
        VisualizerPaint::Circular => {
            paint_circular(frame.buffer_mut(), inner, analyzer.bands(), &theme(), ascii)
        }
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
fn paint_miku(buf: &mut ratatui::buffer::Buffer, inner: Rect, analyzer: &SpectrumAnalyzer) {
    // Fondo del lienzo del arte: campo de `@` blancos en todo el área.
    let white = Style::default().fg(Color::Rgb(0xff, 0xff, 0xff));
    for y in inner.y..inner.y + inner.height {
        buf.set_string(inner.x, y, "@".repeat(inner.width as usize), white);
    }

    use crate::visualizer::miku_art as art;
    let frame = (analyzer.dance_clock() / art::ANIM_TICKS as u64) as usize % art::SAMPLED_FRAMES;
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
pub(super) enum VisualizerPaint {
    Bars,
    Wave,
    Mirror,
    Vu,
    Circular,
    Miku,
}

/// Dispatch puro del modo al pintor: cada modo va al suyo, sin fallback.
pub(super) fn resolve_visualizer_paint(mode: config::VisualizerMode) -> VisualizerPaint {
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
pub(super) fn paint_circular(
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
pub(super) fn paint_mirror(
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
pub(super) fn paint_vu(
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
    let bar_rows: u16 = if inner.height <= 2 {
        1
    } else {
        inner.height - 1
    };
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
pub(super) fn paint_wave(
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
                cell.set_symbol(ASCII_RAMP[8])
                    .set_fg(Color::Rgb(red, green, blue));
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
const BRAILLE_DOT_BITS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

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
pub(super) fn wave_column_spans(
    samples: &[f32],
    columns: usize,
    rows: u16,
) -> Vec<Option<(u16, u16)>> {
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
pub(super) fn paint_bands(
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
pub(super) fn spectrum_color(t: f32, theme: &Theme) -> (u8, u8, u8) {
    fn channel(from: u8, to: u8, u: f32) -> u8 {
        (f64::from(from) + (f64::from(to) - f64::from(from)) * f64::from(u)).round() as u8
    }

    fn mix(from: Color, to: Color, u: f32) -> (u8, u8, u8) {
        let (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) = (from, to) else {
            return (0, 0, 0);
        };
        (channel(r0, r1, u), channel(g0, g1, u), channel(b0, b1, u))
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
pub(super) fn footer_lines(app: &App, compact: bool) -> Vec<Line<'static>> {
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
            Line::from(" Backspace: borrar   Enter: aplicar   Esc: cancelar (lista completa) "),
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
        }
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
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            let line_width = line.width();
            let (spans, entries) = footer_chip_spans(&text);
            lines.push(Line::from(spans));
            let Ok(index) = u16::try_from(index) else {
                continue;
            };
            if index >= inner.height {
                continue;
            }
            let Ok(line_width) = u16::try_from(line_width) else {
                continue;
            };
            // Mismo centrado que el Paragraph de abajo (ratatui centra con
            // `(ancho/2) - (línea/2)`, no con `(ancho - línea)/2`: con resto
            // impar difieren en una columna y el chip quedaba corrido).
            let x0 = inner.x + (inner.width / 2).saturating_sub(line_width / 2);
            let right = inner.x + inner.width;
            for (start, width, keys) in entries {
                let Some(code) = key_for_chip(&keys) else {
                    continue;
                };
                let x = x0.saturating_add(start);
                if x >= right {
                    continue;
                }
                let width = width.min(right - x);
                hit.chips
                    .push((Rect::new(x, inner.y + index, width, 1), code));
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
