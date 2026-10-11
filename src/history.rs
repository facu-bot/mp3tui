//! Historial de URLs reproducidas (T1).
//!
//! Guarda hasta 20 entradas (título + URL original), ordenadas de la más
//! reciente a la más antigua. Se persiste en formato TOML en el directorio de
//! configuración (`%APPDATA%\mp3tui\history.toml` o el directorio indicado por
//! `MP3TUI_CONFIG`).

use std::fs;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};

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

pub fn history_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|mut p| {
        p.push(HISTORY_FILE_NAME);
        p
    })
}

pub fn load() -> (History, Option<String>) {
    let Some(path) = history_path() else {
        return (History::default(), None);
    };
    if !path.exists() {
        return (History::default(), None);
    }
    match fs::read_to_string(&path) {
        Ok(content) => match History::from_toml(&content) {
            Ok(hist) => (hist, None),
            Err(err) => (
                History::default(),
                Some(format!(
                    "archivo de historial '{}' corrupto: {err}",
                    path.display()
                )),
            ),
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

pub fn save(history: &History) -> Result<(), String> {
    let Some(path) = history_path() else {
        return Err("no se pudo determinar la ruta del historial".to_string());
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let content = history.to_toml()?;
    fs::write(&path, content).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_history_add_and_dedup_moves_to_top() {
        let mut history = History::new();
        history.add("Canción 1".to_string(), "https://example.com/1.mp3".to_string());
        history.add("Canción 2".to_string(), "https://example.com/2.mp3".to_string());
        assert_eq!(history.len(), 2);
        assert_eq!(history.get(0).unwrap().title, "Canción 2");

        // Re-agregar URL existente: debe refrescar el título y moverlo al inicio
        history.add("Canción 1 Nueva".to_string(), "https://example.com/1.mp3".to_string());
        assert_eq!(history.len(), 2, "no debe duplicar la URL existente");
        assert_eq!(history.get(0).unwrap().title, "Canción 1 Nueva");
        assert_eq!(history.get(0).unwrap().url, "https://example.com/1.mp3");
        assert_eq!(history.get(1).unwrap().title, "Canción 2");
    }

    #[test]
    fn test_history_trim_to_max_20() {
        let mut history = History::new();
        for i in 0..25 {
            history.add(format!("Canción {i}"), format!("https://example.com/{i}.mp3"));
        }
        assert_eq!(history.len(), 20, "debe acotar a un máximo de 20 entradas");
        assert_eq!(history.get(0).unwrap().title, "Canción 24", "la más reciente debe estar arriba");
        assert_eq!(history.get(19).unwrap().title, "Canción 5", "la más antigua preservada");
    }

    #[test]
    fn test_history_toml_round_trip() {
        let mut history = History::new();
        history.add("Tema 1".to_string(), "https://example.com/1.mp3".to_string());
        history.add("Tema 2".to_string(), "https://example.com/2.mp3".to_string());

        let toml_str = history.to_toml().expect("serialización toml debe funcionar");
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
        let temp_dir = std::env::temp_dir().join(format!("mp3tui_hist_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        let config_file = temp_dir.join("config.toml");
        let _ = fs::write(&config_file, "");

        unsafe {
            std::env::set_var(crate::config::CONFIG_ENV_OVERRIDE, &config_file);
        }

        let mut history = History::new();
        history.add("Canción Guardada".to_string(), "https://example.com/saved.mp3".to_string());
        save(&history).expect("debe guardar en temp dir");

        let (loaded, warning) = load();
        assert!(warning.is_none(), "no debe dar warning para archivo válido: {warning:?}");
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
        let temp_dir = std::env::temp_dir().join(format!("mp3tui_hist_corrupt_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        let config_file = temp_dir.join("config.toml");
        let _ = fs::write(&config_file, "");
        let history_file = temp_dir.join(HISTORY_FILE_NAME);
        let _ = fs::write(&history_file, "[[entries\ninvalid = [");

        unsafe {
            std::env::set_var(crate::config::CONFIG_ENV_OVERRIDE, &config_file);
        }

        let (loaded, warning) = load();
        assert_eq!(loaded.len(), 0, "debe dar historial vacío si el archivo es corrupto");
        assert!(warning.is_some(), "debe retornar un warning ante archivo corrupto");
        assert!(warning.unwrap().contains("corrupto"));

        unsafe {
            std::env::remove_var(crate::config::CONFIG_ENV_OVERRIDE);
        }
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
