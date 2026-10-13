use super::*;

/// Barra clickeable registrada al dibujar: columna inicial, fila y ancho del
/// tramo interno (sin etiqueta ni corchetes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BarHit {
    pub(super) x: u16,
    pub(super) y: u16,
    pub(super) width: u16,
}

impl BarHit {
    /// Fracción 0..=1 de la columna dentro de la barra (afuera se recorta).
    pub(super) fn ratio(&self, column: u16) -> f64 {
        if self.width <= 1 {
            return 0.0;
        }
        let last = self.width - 1;
        let offset = column.saturating_sub(self.x).min(last);
        f64::from(offset) / f64::from(last)
    }

    pub(super) fn contains(&self, column: u16, row: u16) -> bool {
        row == self.y && column >= self.x && column < self.x.saturating_add(self.width)
    }
}

/// Zonas clickeables del último frame (las anota el render).
#[derive(Debug, Clone, Default)]
pub(super) struct HitMap {
    pub(super) progress: Option<BarHit>,
    pub(super) volume: Option<BarHit>,
    /// Área del reproductor (info + visualizador): la rueda cambia el volumen.
    pub(super) player: Option<Rect>,
    /// Botones de transporte dibujados (área y botón).
    pub(super) buttons: Vec<(Rect, Button)>,
    /// Chips del footer: área y tecla que simulan al clickearlos.
    pub(super) chips: Vec<(Rect, KeyCode)>,
}

/// Arrastre en curso: sigue a la barra donde empezó aunque el puntero salga.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Drag {
    /// Arrastre sobre el progreso: guarda la última fracción para el seek al
    /// soltar.
    Progress(Option<f64>),
    Volume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Button {
    Prev,
    PlayPause,
    Next,
    Shuffle,
    Repeat,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum MouseAction {
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
pub(super) fn footer_chip_spans(text: &str) -> (Vec<Span<'static>>, Vec<(u16, u16, String)>) {
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
pub(super) fn key_for_chip(keys_text: &str) -> Option<KeyCode> {
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
        &keys.quit,
        &keys.play_pause,
        &keys.volume_up,
        &keys.volume_down,
        &keys.seek_back,
        &keys.seek_fwd,
        &keys.prev_track,
        &keys.next_track,
        &keys.shuffle,
        &keys.repeat,
        &keys.search,
        &keys.speed_up,
        &keys.speed_down,
        &keys.explorer,
        &keys.move_up,
        &keys.move_down,
        &keys.page_up,
        &keys.page_down,
        &keys.home,
        &keys.end,
        &keys.select,
        &keys.back,
        &keys.open_url,
        &keys.enqueue,
        &keys.queue_panel,
        &keys.queue_delete,
        &keys.settings,
    ]
    .into_iter()
    .flatten()
    .copied()
    .find(|code| config::key_label(*code) == first)
}

/// Traduce un evento de mouse a una acción según las zonas del frame y el
/// arrastre en curso; devuelve también el nuevo estado de arrastre.
pub(super) fn mouse_action(
    hit: &HitMap,
    drag: Option<Drag>,
    event: MouseEvent,
) -> (MouseAction, Option<Drag>) {
    let (column, row) = (event.column, event.row);
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let point = ratatui::layout::Position::new(column, row);
            if let Some((_, button)) = hit.buttons.iter().find(|(area, _)| area.contains(point)) {
                (MouseAction::Press(*button), None)
            } else if let Some((_, key)) = hit.chips.iter().find(|(area, _)| area.contains(point)) {
                (MouseAction::Key(*key), None)
            } else if let Some(bar) = hit.progress.filter(|bar| bar.contains(column, row)) {
                (
                    MouseAction::SeekRatio(bar.ratio(column)),
                    Some(Drag::Progress(None)),
                )
            } else if let Some(bar) = hit.volume.filter(|bar| bar.contains(column, row)) {
                (
                    MouseAction::SetVolume(bar.ratio(column)),
                    Some(Drag::Volume),
                )
            } else {
                (MouseAction::None, None)
            }
        }
        // El progreso solo marca la posición mientras se arrastra (el seek va
        // al soltar); el volumen sí sigue en vivo.
        MouseEventKind::Drag(MouseButton::Left) => match (drag, hit.progress, hit.volume) {
            (Some(Drag::Progress(_)), Some(bar), _) => (
                MouseAction::None,
                Some(Drag::Progress(Some(bar.ratio(column)))),
            ),
            (Some(Drag::Volume), _, Some(bar)) => (MouseAction::SetVolume(bar.ratio(column)), drag),
            _ => (MouseAction::None, drag),
        },
        MouseEventKind::Up(MouseButton::Left) => match drag {
            Some(Drag::Progress(Some(ratio))) => (MouseAction::SeekRatio(ratio), None),
            _ => (MouseAction::None, None),
        },
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let over_player = hit
                .player
                .is_some_and(|area| area.contains(ratatui::layout::Position::new(column, row)));
            if !over_player {
                return (MouseAction::None, drag);
            }
            let step = if event.kind == MouseEventKind::ScrollUp {
                1
            } else {
                -1
            };
            (MouseAction::VolumeStep(step), drag)
        }
        _ => (MouseAction::None, drag),
    }
}

/// Aplica un evento de mouse (clic/arrastre en barras, rueda) si el estado
/// vivo `mouse_enabled` (arranca con `[ui] mouse`, el toggle lo cambia)
/// lo permite.
pub(super) fn handle_mouse(event: MouseEvent, app: &mut App, engine: &mut AudioEngine) {
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
        MouseAction::VolumeStep(step) => {
            engine.set_volume((engine.volume() + f32::from(step) * VOLUME_STEP).clamp(0.0, 1.0))
        }
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
pub(super) fn toggle_mouse(app: &mut App, persist: impl FnOnce(bool) -> Result<(), String>) {
    let enabled = !app.mouse_enabled;
    app.mouse_enabled = enabled;
    apply_mouse_capture(enabled);
    if let Err(err) = persist(enabled) {
        app.error = Some(err);
    }
}
