//! Clasificación de fuentes de entrada: archivos locales vs URLs remotas
//! y detección por URL o cabecera Content-Type.

pub mod http_stream;
pub mod spotify;
pub mod ytdlp;

use std::path::PathBuf;

/// Tipo de entrada clasificada a partir de un string de entrada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Archivo o ruta local en el sistema de archivos.
    Local(PathBuf),
    /// URL remota http/https con clasificación preliminar.
    Url { url: String, kind: UrlKind },
}

/// Clasificación preliminar de una URL según su extensión o host conocido.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlKind {
    /// Archivo de audio directo soportado (mp3, flac, ogg, wav, m4a, etc.).
    DirectAudio,
    /// Página web de servicio conocido (YouTube, SoundCloud, Bandcamp, Vimeo).
    Page,
    /// Playlist de YouTube: tiene `list=` en la query o su path es `/playlist`.
    /// Se lista con yt-dlp `--flat-playlist` (títulos + URLs de página, sin
    /// resolver audio), no como una página única.
    Playlist,
    /// Canción de Spotify (`open.spotify.com/track/<id>` o `spotify:track:<id>`).
    /// No tiene audio propio: se resuelve a YouTube por búsqueda.
    SpotifyTrack,
    /// Playlist de Spotify (`/playlist/<id>`, `spotify:playlist:<id>`).
    SpotifyPlaylist,
    /// Álbum de Spotify: se encola como una playlist pero se lista con otro
    /// endpoint (`/v1/albums/{id}/tracks`).
    SpotifyAlbum,
    /// URL sin extensión ni host reconocido; requiere inspeccionar Content-Type.
    Unknown,
}

/// Clasificación del header HTTP `Content-Type`.
///
/// Todavía sin consumidor en producción: el camino que lo usaría
/// ([`UrlKind::Unknown`]) sigue sin inspeccionar el header. Se conserva con
/// `allow` puntual para no perder un mapeo ya cubierto por tests; el `allow`
/// desaparece el día que ese camino se cablee.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// Flujo de audio directo.
    Audio,
    /// Página HTML.
    Html,
    /// Otro tipo de contenido (video, binario genérico, etc.).
    Other,
}

/// Compara si `s` comienza con `prefix` ignorando mayúsculas/minúsculas ASCII, sin panickear en límites UTF-8.
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    let s_bytes = s.as_bytes();
    let prefix_bytes = prefix.as_bytes();
    if s_bytes.len() < prefix_bytes.len() {
        return false;
    }
    s_bytes[..prefix_bytes.len()].eq_ignore_ascii_case(prefix_bytes)
}

/// Helper que determina si un host corresponde a una página web de un servicio conocido.
fn is_page_host(host: &str) -> bool {
    let stripped = host.strip_prefix("www.").unwrap_or(host);
    stripped == "youtube.com"
        || stripped == "m.youtube.com"
        || stripped == "music.youtube.com"
        || stripped == "youtu.be"
        || stripped == "soundcloud.com"
        || stripped == "m.soundcloud.com"
        || stripped == "vimeo.com"
        || stripped == "bandcamp.com"
        || stripped.ends_with(".bandcamp.com")
}

/// `true` si la query trae un parámetro `list` (playlist de YouTube u otro
/// servicio que use `?list=`). El nombre del parámetro no distingue mayúsculas.
fn has_list_query_param(query: &str) -> bool {
    query.split('&').any(
        |pair| matches!(pair.split_once('='), Some((key, _)) if key.eq_ignore_ascii_case("list")),
    )
}

