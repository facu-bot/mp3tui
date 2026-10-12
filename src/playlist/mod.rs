//! Biblioteca y playlist: escaneo recursivo de carpetas + metadata con lofty.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::prelude::{Accessor, AudioFile, TaggedFileExt};
use lofty::read_from_path;

/// Extensiones de audio soportadas por el explorador.
const AUDIO_EXTENSIONS: [&str; 4] = ["mp3", "flac", "ogg", "wav"];
/// Tope del scan: evita colapsar si alguien apunta a una carpeta enorme.
const MAX_TRACKS: usize = 10_000;

/// Una pista de audio de la biblioteca.
pub struct Track {
    pub path: PathBuf,
    /// Título del tag o, si no hay tags, el nombre del archivo sin extensión.
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    /// Carpeta de origen a mostrar como encabezado en el explorador:
    /// relativa a la raíz escaneada (con `/`), o el nombre de la raíz si el
    /// archivo vive en la raíz misma. Se resuelve en el escaneo, no al
    /// renderizar.
    pub folder: String,
}

impl Track {
    /// Lee la metadata con lofty; si el archivo no se puede parsear (o no tiene
    /// tags) usa el nombre de archivo como título.
    pub fn from_path(path: PathBuf) -> Self {
        let folder = path
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let fallback_title = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "desconocido".to_string());

        let mut track = Track {
            path,
            title: fallback_title,
            artist: None,
            album: None,
            duration: None,
            folder,
        };

        if let Ok(tagged) = read_from_path(&track.path) {
            track.duration = Some(tagged.properties().duration());
            if let Some(tag) = tagged.primary_tag() {
                if let Some(title) = tag.title().filter(|value| !value.trim().is_empty()) {
                    track.title = title.to_string();
                }
                track.artist = tag
                    .artist()
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| value.into_owned());
                track.album = tag
                    .album()
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| value.into_owned());
            }
        }

        track
    }

    /// Metadatos secundarios para la lista: "artista · álbum" (lo que exista).
    pub fn secondary(&self) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        if let Some(artist) = &self.artist {
            parts.push(artist);
        }
        if let Some(album) = &self.album {
            parts.push(album);
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" \u{b7} "))
        }
    }
}

/// Playlist / biblioteca de pistas.
pub struct Playlist {
    tracks: Vec<Track>,
}

impl Playlist {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
        }
    }

    /// Escaneo recursivo de `root` con `std::fs` (sin crates externos),
    /// manteniendo solo archivos de audio y agrupados por carpeta de origen
    /// ver [`Self::from_dirs_scan`]. Atajo solo para tests.
    #[cfg(test)]
    pub fn from_dir_scan(root: &Path) -> Self {
        Self::from_dirs_scan(&[root.to_path_buf()])
    }

    /// Escaneo recursivo de varias raíces: los tracks quedan agrupados
    /// por carpeta de origen — en el orden de `roots` y, dentro de cada raíz,
    /// con las carpetas en orden alfabético y las pistas por nombre de
    /// archivo (sin distinguir mayúsculas) — para que la lista del explorador
    /// se muestre agrupada con encabezados navegables. El tope de
    /// [`MAX_TRACKS`] es global.
    pub fn from_dirs_scan(roots: &[PathBuf]) -> Self {
        let mut tracks = Vec::new();
        'roots: for root in roots {
            let mut paths = Vec::new();
            collect_audio_files(root, &mut paths);

            // Agrupa por carpeta contenedora (primer encuentro = orden del
            // walk; el sort posterior lo deja alfabético).
            let mut groups: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
            for path in paths {
                let dir = path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.clone());
                match groups.iter_mut().find(|(group, _)| *group == dir) {
                    Some((_, list)) => list.push(path),
                    None => groups.push((dir, vec![path])),
                }
            }
            groups.sort_by(|(a, _), (b, _)| {
                a.to_string_lossy()
                    .to_lowercase()
                    .cmp(&b.to_string_lossy().to_lowercase())
            });

            for (dir, mut list) in groups {
                list.sort_by_key(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().to_lowercase())
                        .unwrap_or_default()
                });
                let folder = folder_header(root, &dir);
                for path in list {
                    if tracks.len() >= MAX_TRACKS {
                        break 'roots;
                    }
                    let mut track = Track::from_path(path);
                    track.folder = folder.clone();
                    tracks.push(track);
                }
            }
        }

        Self { tracks }
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&Track> {
        self.tracks.get(index)
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Índice de una pista por path (para sincronizar la selección al
    /// entrar al explorador desde un archivo arrancado por CLI).
    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.tracks.iter().position(|track| track.path == path)
    }
}

