//! Lógica de reproducción pura: shuffle, repeat, saltos de pista y filtro de
//! búsqueda. Sin dependencias de UI ni de audio para poder testearla sola.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::playlist::Track;

/// Modo de repetición. El ciclo es Off → One → All → Off.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RepeatMode {
    Off,
    One,
    All,
}

impl RepeatMode {
    /// Avanza un paso dentro del ciclo de repetición.
    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::One,
            Self::One => Self::All,
            Self::All => Self::Off,
        }
    }

    /// Etiqueta para la UI; `None` cuando el modo es el default, para no
    /// ensuciar la pantalla con un "repeat: off" permanente.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::One => Some("uno"),
            Self::All => Some("todo"),
        }
    }
}

/// RNG mínimo (splitmix64) sin crates externos: la semilla mezcla entropía de
/// `RandomState` (claves aleatorias del proceso), tiempo y pid.
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// Semilla fija para tests deterministas.
    #[cfg(test)]
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn from_entropy() -> Self {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.subsec_nanos() as u64)
                .unwrap_or(0),
        );
        hasher.write_u64(u64::from(std::process::id()));
        Self(hasher.finish())
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Fisher-Yates con el RNG dado: permutación donde cada índice 0..len aparece
/// exactamente una vez.
pub fn shuffle_order_with(len: usize, rng: &mut SplitMix64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..len).collect();
    for index in (1..len).rev() {
        let swap = (rng.next_u64() % (index as u64 + 1)) as usize;
        order.swap(index, swap);
    }
    order
}

/// Permutación de shuffle con entropía de sistema (variante de producción).
pub fn shuffle_order(len: usize) -> Vec<usize> {
    shuffle_order_with(len, &mut SplitMix64::from_entropy())
}

/// Pista destino al saltar `step` (+1 siguiente / -1 anterior) dentro de una
/// playlist de `len` posiciones.
///
/// - `current` es la posición actual; `None` (sin pista) va a la primera con
///   el paso hacia adelante y a la última con el hacia atrás.
/// - Solo `RepeatMode::All` envuelve en los extremos; `Off` y `One` devuelven
///   `None` ahí. `One` gobierna únicamente el fin natural de la pista, no el
///   salto manual.
/// - Con `shuffle` la navegación sigue `order` (permutación de 0..len); si
///   `order` no coincide con la playlist (por ejemplo, cambió el filtro), el
///   shuffle se ignora y se navega el orden natural.
pub fn next_track(
    len: usize,
    current: Option<usize>,
    repeat: RepeatMode,
    shuffle: bool,
    order: &[usize],
    step: isize,
) -> Option<usize> {
    if len == 0 {
        return None;
    }

    let wraps = repeat == RepeatMode::All;
    let use_shuffle = shuffle && order.len() == len;

    // Posición dentro del orden de navegación (mezclado o natural).
    let pos = match current {
        None => None,
        Some(cur) if use_shuffle => order.iter().position(|&index| index == cur),
        Some(cur) if cur < len => Some(cur),
        // Fuera de rango: se trata como "sin pista actual".
        Some(_) => None,
    };

    // `resolve` traduce posición de navegación → índice de playlist.
    let resolve = |position: usize| if use_shuffle { order[position] } else { position };

    match pos {
        // Sin pista conocida: la extrema según la dirección del salto.
        None => Some(resolve(if step >= 0 { 0 } else { len - 1 })),
        Some(position) => {
            let target = position as isize + step;
            if target < 0 {
                wraps.then(|| resolve(len - 1))
            } else if target >= len as isize {
                wraps.then(|| resolve(0))
            } else {
                Some(resolve(target as usize))
            }
        }
    }
}

/// Coincidencia case-insensitive por substring sobre título, artista, álbum o
/// nombre de archivo. Un needle vacío (o solo espacios) matchea todo.
pub fn matches_filter(track: &Track, needle: &str) -> bool {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    if track.title.to_lowercase().contains(&needle) {
        return true;
    }
    if track
        .artist
        .as_deref()
        .is_some_and(|value| value.to_lowercase().contains(&needle))
    {
        return true;
    }
    if track
        .album
        .as_deref()
        .is_some_and(|value| value.to_lowercase().contains(&needle))
    {
        return true;
    }
    file_name(&track.path).contains(&needle)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Índices de `tracks` que cumplen `needle`, en orden; `None` = todos.
pub fn filter_tracks(tracks: &[Track], needle: Option<&str>) -> Vec<usize> {
    match needle {
        None => (0..tracks.len()).collect(),
        Some(needle) => tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| matches_filter(track, needle))
            .map(|(index, _)| index)
            .collect(),
    }
}

/// Segundos que avanza o retrocede un seek con ←/→.
pub const SEEK_STEP_SECS: u64 = 5;

