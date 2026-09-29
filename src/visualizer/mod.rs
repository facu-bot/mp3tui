#![allow(dead_code)]

//! Analizador de espectro FFT (stub T1).
//! El cálculo con rustfft sobre el ring buffer se implementa en T4.

/// Cantidad de barras del visualizador estilo cava.
pub const NUM_BANDS: usize = 32;

/// Analizador de espectro: convierte muestras PCM en magnitudes por banda.
pub struct SpectrumAnalyzer {
    bands: [f32; NUM_BANDS],
}

impl SpectrumAnalyzer {
    pub fn new() -> Self {
        Self {
            bands: [0.0; NUM_BANDS],
        }
    }

    pub fn bands(&self) -> &[f32; NUM_BANDS] {
        &self.bands
    }
}

impl Default for SpectrumAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}