impl Default for Playlist {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
pub(crate) fn from_tracks_for_test(tracks: Vec<Track>) -> Playlist {
    Playlist { tracks }
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            AUDIO_EXTENSIONS
                .iter()
                .any(|supported| ext.eq_ignore_ascii_case(supported))
        })
}

/// Walk manual con std::fs. Usa `file_type()` (no sigue symlinks) para no
/// entrar en bucles; los errores de permisos se ignoran por directorio.
fn collect_audio_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_TRACKS {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_TRACKS {
            return;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => collect_audio_files(&path, out),
            Ok(file_type) if file_type.is_file() && is_audio_file(&path) => out.push(path),
            _ => {}
        }
    }
}

/// Encabezado a mostrar para la carpeta `dir` dentro de `root`: la
/// ruta relativa con `/` como separador, o el nombre de la raíz si el track
/// vive en la raíz misma (o si `dir` no está debajo de `root`).
fn folder_header(root: &Path, dir: &Path) -> String {
    match dir.strip_prefix(root) {
        Ok(relative) if !relative.as_os_str().is_empty() => {
            relative.to_string_lossy().replace('\\', "/")
        }
        _ => root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned()),
    }
}

/// Mueve la selección con clamp a los límites de la lista (nunca sale del rango).
pub fn move_selection(selected: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let next = (selected as i64).saturating_add(delta as i64);
    next.clamp(0, len as i64 - 1) as usize
}

