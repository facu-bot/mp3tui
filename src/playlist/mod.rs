#![allow(dead_code)]

//! Playlist y biblioteca (stub T1).
//! El escaneo de carpetas + metadata con lofty se implementa en T5.

use std::path::PathBuf;

/// Una pista de audio de la biblioteca.
pub struct Track {
    pub path: PathBuf,
    pub title: Option<String>,
}

/// Playlist / biblioteca de pistas.
pub struct Playlist {
    tracks: Vec<Track>,
    current: Option<usize>,
}

impl Playlist {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            current: None,
        }
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn current(&self) -> Option<usize> {
        self.current
    }
}

impl Default for Playlist {
    fn default() -> Self {
        Self::new()
    }
}
