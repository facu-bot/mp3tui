//! Integración con Spotify: autenticación (PKCE) y cliente de metadatos de la
//! Web API.

// Los consumidores de producción (reproducción en cola, CLI de login) llegan en
// tareas posteriores (T4/T5/T7). Hasta que estén cableados, `dead_code` evita
// ruido sobre una API pública ya cubierta por tests; el `allow` desaparece
// cuando el cliente de metadatos tenga un consumidor en producción.
#![allow(dead_code)]

pub mod auth;
pub mod client;