/// Primer índice visible para mantener la selección a la vista (centrada
/// cuando hay lugar, ajustada al final de la lista cuando se pasa).
pub fn scroll_offset(selected: usize, len: usize, view_height: usize) -> usize {
    if len == 0 || view_height == 0 {
        return 0;
    }
    if len <= view_height {
        return 0;
    }
    let half = view_height / 2;
    let centered = selected.saturating_sub(half);
    let max_offset = len - view_height;
    centered.min(max_offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "mp3tui_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn scan_groups_by_folder_and_sorts_by_name() {
        let root = temp_root("scan");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("beta.mp3"), b"").unwrap();
        fs::write(root.join("Alpha.mp3"), b"").unwrap();
        fs::write(root.join("not-audio.txt"), b"").unwrap();
        fs::write(root.join("sub").join("gamma.wav"), b"").unwrap();
        fs::write(root.join("track.FLAC"), b"").unwrap();

        let playlist = Playlist::from_dir_scan(&root);
        let names: Vec<String> = playlist
            .tracks()
            .iter()
            .map(|track| {
                track
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        // Primero el grupo de la raíz (orden alfabético), después las
        // subcarpetas — ya no es un orden global por nombre.
        assert_eq!(
            names,
            vec!["Alpha.mp3", "beta.mp3", "track.FLAC", "gamma.wav"]
        );
        assert!(!names.iter().any(|name| name.ends_with(".txt")));

        let root_name = root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let folders: Vec<&str> = playlist
            .tracks()
            .iter()
            .map(|track| track.folder.as_str())
            .collect();
        assert_eq!(
            folders,
            vec![
                root_name.as_str(),
                root_name.as_str(),
                root_name.as_str(),
                "sub"
            ]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn multi_root_scan_groups_by_folder_in_root_order() {
        let root_a = temp_root("multi_a");
        let root_b = temp_root("multi_b");
        fs::create_dir_all(root_a.join("sub")).unwrap();
        fs::write(root_a.join("beta.mp3"), b"").unwrap();
        fs::write(root_a.join("alpha.mp3"), b"").unwrap();
        fs::write(root_a.join("sub").join("gamma.wav"), b"").unwrap();
        fs::write(root_b.join("delta.flac"), b"").unwrap();

        let playlist =
            Playlist::from_dirs_scan(&[root_a.clone(), root_b.clone()]);
        let names: Vec<String> = playlist
            .tracks()
            .iter()
            .map(|track| {
                track
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec!["alpha.mp3", "beta.mp3", "gamma.wav", "delta.flac"]
        );

        let name_a = root_a.file_name().unwrap().to_string_lossy().into_owned();
        let name_b = root_b.file_name().unwrap().to_string_lossy().into_owned();
        let folders: Vec<&str> = playlist
            .tracks()
            .iter()
            .map(|track| track.folder.as_str())
            .collect();
        assert_eq!(
            folders,
            vec![
                name_a.as_str(),
                name_a.as_str(),
                "sub",
                name_b.as_str()
            ]
        );

        let _ = fs::remove_dir_all(&root_a);
        let _ = fs::remove_dir_all(&root_b);
    }

    /// Escaneo del set por defecto contra el perfil real del usuario:
    /// diagnóstico de rendimiento, fuera de la suite normal. Imprime el
    /// total (frío, como al arrancar) y el desglose por raíz (cálido).
    /// `cargo test perf_scan_default_roots -- --ignored --nocapture`
    #[test]
    #[ignore = "escanea carpetas reales del usuario; corrida manual"]
    fn perf_scan_default_roots_reports_duration() {
        let roots = crate::config::LibraryConfig::default().scan_roots();
        let start = std::time::Instant::now();
        let playlist = Playlist::from_dirs_scan(&roots);
        println!(
            "total: roots={} tracks={} en {:?}",
            roots.len(),
            playlist.len(),
            start.elapsed()
        );
        for root in &roots {
            let root_start = std::time::Instant::now();
            let root_playlist = Playlist::from_dirs_scan(std::slice::from_ref(root));
            println!(
                "  {} -> {} tracks en {:?}",
                root.display(),
                root_playlist.len(),
                root_start.elapsed()
            );
        }
        assert!(!roots.is_empty());
    }

    #[test]
    fn scan_falls_back_to_filename_without_tags() {
        let root = temp_root("fallback");
        fs::write(root.join("cancion.mp3"), b"not really an mp3").unwrap();

        let playlist = Playlist::from_dir_scan(&root);
        assert_eq!(playlist.len(), 1);
        let track = playlist.get(0).unwrap();
        assert_eq!(track.title, "cancion");
        assert!(track.artist.is_none());
        assert!(track.duration.is_none());
        assert!(track.secondary().is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_of_empty_or_missing_dir_is_empty_not_a_panic() {
        let root = temp_root("empty");
        assert!(Playlist::from_dir_scan(&root).is_empty());

        let missing = root.join("no-existe");
        assert!(Playlist::from_dir_scan(&missing).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn selection_clamps_to_range() {
        assert_eq!(move_selection(0, -1, 5), 0);
        assert_eq!(move_selection(4, 1, 5), 4);
        assert_eq!(move_selection(2, -1, 5), 1);
        assert_eq!(move_selection(2, 1, 5), 3);
        assert_eq!(move_selection(0, -1, 0), 0);
        // saltos grandes (PageUp/PageDown) también quedan dentro del rango
        assert_eq!(move_selection(3, -50, 5), 0);
        assert_eq!(move_selection(3, 50, 5), 4);
    }

    #[test]
    fn scroll_keeps_selection_visible_and_in_bounds() {
        // lista más corta que la ventana: siempre arranca en 0
        assert_eq!(scroll_offset(3, 5, 10), 0);

        // selección en el medio: centrada
        assert_eq!(scroll_offset(10, 30, 6), 10 - 3);

        // selección al final: offset máximo para no pasarse
        let offset = scroll_offset(29, 30, 6);
        assert_eq!(offset, 24);
        assert!(
            29 >= offset && 29 < offset + 6,
            "selección visible (offset={offset})"
        );

        // selección al inicio
        assert_eq!(scroll_offset(0, 30, 6), 0);

        // casos borde
        assert_eq!(scroll_offset(0, 0, 6), 0);
        assert_eq!(scroll_offset(5, 30, 0), 0);

        // invariante: la selección siempre cae dentro de la ventana
        for selected in 0..30 {
            let offset = scroll_offset(selected, 30, 6);
            assert!(
                selected >= offset && selected < offset + 6,
                "selected={selected} offset={offset}"
            );
        }
    }
}
