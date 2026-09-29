mod audio;
mod playlist;
mod ui;
mod visualizer;

use clap::Parser;
use std::path::PathBuf;

/// Reproductor de música para terminal (stub T1 — scaffold que compila).
#[derive(Parser, Debug)]
#[command(name = "mp3tui", about = "Reproductor MP3 visual para terminal")]
struct Cli {
    /// Archivo de audio o carpeta con la biblioteca a reproducir.
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();

    match &cli.path {
        Some(path) => println!("mp3tui: path recibido: {}", path.display()),
        None => println!("mp3tui: sin path, solo stub T1 (scaffold)."),
    }

    // Referencias a los stubs para verificar que los módulos enlazan.
    let _engine = audio::AudioEngine::new();
    let _playlist = playlist::Playlist::new();
    let _analyzer = visualizer::SpectrumAnalyzer::new();
    let _app = ui::App::new();

    println!("mp3tui scaffold OK (T1). Lógica real en T2+.");
}
