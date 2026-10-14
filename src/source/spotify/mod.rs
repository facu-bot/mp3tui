//! Integración con Spotify: autenticación (PKCE) y cliente de metadatos de la
//! Web API.

// Los consumidores de producción (reproducción en cola, CLI de login) llegan en
// tareas posteriores (T4/T5/T7). Hasta que estén cableados, `dead_code` evita
// ruido sobre una API pública ya cubierta por tests; el `allow` desaparece
// cuando el cliente de metadatos tenga un consumidor en producción.
#![allow(dead_code)]

pub mod auth;
pub mod client;
pub mod play;
pub mod resolve;

use crate::config::Config;

/// Variable de entorno que fija el `client_id` de la app de Spotify.
pub const CLIENT_ID_ENV: &str = "MP3TUI_SPOTIFY_CLIENT_ID";

/// Elige el `client_id`: la variable de entorno gana sobre el config. Los
/// valores vacíos o en blanco cuentan como ausentes.
pub fn client_id_from(env: Option<String>, config: Option<String>) -> Option<String> {
    [env, config]
        .into_iter()
        .flatten()
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

/// No hay `client_id` configurado en ningún lado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingClientId;

impl std::fmt::Display for MissingClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "falta el client_id de Spotify. Registrá una app en \
             https://developer.spotify.com/dashboard (redirect URI \
             http://127.0.0.1:<puerto>) y copiá su Client ID en la variable \
             {CLIENT_ID_ENV} o en config.toml: [spotify] client_id = \"...\""
        )
    }
}

impl std::error::Error for MissingClientId {}

/// `client_id` efectivo: `MP3TUI_SPOTIFY_CLIENT_ID` y, si no, `[spotify] client_id`.
pub fn client_id(config: &Config) -> Result<String, MissingClientId> {
    client_id_from(std::env::var(CLIENT_ID_ENV).ok(), config.spotify.client_id.clone())
        .ok_or(MissingClientId)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(text: &str) -> Option<String> {
        Some(text.to_string())
    }

    #[test]
    fn client_id_env_wins_over_config() {
        assert_eq!(client_id_from(some("env-id"), some("cfg-id")), some("env-id"));
    }

    #[test]
    fn client_id_falls_back_to_config() {
        assert_eq!(client_id_from(None, some("cfg-id")), some("cfg-id"));
        assert_eq!(client_id_from(some("   "), some("cfg-id")), some("cfg-id"));
    }

    #[test]
    fn client_id_is_trimmed_and_blank_means_missing() {
        assert_eq!(client_id_from(some("  abc \n"), None), some("abc"));
        assert_eq!(client_id_from(None, None), None);
        assert_eq!(client_id_from(some(""), some("  ")), None);
    }

    #[test]
    fn missing_client_id_error_explains_how_to_fix_it() {
        let text = MissingClientId.to_string();
        assert!(text.contains(CLIENT_ID_ENV), "{text}");
        assert!(text.contains("[spotify]"), "{text}");
        assert!(text.contains("developer.spotify.com"), "{text}");
    }
}
