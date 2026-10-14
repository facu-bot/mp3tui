mod audio;
mod config;
pub mod history;
mod playback;
mod playlist;
mod runtime;
mod source;
mod ui;
mod visualizer;

use clap::Parser;
use std::process::ExitCode;

/// Reproductor MP3 visual para terminal.
#[derive(Parser, Debug)]
#[command(name = "mp3tui", about = "Reproductor MP3 visual para terminal")]
struct Cli {
    /// Archivo de audio, carpeta con la biblioteca o URL (HTTP, YouTube o
    /// Spotify) a reproducir.
    #[arg(value_name = "TARGET")]
    target: Option<String>,

    /// Autoriza mp3tui en Spotify (PKCE) y sale; no abre la TUI.
    #[arg(
        long,
        long_help = "Autoriza mp3tui en Spotify con Authorization Code + PKCE y sale sin abrir la \
                     TUI.\n\n\
                     Necesitás el client_id de una app tuya: registrala en \
                     https://developer.spotify.com/dashboard con un redirect URI loopback \
                     (http://127.0.0.1:<puerto>/callback) y pasá el Client ID en la variable \
                     MP3TUI_SPOTIFY_CLIENT_ID o en config.toml: [spotify] client_id = \"...\". \
                     La sesión dura 6 meses; después hay que volver a autorizar. No hace falta \
                     Client Secret (PKCE) ni Spotify Premium: la Web API solo aporta metadatos \
                     y el audio lo resuelve yt-dlp en YouTube."
    )]
    spotify_login: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Config: se carga una vez acá para que los avisos salgan a stderr
    // antes de que la TUI tome la pantalla; cada campo inválido cayó a su
    // default en config::parse.
    for warning in &config::init().warnings {
        eprintln!("mp3tui: config: {warning}");
    }

    if cli.spotify_login {
        return match source::spotify::play::run_login(
            source::spotify::client_id(config::config()),
            source::spotify::auth::tokens_path(),
            |client, store| {
                source::spotify::auth::login(client, store, std::time::Duration::from_secs(300))
            },
        ) {
            Ok(()) => {
                println!("Listo: sesión de Spotify guardada.");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("mp3tui: {err}");
                ExitCode::FAILURE
            }
        };
    }

    // Sin target: biblioteca por defecto (carpetas conocidas del sistema
    // que existan, unidas a las [library] roots configuradas).
    // Con URL: reproducción remota (directa o via yt-dlp) en background.
    // Con carpeta: explorador solo en esa carpeta (la CLI tiene prioridad).
    // Con archivo: comportamiento original (reproducción directa a la TUI,
    // con la biblioteca de la carpeta contenedora a mano en modo explorador).
    let start = match cli.target.as_deref() {
        None => ui::StartMode::Browse(config::config().library.scan_roots()),
        Some(target) => match source::classify(target) {
            source::Input::Url { url, kind } => ui::StartMode::PlayUrl {
                url,
                kind,
                roots: config::config().library.scan_roots(),
            },
            source::Input::Local(path) => {
                if path.is_dir() {
                    ui::StartMode::Browse(vec![path])
                } else if path.is_file() {
                    ui::StartMode::PlayFile(path)
                } else {
                    eprintln!("mp3tui: '{}' no existe.", path.display());
                    return ExitCode::FAILURE;
                }
            }
        },
    };

    let mut engine = match audio::AudioEngine::new() {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("mp3tui: no se pudo inicializar el device de audio: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let ui::StartMode::PlayFile(path) = &start
        && let Err(err) = engine.play(path)
    {
        eprintln!("mp3tui: no se pudo reproducir '{}': {err}", path.display());
        return ExitCode::FAILURE;
    }

    match ui::run(&mut engine, start) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("mp3tui: error de la TUI: {err}");
            ExitCode::FAILURE
        }
    }
}