/// Clasifica una entrada de usuario como ruta local o URL remota.
pub fn classify(input: &str) -> Input {
    let trimmed = input.trim();
    // Spotify va antes que el resto: `/playlist/<id>` en open.spotify.com
    // no es una playlist de YouTube.
    if let Some(reference) = spotify_ref(trimmed) {
        return Input::Url {
            url: trimmed.to_string(),
            kind: reference.kind,
        };
    }
    let scheme_len = if starts_with_ignore_ascii_case(trimmed, "http://") {
        7
    } else if starts_with_ignore_ascii_case(trimmed, "https://") {
        8
    } else {
        return Input::Local(PathBuf::from(trimmed));
    };

    let rest = &trimmed[scheme_len..];
    let (authority, path_and_rest) = match rest.find(['/', '?', '#']) {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };

    let path = match path_and_rest.find(['?', '#']) {
        Some(idx) => &path_and_rest[..idx],
        None => path_and_rest,
    };

    // Query (sin el `#fragment`), para detectar `?list=` de playlist.
    let query = match path_and_rest.find('?') {
        Some(idx) => {
            let after = &path_and_rest[idx + 1..];
            match after.find('#') {
                Some(hash) => &after[..hash],
                None => after,
            }
        }
        None => "",
    };

    let last_segment = path.rsplit('/').next().unwrap_or("");
    let ext_opt = last_segment.rsplit_once('.').and_then(|(stem, ext)| {
        if !stem.is_empty() && !ext.is_empty() {
            Some(ext.to_ascii_lowercase())
        } else {
            None
        }
    });

    if let Some(ref ext) = ext_opt {
        match ext.as_str() {
            "mp3" | "flac" | "ogg" | "oga" | "wav" | "m4a" | "aac" | "opus" => {
                return Input::Url {
                    url: trimmed.to_string(),
                    kind: UrlKind::DirectAudio,
                };
            }
            _ => {}
        }
    }

    // Playlist: `list=` en la query o path `/playlist` (gana sobre el host
    // de página para que un watch con lista no se trate como video suelto).
    // `/playlists` (la página de playlists del usuario) NO es una lista.
    let path_lower = path.to_ascii_lowercase();
    if has_list_query_param(query)
        || path_lower == "/playlist"
        || path_lower.starts_with("/playlist/")
    {
        return Input::Url {
            url: trimmed.to_string(),
            kind: UrlKind::Playlist,
        };
    }

    let host_and_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = if host_and_port.starts_with('[') {
        if let Some(close_bracket) = host_and_port.find(']') {
            &host_and_port[1..close_bracket]
        } else {
            host_and_port
        }
    } else {
        host_and_port.split(':').next().unwrap_or(host_and_port)
    };

    let host_lower = host.to_ascii_lowercase();
    if is_page_host(&host_lower) {
        return Input::Url {
            url: trimmed.to_string(),
            kind: UrlKind::Page,
        };
    }

    Input::Url {
        url: trimmed.to_string(),
        kind: UrlKind::Unknown,
    }
}

/// Referencia a un recurso de Spotify extraída de una URL web o un URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyRef {
    /// [`UrlKind::SpotifyTrack`], [`UrlKind::SpotifyPlaylist`] o
    /// [`UrlKind::SpotifyAlbum`].
    pub kind: UrlKind,
    /// Id base62 del recurso; solo alfanumérico, así que es seguro como
    /// segmento de path de la Web API.
    pub id: String,
}

