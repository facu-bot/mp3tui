//! Integración con Spotify: autenticación (PKCE) y, más adelante, cliente de
//! metadatos de la Web API.

// Los consumidores (cliente de metadatos, CLI de login) llegan en tareas
// posteriores; el `allow` desaparece cuando `auth` quede cableado.
#![allow(dead_code)]

pub mod auth;
