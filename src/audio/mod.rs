#![allow(dead_code)]

//! Motor de audio (stub T1).
//! La lógica real (symphonia + cpal + ringbuf) se implementa en T2.

/// Motor de audio: decodifica con symphonia y reproduce con cpal.
pub struct AudioEngine {
    volume: f32,
}

impl AudioEngine {
    pub fn new() -> Self {
        Self { volume: 1.0 }
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }
}

impl Default for AudioEngine {
    fn default() -> Self {
        Self::new()
    }
}
