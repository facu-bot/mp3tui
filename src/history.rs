//! Historial de playlists y álbumes de Spotify reproducidos.
//!
//! Guarda hasta 20 entradas (id, nombre, cantidad de canciones y los nombres
//! de las primeras), de la más reciente a la más antigua. Se persiste en
//! formato TOML en el directorio de configuración (la misma carpeta que
//! `config.toml`: `%APPDATA%\mp3tui` en Windows, `~/.config/mp3tui` en
//! Linux/macOS; o el directorio indicado por `MP3TUI_CONFIG`).
//!
//! El archivo de versiones anteriores guardaba URLs reproducidas
//! (`title` + `url`): esa forma se reconoce y se descarta sin avisar de
//! corrupción, arrancando con el historial vacío.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const MAX_ENTRIES: usize = 20;
/// Cuántos nombres de canciones se guardan por entrada.
pub const SAMPLE_TRACKS: usize = 5;
pub const HISTORY_FILE_NAME: &str = "history.toml";

/// Qué tipo de colección de Spotify es la entrada.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectionKind {
    Playlist,
    Album,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub kind: CollectionKind,
    pub id: String,
    pub name: String,
    pub track_count: usize,
    /// Nombres de las primeras canciones («Artistas - Título»).
    #[serde(default)]
    pub sample_tracks: Vec<String>,
}

impl HistoryEntry {
    /// Entrada con las primeras [`SAMPLE_TRACKS`] canciones de `track_names`.
    pub fn new(
        kind: CollectionKind,
        id: String,
        name: String,
        track_count: usize,
        track_names: &[String],
    ) -> Self {
        Self {
            kind,
            id,
            name,
            track_count,
            sample_tracks: track_names.iter().take(SAMPLE_TRACKS).cloned().collect(),
        }
    }

    /// Marcador que entiende el flujo de URLs para volver a reproducirla.
    pub fn marker(&self) -> String {
        match self.kind {
            CollectionKind::Playlist => format!("spotify:playlist:{}", self.id),
            CollectionKind::Album => format!("spotify:album:{}", self.id),
        }
    }

    /// «Nombre - N canciones».
    pub fn title_line(&self) -> String {
        let count = match self.track_count {
            1 => "1 canción".to_string(),
            n => format!("{n} canciones"),
        };
        format!("{} - {count}", self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    pub entries: Vec<HistoryEntry>,
}

/// Forma del archivo de versiones anteriores (URLs reproducidas).
#[derive(Deserialize)]
struct LegacyHistory {
    #[serde(default)]
    #[allow(dead_code)]
    entries: Vec<LegacyEntry>,
}

#[derive(Deserialize)]
struct LegacyEntry {
    #[allow(dead_code)]
    title: String,
    #[allow(dead_code)]
    url: String,
}

impl History {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&HistoryEntry> {
        self.entries.get(index)
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Registra la colección al frente; si ya estaba (mismo tipo e id) la
    /// refresca en vez de duplicarla.
    pub fn add(&mut self, entry: HistoryEntry) {
        self.entries
            .retain(|e| !(e.kind == entry.kind && e.id == entry.id));
        self.entries.insert(0, entry);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.truncate(MAX_ENTRIES);
        }
    }

    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    /// Parsea el archivo. Un archivo con la forma vieja (URLs) no es un
    /// error: se descarta y queda el historial vacío.
    pub fn from_toml(content: &str) -> Result<Self, String> {
        match toml::from_str::<Self>(content) {
            Ok(history) => Ok(history),
            Err(err) => match toml::from_str::<LegacyHistory>(content) {
                Ok(_) => Ok(Self::default()),
                Err(_) => Err(err.to_string()),
            },
        }
    }
}

pub fn history_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|mut p| {
        p.push(HISTORY_FILE_NAME);
        p
    })
}

pub fn load_from(path: &std::path::Path) -> (History, Option<String>) {
    if !path.exists() {
        return (History::default(), None);
    }
    match fs::read_to_string(path) {
        Ok(content) => match History::from_toml(&content) {
            Ok(hist) => (hist, None),
            Err(err) => {
                let bak_path = path.with_extension("toml.bak");
                let _ = fs::copy(path, &bak_path);
                (
                    History::default(),
                    Some(format!(
                        "archivo de historial '{}' corrupto: {err}",
                        path.display()
                    )),
                )
            }
        },
        Err(err) => (
            History::default(),
            Some(format!(
                "no se pudo leer historial '{}': {err}",
                path.display()
            )),
        ),
    }
}

pub fn load() -> (History, Option<String>) {
    let Some(path) = history_path() else {
        return (History::default(), None);
    };
    load_from(&path)
}