/// Reconoce `https://open.spotify.com/{track,playlist,album}/<id>` (con o sin
/// query, fragmento, barra final o prefijo de locale `/intl-xx/`) y los URIs
/// `spotify:{track,playlist,album}:<id>`. Cualquier otra cosa devuelve `None`.
pub fn spotify_ref(input: &str) -> Option<SpotifyRef> {
    let input = input.trim();
    if starts_with_ignore_ascii_case(input, "spotify:") {
        let mut parts = input["spotify:".len()..].split(':');
        let (resource, id) = (parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        return build_spotify_ref(resource, id);
    }

    let scheme_len = if starts_with_ignore_ascii_case(input, "https://") {
        8
    } else if starts_with_ignore_ascii_case(input, "http://") {
        7
    } else {
        return None;
    };
    let rest = &input[scheme_len..];
    let (host, path) = rest.split_once('/')?;
    if !host.eq_ignore_ascii_case("open.spotify.com") {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let path = path.strip_suffix('/').unwrap_or(path);
    let mut segments = path.split('/').peekable();
    if segments.peek().is_some_and(|s| is_intl_locale(s)) {
        segments.next();
    }
    let (resource, id) = (segments.next()?, segments.next()?);
    if segments.next().is_some() {
        return None;
    }
    build_spotify_ref(resource, id)
}

fn build_spotify_ref(resource: &str, id: &str) -> Option<SpotifyRef> {
    let kind = match resource.to_ascii_lowercase().as_str() {
        "track" => UrlKind::SpotifyTrack,
        "playlist" => UrlKind::SpotifyPlaylist,
        "album" => UrlKind::SpotifyAlbum,
        _ => return None,
    };
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(SpotifyRef {
        kind,
        id: id.to_string(),
    })
}

/// Prefijo de locale de open.spotify.com: `intl-es`, `intl-pt-br`, etc.
fn is_intl_locale(segment: &str) -> bool {
    segment
        .strip_prefix("intl-")
        .is_some_and(|tag| !tag.is_empty() && tag.chars().all(|c| c.is_ascii_alphabetic() || c == '-'))
}

/// Clasifica una cabecera HTTP `Content-Type`. Sin consumidor en producción
/// todavía: ver [`ContentKind`].
#[allow(dead_code)]
pub fn classify_content_type(header: &str) -> ContentKind {
    let mime = header
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    match mime.as_str() {
        "audio/x-mpegurl" | "audio/mpegurl" | "audio/x-scpls" => ContentKind::Other,
        "application/ogg" => ContentKind::Audio,
        "text/html" | "application/xhtml+xml" => ContentKind::Html,
        _ if mime.starts_with("audio/") => ContentKind::Audio,
        _ => ContentKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(input: &str) -> Input {
        classify(input)
    }

    fn spotify(input: &str, kind: UrlKind) -> Input {
        Input::Url {
            url: input.to_string(),
            kind,
        }
    }

    #[test]
    fn test_classify_spotify_web_urls() {
        let id = "4uLU6hMCjMI75M1A2tKUQC";
        for (input, kind) in [
            (format!("https://open.spotify.com/track/{id}"), UrlKind::SpotifyTrack),
            (format!("https://open.spotify.com/track/{id}?si=abc123"), UrlKind::SpotifyTrack),
            (format!("https://open.spotify.com/track/{id}/"), UrlKind::SpotifyTrack),
            (format!("https://open.spotify.com/intl-es/track/{id}"), UrlKind::SpotifyTrack),
            (format!("https://open.spotify.com/intl-pt-br/track/{id}?si=x"), UrlKind::SpotifyTrack),
            (format!("http://open.spotify.com/track/{id}"), UrlKind::SpotifyTrack),
            (format!("HTTPS://OPEN.SPOTIFY.COM/track/{id}"), UrlKind::SpotifyTrack),
            (format!("https://open.spotify.com/playlist/{id}"), UrlKind::SpotifyPlaylist),
            (format!("https://open.spotify.com/playlist/{id}?si=zz"), UrlKind::SpotifyPlaylist),
            (format!("https://open.spotify.com/intl-fr/playlist/{id}"), UrlKind::SpotifyPlaylist),
            (format!("https://open.spotify.com/album/{id}"), UrlKind::SpotifyAlbum),
            (format!("https://open.spotify.com/intl-de/album/{id}?si=q#frag"), UrlKind::SpotifyAlbum),
        ] {
            assert_eq!(kind_of(&input), spotify(&input, kind), "{input}");
        }
    }

    #[test]
    fn test_classify_spotify_uris() {
        let id = "4uLU6hMCjMI75M1A2tKUQC";
        for (input, kind) in [
            (format!("spotify:track:{id}"), UrlKind::SpotifyTrack),
            (format!("spotify:playlist:{id}"), UrlKind::SpotifyPlaylist),
            (format!("spotify:album:{id}"), UrlKind::SpotifyAlbum),
        ] {
            assert_eq!(kind_of(&input), spotify(&input, kind), "{input}");
        }
        assert_eq!(
            kind_of(&format!("  spotify:track:{id}  ")),
            spotify(&format!("spotify:track:{id}"), UrlKind::SpotifyTrack)
        );
    }

    #[test]
    fn test_spotify_ref_extracts_kind_and_id() {
        assert_eq!(
            spotify_ref("https://open.spotify.com/intl-es/album/AbC123?si=x"),
            Some(SpotifyRef { kind: UrlKind::SpotifyAlbum, id: "AbC123".to_string() })
        );
        assert_eq!(
            spotify_ref("spotify:track:XyZ9"),
            Some(SpotifyRef { kind: UrlKind::SpotifyTrack, id: "XyZ9".to_string() })
        );
        assert_eq!(spotify_ref("https://youtube.com/watch?v=1"), None);
    }

    #[test]
    fn test_classify_spotify_negative_cases() {
        for input in [
            "https://open.spotify.com/artist/4uLU6hMCjMI75M1A2tKUQC",
            "https://open.spotify.com/track/",
            "https://open.spotify.com/track",
            "https://open.spotify.com/",
            "https://open.spotify.com/track/abc/extra",
            "https://open.spotify.com/track/ab$c",
            "https://open.spotify.com/intl-es/",
            "https://open.spotify.com.evil.com/track/abc",
            "https://evil.com/open.spotify.com/track/abc",
            "https://example.com/track/abc",
            "spotify:artist:abc",
            "spotify:track:",
            "spotify:track:a:b",
            "spotify:track:ab/../c",
            "spotify:user:me",
            "spotify:track",
        ] {
            let got = kind_of(input);
            assert!(
                !matches!(
                    got,
                    Input::Url { kind: UrlKind::SpotifyTrack
                            | UrlKind::SpotifyPlaylist
                            | UrlKind::SpotifyAlbum,
                        .. }
                ),
                "{input} -> {got:?}"
            );
        }
        // Un `spotify:` no reconocido sigue siendo una ruta local, como antes.
        assert_eq!(
            kind_of("spotify:artist:abc"),
            Input::Local(PathBuf::from("spotify:artist:abc"))
        );
    }

    #[test]
    fn test_classify_youtube_playlist_still_works_next_to_spotify() {
        assert_eq!(
            classify("https://www.youtube.com/playlist?list=PL123"),
            Input::Url {
                url: "https://www.youtube.com/playlist?list=PL123".to_string(),
                kind: UrlKind::Playlist,
            }
        );
        assert_eq!(
            classify("https://www.youtube.com/playlist/abc"),
            Input::Url {
                url: "https://www.youtube.com/playlist/abc".to_string(),
                kind: UrlKind::Playlist,
            }
        );
    }

    #[test]
    fn test_classify_empty_string_is_local() {
        assert_eq!(classify(""), Input::Local(PathBuf::from("")));
        assert_eq!(classify("   "), Input::Local(PathBuf::from("")));
    }

    #[test]
    fn test_classify_windows_and_relative_paths() {
        assert_eq!(
            classify(r"C:\Music\a.mp3"),
            Input::Local(PathBuf::from(r"C:\Music\a.mp3"))
        );
        assert_eq!(
            classify("musica/tema.flac"),
            Input::Local(PathBuf::from("musica/tema.flac"))
        );
        assert_eq!(
            classify("  ./audio.ogg  "),
            Input::Local(PathBuf::from("./audio.ogg"))
        );
    }

    #[test]
    fn test_classify_non_ascii_unicode_paths_do_not_panic() {
        let non_ascii_paths = [
            "ñandú.mp3",
            "ñññññ",
            r"Música\tema.mp3",
            "canción.mp3",
            "áéíóú.ogg",
        ];
        for path in non_ascii_paths {
            assert_eq!(classify(path), Input::Local(PathBuf::from(path)));
        }
    }

    #[test]
    fn test_classify_direct_audio_extensions() {
        let exts = ["mp3", "flac", "ogg", "oga", "wav", "m4a", "aac", "opus"];
        for ext in exts {
            let url = format!("http://example.com/stream.{}", ext);
            assert_eq!(
                classify(&url),
                Input::Url {
                    url: url.clone(),
                    kind: UrlKind::DirectAudio,
                }
            );
        }
    }

    #[test]
    fn test_classify_uppercase_scheme_and_extension() {
        assert_eq!(
            classify("HTTP://EXAMPLE.COM/AUDIO.MP3"),
            Input::Url {
                url: "HTTP://EXAMPLE.COM/AUDIO.MP3".to_string(),
                kind: UrlKind::DirectAudio,
            }
        );
        assert_eq!(
            classify("HTTPS://YOUTUBE.COM/WATCH?v=123"),
            Input::Url {
                url: "HTTPS://YOUTUBE.COM/WATCH?v=123".to_string(),
                kind: UrlKind::Page,
            }
        );
    }

    #[test]
    fn test_classify_m3u_pls_and_m3u8_are_unknown() {
        assert_eq!(
            classify("http://radio.example.org/listen.m3u"),
            Input::Url {
                url: "http://radio.example.org/listen.m3u".to_string(),
                kind: UrlKind::Unknown,
            }
        );
        assert_eq!(
            classify("http://radio.example.org/listen.pls"),
            Input::Url {
                url: "http://radio.example.org/listen.pls".to_string(),
                kind: UrlKind::Unknown,
            }
        );
        assert_eq!(
            classify("https://live.example.com/stream.m3u8"),
            Input::Url {
                url: "https://live.example.com/stream.m3u8".to_string(),
                kind: UrlKind::Unknown,
            }
        );
    }

    #[test]
    fn test_classify_query_with_fake_extension_is_unknown() {
        assert_eq!(
            classify("https://x.com/a?f=b.mp3"),
            Input::Url {
                url: "https://x.com/a?f=b.mp3".to_string(),
                kind: UrlKind::Unknown,
            }
        );
    }

    #[test]
    fn test_classify_fragment_is_ignored() {
        assert_eq!(
            classify("https://example.com/audio.mp3#t=10"),
            Input::Url {
                url: "https://example.com/audio.mp3#t=10".to_string(),
                kind: UrlKind::DirectAudio,
            }
        );
    }

    #[test]
    fn test_classify_trailing_slash() {
        assert_eq!(
            classify("https://youtube.com/"),
            Input::Url {
                url: "https://youtube.com/".to_string(),
                kind: UrlKind::Page,
            }
        );
        assert_eq!(
            classify("https://example.com/dir/"),
            Input::Url {
                url: "https://example.com/dir/".to_string(),
                kind: UrlKind::Unknown,
            }
        );
    }

    #[test]
    fn test_classify_host_with_port() {
        assert_eq!(
            classify("http://example.com:8080/song.mp3"),
            Input::Url {
                url: "http://example.com:8080/song.mp3".to_string(),
                kind: UrlKind::DirectAudio,
            }
        );
        assert_eq!(
            classify("https://soundcloud.com:443/track"),
            Input::Url {
                url: "https://soundcloud.com:443/track".to_string(),
                kind: UrlKind::Page,
            }
        );
    }

    #[test]
    fn test_classify_playlist_urls() {
        // Playlist por query `list=` (watch con lista o /playlist con lista).
        let by_query = [
            "https://www.youtube.com/watch?v=abc123&list=PLxyz",
            "https://www.youtube.com/playlist?list=PLxyz",
            "https://music.youtube.com/playlist?list=PLxyz&feature=share",
            "https://www.youtube.com/embed/videoseries?list=PLxyz",
            "https://example.com/watch?foo=bar&list=PLxyz",
        ];
        for url in by_query {
            assert_eq!(
                classify(url),
                Input::Url {
                    url: url.to_string(),
                    kind: UrlKind::Playlist,
                },
                "Fallo clasificando playlist por query: {url}"
            );
        }

        // Playlist por path `/playlist`.
        let by_path = [
            "https://www.youtube.com/playlist",
            "https://www.youtube.com/playlist/",
            "https://music.youtube.com/playlist?foo=bar",
        ];
        for url in by_path {
            assert_eq!(
                classify(url),
                Input::Url {
                    url: url.to_string(),
                    kind: UrlKind::Playlist,
                },
                "Fallo clasificando playlist por path: {url}"
            );
        }

        // Un watch normal sin `list=` sigue siendo Page.
        assert_eq!(
            classify("https://www.youtube.com/watch?v=abc123"),
            Input::Url {
                url: "https://www.youtube.com/watch?v=abc123".to_string(),
                kind: UrlKind::Page,
            }
        );
    }

    /// El path `/playlists` (la página de playlists del usuario) no es una
    /// URL de playlist: solo `/playlist` o `/playlist/`.
    #[test]
    fn test_classify_playlists_page_is_not_a_playlist() {
        assert_eq!(
            classify("https://www.youtube.com/playlists"),
            Input::Url {
                url: "https://www.youtube.com/playlists".to_string(),
                kind: UrlKind::Page,
            },
            "la página de playlists del usuario no se lista como playlist"
        );
    }

    #[test]
    fn test_classify_known_page_hosts() {
        let page_urls = [
            "https://youtu.be/dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=abc123xyz",
            "https://m.youtube.com/watch?v=abc123xyz",
            "https://www.youtube.com/watch?v=abc123xyz",
            "https://soundcloud.com/artist/track-name",
            "https://vimeo.com/12345678",
            "https://artist.bandcamp.com/track/cool-song",
            "http://label.bandcamp.com/album/lp",
            "https://bandcamp.com/tag/electronic",
        ];
        for url in page_urls {
            assert_eq!(
                classify(url),
                Input::Url {
                    url: url.to_string(),
                    kind: UrlKind::Page,
                },
                "Fallo en host de página: {url}"
            );
        }
    }

    #[test]
    fn test_classify_extension_wins_over_host() {
        assert_eq!(
            classify("https://youtube.com/audio.mp3"),
            Input::Url {
                url: "https://youtube.com/audio.mp3".to_string(),
                kind: UrlKind::DirectAudio,
            }
        );
        assert_eq!(
            classify("https://soundcloud.com/track.flac"),
            Input::Url {
                url: "https://soundcloud.com/track.flac".to_string(),
                kind: UrlKind::DirectAudio,
            }
        );
    }

    #[test]
    fn test_classify_unknown_url() {
        assert_eq!(
            classify("http://radio.example:8000/live"),
            Input::Url {
                url: "http://radio.example:8000/live".to_string(),
                kind: UrlKind::Unknown,
            }
        );
    }

    #[test]
    fn test_classify_content_type_playlist_and_hls_are_other() {
        let headers = [
            "audio/x-mpegurl",
            "audio/mpegurl",
            "audio/x-scpls",
            "application/pls+xml",
            "Audio/X-MpegUrl; charset=utf-8",
            "application/vnd.apple.mpegurl",
            "application/x-mpegurl",
            "APPLICATION/VND.APPLE.MPEGURL; charset=utf-8",
        ];
        for h in headers {
            assert_eq!(
                classify_content_type(h),
                ContentKind::Other,
                "Fallo esperando Other: {h}"
            );
        }
    }

    #[test]
    fn test_classify_content_type_audio() {
        let headers = [
            "audio/mpeg",
            "audio/aacp",
            "audio/ogg",
            "audio/flac",
            "audio/wav",
            "application/ogg",
            "AUDIO/MPEG; codecs=mp3",
        ];
        for h in headers {
            assert_eq!(
                classify_content_type(h),
                ContentKind::Audio,
                "Fallo en audio header: {h}"
            );
        }
    }

    #[test]
    fn test_classify_content_type_html() {
        assert_eq!(
            classify_content_type("text/html; charset=utf-8"),
            ContentKind::Html
        );
        assert_eq!(
            classify_content_type("application/xhtml+xml"),
            ContentKind::Html
        );
    }

    #[test]
    fn test_classify_content_type_other() {
        let headers = [
            "video/mp4",
            "video/webm",
            "application/octet-stream",
            "",
            "   ",
            "text/plain",
        ];
        for h in headers {
            assert_eq!(
                classify_content_type(h),
                ContentKind::Other,
                "Fallo en other header: {h}"
            );
        }
    }
}