/// Destino de un seek relativo: `position + delta * SEEK_STEP_SECS`, con
/// saturación en 0 y tope en `duration` cuando se conoce.
pub fn seek_target(position: Duration, delta: i64, duration: Option<Duration>) -> Duration {
    let step = Duration::from_secs(delta.unsigned_abs().saturating_mul(SEEK_STEP_SECS));
    let target = if delta >= 0 {
        position.checked_add(step).unwrap_or(position)
    } else {
        position.checked_sub(step).unwrap_or(Duration::ZERO)
    };
    match duration {
        Some(max) => target.min(max),
        None => target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn track(path: &str, title: &str, artist: Option<&str>, album: Option<&str>) -> Track {
        Track {
            path: PathBuf::from(path),
            title: title.to_string(),
            artist: artist.map(str::to_string),
            album: album.map(str::to_string),
            duration: None,
        }
    }

    // ---- shuffle ----

    #[test]
    fn shuffle_visits_every_index_exactly_once() {
        for len in [0usize, 1, 2, 3, 10, 100] {
            for seed in [1u64, 42, 987_654_321] {
                let order = shuffle_order_with(len, &mut SplitMix64::new(seed));
                assert_eq!(order.len(), len, "len={len} seed={seed}");

                let mut sorted = order.clone();
                sorted.sort_unstable();
                let expected: Vec<usize> = (0..len).collect();
                assert_eq!(
                    sorted, expected,
                    "permutación inválida len={len} seed={seed}: {order:?}"
                );
            }
        }
    }

    #[test]
    fn shuffle_with_fixed_seed_is_deterministic() {
        let a = shuffle_order_with(50, &mut SplitMix64::new(7));
        let b = shuffle_order_with(50, &mut SplitMix64::new(7));
        assert_eq!(a, b, "misma semilla debe dar el mismo orden");

        let c = shuffle_order_with(50, &mut SplitMix64::new(8));
        assert_ne!(a, c, "seeds distintos deberían mezclar distinto");
    }

    #[test]
    fn shuffle_of_small_lists_is_a_permutation_too() {
        // Extremo donde Fisher-Yates no tiene dónde moverse.
        assert_eq!(shuffle_order_with(0, &mut SplitMix64::new(1)), Vec::<usize>::new());
        assert_eq!(shuffle_order_with(1, &mut SplitMix64::new(1)), vec![0]);
    }

    // ---- repeat ----

    #[test]
    fn repeat_cycles_off_one_all_and_back() {
        let mut mode = RepeatMode::Off;
        assert_eq!(mode.label(), None);
        mode = mode.cycle();
        assert_eq!(mode, RepeatMode::One);
        assert_eq!(mode.label(), Some("uno"));
        mode = mode.cycle();
        assert_eq!(mode, RepeatMode::All);
        assert_eq!(mode.label(), Some("todo"));
        mode = mode.cycle();
        assert_eq!(mode, RepeatMode::Off, "el ciclo cierra en Off");
    }

    // ---- next/previous ----

    #[test]
    fn next_and_previous_without_repeat_do_not_wrap() {
        let order: [usize; 0] = [];
        // Off y One comparten navegación en saltos manuales.
        for repeat in [RepeatMode::Off, RepeatMode::One] {
            assert_eq!(
                next_track(3, Some(0), repeat, false, &order, -1),
                None,
                "prev en la primera no envuelve con {repeat:?}"
            );
            assert_eq!(
                next_track(3, Some(2), repeat, false, &order, 1),
                None,
                "next en la última no envuelve con {repeat:?}"
            );
            assert_eq!(next_track(3, Some(0), repeat, false, &order, 1), Some(1));
            assert_eq!(next_track(3, Some(2), repeat, false, &order, -1), Some(1));
        }
    }

    #[test]
    fn next_and_previous_wrap_with_repeat_all() {
        let order: [usize; 0] = [];
        assert_eq!(
            next_track(3, Some(2), RepeatMode::All, false, &order, 1),
            Some(0),
            "All envuelve de la última a la primera"
        );
        assert_eq!(
            next_track(3, Some(0), RepeatMode::All, false, &order, -1),
            Some(2),
            "All envuelve de la primera a la última"
        );
    }

    #[test]
    fn without_current_track_next_goes_first_and_prev_goes_last() {
        let order: [usize; 0] = [];
        assert_eq!(next_track(4, None, RepeatMode::Off, false, &order, 1), Some(0));
        assert_eq!(next_track(4, None, RepeatMode::Off, false, &order, -1), Some(3));
    }

    #[test]
    fn empty_playlist_has_no_next_or_previous() {
        assert_eq!(
            next_track(0, None, RepeatMode::All, false, &[], 1),
            None
        );
        assert_eq!(next_track(0, Some(0), RepeatMode::Off, false, &[], -1), None);
    }

    #[test]
    fn shuffle_follows_the_mixed_order() {
        // Orden mezclado fijo: la playlist [A,B,C,D] se navega 2 → 0 → 3 → 1.
        let order = [2usize, 0, 3, 1];
        let walk_next: Vec<usize> = {
            let mut path = Vec::new();
            let mut current = Some(2);
            while let Some(step) = next_track(4, current, RepeatMode::Off, true, &order, 1) {
                path.push(step);
                current = Some(step);
            }
            path
        };
        assert_eq!(walk_next, vec![0, 3, 1], "next recorre el orden mezclado");
        assert_eq!(
            next_track(4, Some(1), RepeatMode::Off, true, &order, 1),
            None,
            "Off no envuelve ni en el orden mezclado"
        );
        assert_eq!(
            next_track(4, Some(2), RepeatMode::All, true, &order, -1),
            Some(1),
            "All envuelve atrás dentro del orden mezclado"
        );
        assert_eq!(
            next_track(4, Some(1), RepeatMode::All, true, &order, 1),
            Some(2),
            "All envuelve adelante dentro del orden mezclado"
        );
    }

    #[test]
    fn stale_shuffle_order_falls_back_to_natural_order() {
        // El filtro cambió la playlist y el orden quedó con 2 entradas.
        let stale = [1usize, 0];
        assert_eq!(
            next_track(4, Some(1), RepeatMode::Off, true, &stale, 1),
            Some(2),
            "order desactualizado se ignora y navega el orden natural"
        );
    }

    #[test]
    fn out_of_range_current_is_treated_as_no_track() {
        assert_eq!(
            next_track(3, Some(99), RepeatMode::Off, false, &[], 1),
            Some(0)
        );
    }

    // ---- filtro de búsqueda ----

    fn sample_tracks() -> Vec<Track> {
        vec![
            track(
                r"C:\m\01-intro.mp3",
                "Intro",
                Some("Aphex Twin"),
                Some("Selected Ambient Works"),
            ),
            track(
                r"C:\m\02-song.mp3",
                "Back to the old house",
                Some("The Smiths"),
                Some("Hatful of Hollow"),
            ),
            track(
                r"C:\m\03-blank.mp3",
                "Untitled",
                None,
                None,
            ),
        ]
    }

    #[test]
    fn filter_matches_title_case_insensitively() {
        let tracks = sample_tracks();
        assert_eq!(filter_tracks(&tracks, Some("back TO")), vec![1]);
        assert_eq!(filter_tracks(&tracks, Some("INTRO")), vec![0]);
    }

    #[test]
    fn filter_matches_artist_album_and_filename() {
        let tracks = sample_tracks();
        assert_eq!(filter_tracks(&tracks, Some("smiths")), vec![1], "artista");
        assert_eq!(
            filter_tracks(&tracks, Some("ambient works")),
            vec![0],
            "album"
        );
        assert_eq!(filter_tracks(&tracks, Some("03-")), vec![2], "nombre de archivo");
    }

    #[test]
    fn filter_with_no_matches_is_empty_and_none_returns_all() {
        let tracks = sample_tracks();
        assert!(filter_tracks(&tracks, Some("zzz-no-existe")).is_empty());
        assert_eq!(filter_tracks(&tracks, None), vec![0, 1, 2]);
        assert_eq!(
            filter_tracks(&tracks, Some("   ")),
            vec![0, 1, 2],
            "solo espacios = sin filtro"
        );
    }

    #[test]
    fn matches_filter_requires_substring_not_fuzzy() {
        let tracks = sample_tracks();
        // Substring literal: "smt" no matchea "smiths" (sin fuzzy matching).
        assert!(!matches_filter(&tracks[1], "smt"));
        assert!(matches_filter(&tracks[1], "the sm"));
    }

    #[test]
    fn seek_target_steps_forward_and_backward() {
        let pos = Duration::from_secs(10);
        let some = Some(Duration::from_secs(60));
        assert_eq!(seek_target(pos, 1, some), Duration::from_secs(15));
        assert_eq!(seek_target(pos, -1, some), Duration::from_secs(5));
        assert_eq!(seek_target(pos, 3, some), Duration::from_secs(25));
    }

    #[test]
    fn seek_target_saturates_at_zero() {
        let pos = Duration::from_secs(2);
        assert_eq!(
            seek_target(pos, -1, None),
            Duration::ZERO,
            "retroceso más allá del inicio queda en 0"
        );
        assert_eq!(seek_target(Duration::ZERO, -5, None), Duration::ZERO);
    }

    #[test]
    fn seek_target_clamps_to_duration() {
        let pos = Duration::from_secs(10);
        let duration = Some(Duration::from_secs(12));
        assert_eq!(
            seek_target(pos, 1, duration),
            Duration::from_secs(12),
            "adelanto tope en la duración"
        );
        assert_eq!(
            seek_target(Duration::from_secs(11), 1, duration),
            Duration::from_secs(12)
        );
    }

    #[test]
    fn seek_target_without_duration_does_not_clamp() {
        let pos = Duration::from_secs(10);
        assert_eq!(seek_target(pos, 1, None), Duration::from_secs(15));
    }
}
