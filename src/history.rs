//! Historial de URLs reproducidas.
//!
//! Guarda hasta 20 entradas (título + URL original), ordenadas de la más
//! reciente a la más antigua. Se persiste en formato TOML en el directorio de
//! configuración (la misma carpeta que `config.toml`: `%APPDATA%\mp3tui` en
//! Windows, `~/.config/mp3tui` en Linux/macOS; o el directorio indicado por
//! `MP3TUI_CONFIG`).

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const MAX_ENTRIES: usize = 20;
pub const HISTORY_FILE_NAME: &str = "history.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    pub entries: Vec<HistoryEntry>,
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

    pub fn add(&mut self, title: String, url: String) {
        self.entries.retain(|e| e.url != url);
        self.entries.insert(0, HistoryEntry { title, url });
        if self.entries.len() > MAX_ENTRIES {
            self.entries.truncate(MAX_ENTRIES);
        }
    }

    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    pub fn from_toml(content: &str) -> Result<Self, String> {
        toml::from_str::<Self>(content).map_err(|e| e.to_string())
    }
}

pub fn sanitize_url(url: &str) -> String {
    let (scheme, rest) = if let Some(idx) = url.find("://") {
        (&url[..idx + 3], &url[idx + 3..])
    } else {
        ("", url)
    };

    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let remainder = &rest[auth_end..];

    if let Some(at_idx) = authority.rfind('@') {
        let host_port = &authority[at_idx + 1..];
        format!("{scheme}{host_port}{remainder}")
    } else {
        url.to_string()
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

    #[test]
    fn test_history_add_and_dedup_moves_to_top() {
        let mut history = History::new();
        history.add(
            "Canción 1".to_string(),
            "https://example.com/1.mp3".to_string(),
        );
        history.add(
            "Canción 2".to_string(),
            "https://example.com/2.mp3".to_string(),
        );
        assert_eq!(history.len(), 2);
        assert_eq!(history.get(0).unwrap().title, "Canción 2");

        // Re-agregar URL existente: debe refrescar el título y moverlo al inicio
        history.add(
            "Canción 1 Nueva".to_string(),
            "https://example.com/1.mp3".to_string(),
        );
        assert_eq!(history.len(), 2, "no debe duplicar la URL existente");
        assert_eq!(history.get(0).unwrap().title, "Canción 1 Nueva");
        assert_eq!(history.get(0).unwrap().url, "https://example.com/1.mp3");
        assert_eq!(history.get(1).unwrap().title, "Canción 2");
    }

    #[test]
    fn test_history_trim_to_max_20() {
        let mut history = History::new();
        for i in 0..25 {
            history.add(
                format!("Canción {i}"),
                format!("https://example.com/{i}.mp3"),
            );
        }
        assert_eq!(history.len(), 20, "debe acotar a un máximo de 20 entradas");
        assert_eq!(
            history.get(0).unwrap().title,
            "Canción 24",
            "la más reciente debe estar arriba"
        );
        assert_eq!(
            history.get(19).unwrap().title,
            "Canción 5",
            "la más antigua preservada"
        );
    }

    #[test]
    fn test_history_toml_round_trip() {
        let mut history = History::new();
        history.add(
            "Tema 1".to_string(),
            "https://example.com/1.mp3".to_string(),
        );
        history.add(
            "Tema 2".to_string(),
            "https://example.com/2.mp3".to_string(),
        );

        let toml_str = history
            .to_toml()
            .expect("serialización toml debe funcionar");
        let parsed = History::from_toml(&toml_str).expect("deserialización toml debe funcionar");
        assert_eq!(history, parsed);
    }

    #[test]
    fn test_history_corrupt_yields_empty_and_warning() {
        let corrupt_toml = "[[entries\nurl = invalid";
        let res = History::from_toml(corrupt_toml);
        assert!(res.is_err(), "debe fallar ante toml corrupto");
    }

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_history_persistence_in_temp_dir_via_config_override() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let temp_dir =
            std::env::temp_dir().join(format!("mp3tui_hist_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        let config_file = temp_dir.join("config.toml");
        let _ = fs::write(&config_file, "");

        unsafe {
            std::env::set_var(crate::config::CONFIG_ENV_OVERRIDE, &config_file);
        }

        let mut history = History::new();
        history.add(
            "Canción Guardada".to_string(),
            "https://example.com/saved.mp3".to_string(),
        );
        save(&history).expect("debe guardar en temp dir");

        let (loaded, warning) = load();
        assert!(
            warning.is_none(),
            "no debe dar warning para archivo válido: {warning:?}"
        );
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get(0).unwrap().title, "Canción Guardada");

        // Limpieza de variable
        unsafe {
            std::env::remove_var(crate::config::CONFIG_ENV_OVERRIDE);
        }
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_history_load_corrupt_file_returns_warning() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let temp_dir =
            std::env::temp_dir().join(format!("mp3tui_hist_corrupt_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        let config_file = temp_dir.join("config.toml");
        let _ = fs::write(&config_file, "");
        let history_file = temp_dir.join(HISTORY_FILE_NAME);
        let _ = fs::write(&history_file, "[[entries\ninvalid = [");

        unsafe {
            std::env::set_var(crate::config::CONFIG_ENV_OVERRIDE, &config_file);
        }

        let (loaded, warning) = load();
        assert_eq!(
            loaded.len(),
            0,
            "debe dar historial vacío si el archivo es corrupto"
        );
        assert!(
            warning.is_some(),
            "debe retornar un warning ante archivo corrupto"
        );
        assert!(warning.unwrap().contains("corrupto"));

        unsafe {
            std::env::remove_var(crate::config::CONFIG_ENV_OVERRIDE);
        }
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_sanitize_url_strips_userinfo_and_handles_unicode() {
        assert_eq!(
            sanitize_url("http://user:pass@example.com/audio.mp3"),
            "http://example.com/audio.mp3"
        );
        assert_eq!(
            sanitize_url("https://alice:s3cr3t@domain.com:8080/path?q=val&u=user@other#hash"),
            "https://domain.com:8080/path?q=val&u=user@other#hash"
        );
        assert_eq!(
            sanitize_url("http://usér:páss@example.com/música.mp3?query=val"),
            "http://example.com/música.mp3?query=val"
        );
        assert_eq!(
            sanitize_url("https://example.com/clean.mp3"),
            "https://example.com/clean.mp3"
        );
    }

    #[test]
    fn test_corrupt_history_preserved_in_bak_after_save() {
        let temp_dir = std::env::temp_dir().join(format!("mp3tui_hist_bak_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        let hist_file = temp_dir.join("history.toml");
        let corrupt_bytes = "broken toml [[[]]";
        fs::write(&hist_file, corrupt_bytes).unwrap();

        let (mut hist, warn) = load_from(&hist_file);
        assert!(warn.is_some());
        assert_eq!(hist.len(), 0);

        hist.add(
            "New Title".to_string(),
            "https://example.com/audio.mp3".to_string(),
        );
        save_to(&hist, &hist_file).unwrap();

        let bak_file = hist_file.with_extension("toml.bak");
        assert!(bak_file.exists(), "debe crear respaldo .bak");
        assert_eq!(fs::read_to_string(&bak_file).unwrap(), corrupt_bytes);

        let (reloaded, warn2) = load_from(&hist_file);
        assert!(warn2.is_none());
        assert_eq!(reloaded.len(), 1);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_save_to_uses_temp_and_rename_without_leaving_temp_files() {
        let temp_dir =
            std::env::temp_dir().join(format!("mp3tui_hist_atomic_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        let hist_file = temp_dir.join("history.toml");

        let mut hist = History::new();
        hist.add(
            "Atomic Title".to_string(),
            "https://example.com/atomic.mp3".to_string(),
        );
        save_to(&hist, &hist_file).unwrap();

        assert!(hist_file.exists());
        let entries = fs::read_dir(&temp_dir).unwrap();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !name.contains("tmp"),
                "no debe quedar archivo temporal: {name}"
            );
        }

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
