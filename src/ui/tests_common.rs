#![cfg(test)]

use super::*;

/// Contador global de ids para historiales temporales aislados por test.
pub(crate) static TEST_HIST_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Serializa los tests del menú de temas (mutan el tema activo en `runtime`).
pub(crate) static THEME_MENU_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Serializa los tests del menú de bandas (mutan el visualizador en `runtime`).
pub(crate) static BANDS_MENU_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Serializa los tests del menú de modo (mutan el visualizador en `runtime`).
pub(crate) static MODE_MENU_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn sample_track() -> Track {
    Track {
        path: PathBuf::from(r"C:\music\cancion.mp3"),
        title: "Una canción con un título bastante largo".to_string(),
        artist: Some("Artista".to_string()),
        album: Some("Álbum".to_string()),
        duration: Some(Duration::from_secs(65)),
        folder: "music".to_string(),
    }
}
pub(crate) fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}
pub(crate) fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.to_string())
        .collect()
}
pub(crate) fn test_app() -> App {
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
        spotify_search: None,
        spotify_search_rx: None,
        spotify_search_id: 0,
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
pub(crate) fn test_temp_history_path() -> PathBuf {
    let id = TEST_HIST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mp3tui_test_hist_{}_{}.toml",
        std::process::id(),
        id
    ))
}
pub(crate) fn track_named(name: &str) -> Track {
    Track {
        path: PathBuf::from(format!(r"C:\x\{name}.mp3")),
        title: name.to_string(),
        artist: None,
        album: None,
        duration: None,
        folder: "x".to_string(),
    }
}
pub(crate) fn library_with_three() -> App {
    let mut app = test_app();
    app.library = Some(crate::playlist::from_tracks_for_test(vec![
        track_named("alpha"),
        track_named("beta"),
        track_named("gamma"),
    ]));
    refresh_view(&mut app);
    app
}
pub(crate) fn track_in(name: &str, folder: &str) -> Track {
    Track {
        path: PathBuf::from(format!(r"C:\lib\{folder}\{name}.mp3")),
        title: name.to_string(),
        artist: None,
        album: None,
        duration: None,
        folder: folder.to_string(),
    }
}
pub(crate) fn library_two_folders() -> App {
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
pub(crate) struct ThemeMenuTestGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    name: String,
    theme: Theme,
}
impl Drop for ThemeMenuTestGuard {
    fn drop(&mut self) {
        runtime::activate_theme(&self.name, self.theme.clone());
    }
}
pub(crate) fn theme_menu_test_setup() -> ThemeMenuTestGuard {
    let lock = THEME_MENU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ThemeMenuTestGuard {
        _lock: lock,
        name: runtime::active_theme_name(),
        theme: runtime::active_theme(),
    }
}
pub(crate) fn theme_menu_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
pub(crate) fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}
pub(crate) fn sample_hit() -> HitMap {
    HitMap {
        progress: Some(BarHit {
            x: 20,
            y: 6,
            width: 51,
        }),
        volume: Some(BarHit {
            x: 20,
            y: 5,
            width: 11,
        }),
        player: Some(Rect::new(0, 0, 100, 20)),
        buttons: Vec::new(),
        chips: Vec::new(),
    }
}
pub(crate) fn render_player_frame(app: &App, engine: &AudioEngine) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(100, 40);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
    terminal
        .draw(|frame| render(frame, app, engine))
        .expect("dibuja");
    terminal.backend().buffer().clone()
}
pub(crate) fn player_screen_text(buffer: &ratatui::buffer::Buffer) -> String {
    (0..buffer.area.height)
        .flat_map(|y| {
            (0..buffer.area.width)
                .map(move |x| buffer[(x, y)].symbol().to_string())
                .chain(std::iter::once("\n".to_string()))
        })
        .collect()
}
pub(crate) fn row_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}
pub(crate) fn find_search_box(line: &Line<'_>) -> Option<Span<'static>> {
    line.spans
        .iter()
        .find(|span| span.style.bg == Some(theme().select_bg))
        .map(|span| Span::styled(span.content.to_string(), span.style))
}
pub(crate) fn engine_or_skip() -> Option<AudioEngine> {
    AudioEngine::new().ok()
}
pub(crate) struct BandsMenuTestGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    style: config::VisualizerStyle,
}
impl Drop for BandsMenuTestGuard {
    fn drop(&mut self) {
        runtime::activate_visualizer_style(self.style);
    }
}
pub(crate) fn bands_menu_test_setup() -> BandsMenuTestGuard {
    let lock = BANDS_MENU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    BandsMenuTestGuard {
        _lock: lock,
        style: runtime::active_visualizer_style(),
    }
}
pub(crate) struct ModeMenuTestGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    mode: config::VisualizerMode,
}
impl Drop for ModeMenuTestGuard {
    fn drop(&mut self) {
        runtime::activate_visualizer_mode(self.mode);
    }
}
pub(crate) fn mode_menu_test_setup() -> ModeMenuTestGuard {
    let lock = MODE_MENU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ModeMenuTestGuard {
        _lock: lock,
        mode: runtime::active_visualizer_mode(),
    }
}
pub(crate) fn make_test_wav(num_samples: usize) -> Vec<u8> {
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
pub(crate) fn local_wav_url(filename: &str, wav: &[u8]) -> String {
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
pub(crate) fn track_with_path(path: PathBuf, name: &str) -> Track {
    Track {
        path,
        title: name.to_string(),
        artist: None,
        album: None,
        duration: None,
        folder: "x".to_string(),
    }
}
pub(crate) fn queue_url(title: &str) -> QueueItem {
    QueueItem::Url {
        url: format!("https://x/{title}"),
        title: title.to_string(),
    }
}
pub(crate) fn queue_fallback_app() -> (App, PathBuf, PathBuf) {
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
pub(crate) fn empty_resolve_rx(
    urls: &std::cell::RefCell<Vec<String>>,
) -> impl FnMut(String, u64) -> Receiver<YtDlpMessage> + '_ {
    move |url: String, _req_id: u64| {
        urls.borrow_mut().push(url);
        let (_tx, rx) = mpsc::channel();
        rx
    }
}
pub(crate) fn empty_list_rx(
    urls: &std::cell::RefCell<Vec<String>>,
) -> impl FnMut(String, u64) -> Receiver<YtDlpListMessage> + '_ {
    move |url: String, _req_id: u64| {
        urls.borrow_mut().push(url);
        let (_tx, rx) = mpsc::channel();
        rx
    }
}
pub(crate) fn playlist_entry(title: &str, id: &str) -> crate::source::ytdlp::PlaylistEntry {
    crate::source::ytdlp::PlaylistEntry {
        title: title.to_string(),
        page_url: format!("https://www.youtube.com/watch?v={id}"),
    }
}