static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn save_to(history: &History, path: &std::path::Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let content = history.to_toml()?;
    let temp_name = format!(
        "{}.tmp.{}.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("history"),
        std::process::id(),
        TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let temp_path = path.with_file_name(temp_name);
    fs::write(&temp_path, &content).map_err(|e| e.to_string())?;
    fs::rename(&temp_path, path).map_err(|e| {
        let _ = fs::remove_file(&temp_path);
        e.to_string()
    })
}

pub fn save(history: &History) -> Result<(), String> {
    let Some(path) = history_path() else {
        return Err("no se pudo determinar la ruta del historial".to_string());
    };
    save_to(history, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("Artista - Tema {i}")).collect()
    }

    fn entry(kind: CollectionKind, id: &str, name: &str, count: usize) -> HistoryEntry {
        HistoryEntry::new(kind, id.to_string(), name.to_string(), count, &names(count))
    }

    #[test]
    fn test_entry_keeps_only_the_first_five_track_names() {
        let e = entry(CollectionKind::Playlist, "p1", "Mix", 12);
        assert_eq!(e.track_count, 12);
        assert_eq!(e.sample_tracks, names(5));
    }

    #[test]
    fn test_entry_marker_and_title_line() {
        let p = entry(CollectionKind::Playlist, "p1", "Mix", 3);
        assert_eq!(p.marker(), "spotify:playlist:p1");
        assert_eq!(p.title_line(), "Mix - 3 canciones");
        let a = entry(CollectionKind::Album, "a1", "Disco", 1);
        assert_eq!(a.marker(), "spotify:album:a1");
        assert_eq!(a.title_line(), "Disco - 1 canción");
    }

    #[test]
    fn test_history_add_dedups_by_kind_and_id_and_moves_to_top() {
        let mut history = History::new();
        history.add(entry(CollectionKind::Playlist, "p1", "Uno", 2));
        history.add(entry(CollectionKind::Playlist, "p2", "Dos", 2));
        assert_eq!(history.get(0).unwrap().name, "Dos");

        history.add(entry(CollectionKind::Playlist, "p1", "Uno renombrada", 4));
        assert_eq!(history.len(), 2, "no duplica la misma playlist");
        assert_eq!(history.get(0).unwrap().name, "Uno renombrada");
        assert_eq!(history.get(0).unwrap().track_count, 4);

        // Mismo id pero otro tipo: es otra entrada.
        history.add(entry(CollectionKind::Album, "p1", "Álbum", 2));
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn test_history_trim_to_max_20() {
        let mut history = History::new();
        for i in 0..25 {
            history.add(entry(CollectionKind::Playlist, &format!("p{i}"), &format!("L{i}"), 1));
        }
        assert_eq!(history.len(), MAX_ENTRIES);
        assert_eq!(history.get(0).unwrap().name, "L24");
        assert_eq!(history.get(19).unwrap().name, "L5");
    }

    #[test]
    fn test_history_toml_round_trip() {
        let mut history = History::new();
        history.add(entry(CollectionKind::Album, "a1", "Disco \"raro\"", 7));
        history.add(entry(CollectionKind::Playlist, "p1", "Mix", 0));
        let text = history.to_toml().unwrap();
        assert_eq!(History::from_toml(&text).unwrap(), history);
    }

    #[test]
    fn test_legacy_url_history_is_tolerated_as_empty_without_error() {
        let legacy = r#"
            [[entries]]
            title = "Canción 1"
            url = "https://example.com/1.mp3"

            [[entries]]
            title = "Otra"
            url = "spotify:track:abc"
        "#;
        let history = History::from_toml(legacy).expect("la forma vieja no es un error");
        assert!(history.is_empty());
        assert_eq!(History::from_toml("").unwrap(), History::default());
        assert_eq!(History::from_toml("entries = []").unwrap(), History::default());
    }

    #[test]
    fn test_history_corrupt_yields_error() {
        assert!(History::from_toml("[[entries\nurl = invalid").is_err());
        // Forma nueva a medias (sin id): tampoco es la vieja, es corrupta.
        assert!(History::from_toml("[[entries]]\nname = \"x\"").is_err());
    }

    fn temp_file(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mp3tui_hist_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(HISTORY_FILE_NAME);
        (dir, file)
    }

    #[test]
    fn test_load_from_legacy_file_starts_empty_without_warning_and_saves_new_shape() {
        let (dir, file) = temp_file("legacy");
        fs::write(&file, "[[entries]]\ntitle = \"t\"\nurl = \"https://x/y.mp3\"\n").unwrap();

        let (mut history, warning) = load_from(&file);
        assert!(history.is_empty());
        assert!(warning.is_none(), "{warning:?}");

        history.add(entry(CollectionKind::Playlist, "p1", "Mix", 2));
        save_to(&history, &file).unwrap();
        let (reloaded, warning) = load_from(&file);
        assert!(warning.is_none());
        assert_eq!(reloaded, history);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_from_missing_file_is_empty() {
        let (dir, file) = temp_file("missing");
        let (history, warning) = load_from(&file);
        assert!(history.is_empty() && warning.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_corrupt_file_returns_warning_and_keeps_a_backup() {
        let (dir, file) = temp_file("corrupt");
        let corrupt = "broken toml [[[]]";
        fs::write(&file, corrupt).unwrap();

        let (mut history, warning) = load_from(&file);
        assert!(history.is_empty());
        assert!(warning.expect("debe avisar").contains("corrupto"));

        history.add(entry(CollectionKind::Playlist, "p1", "Mix", 1));
        save_to(&history, &file).unwrap();
        let bak = file.with_extension("toml.bak");
        assert_eq!(fs::read_to_string(&bak).unwrap(), corrupt);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_to_uses_temp_and_rename_without_leaving_temp_files() {
        let (dir, file) = temp_file("atomic");
        let mut history = History::new();
        history.add(entry(CollectionKind::Album, "a1", "Disco", 3));
        save_to(&history, &file).unwrap();

        assert!(file.exists());
        for item in fs::read_dir(&dir).unwrap().flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            assert!(!name.contains("tmp"), "no debe quedar archivo temporal: {name}");
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
