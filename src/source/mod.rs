//! Clasificación de fuentes de entrada (T1): archivos locales vs URLs remotas
//! y detección por URL o cabecera Content-Type.

pub mod playlist_file;

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
    /// Archivo de lista de reproducción (.m3u, .pls).
    PlaylistFile,
    /// Stream HLS (.m3u8) no soportado nativamente.
    Hls,
    /// Página web de servicio conocido (YouTube, SoundCloud, Bandcamp, Vimeo).
    Page,
    /// URL sin extensión ni host reconocido; requiere inspeccionar Content-Type.
    Unknown,
}

/// Clasificación del header HTTP `Content-Type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// Flujo de audio directo.
    Audio,
    /// Archivo de playlist (.m3u, .pls).
    Playlist,
    /// Stream HLS (.m3u8).
    Hls,
    /// Página HTML.
    Html,
    /// Otro tipo de contenido (video, binario genérico, etc.).
    Other,
}

pub(crate) const SCHEME_HTTP: &str = "http://";
pub(crate) const SCHEME_HTTPS: &str = "https://";

/// Compara si `s` comienza con `prefix` ignorando mayúsculas/minúsculas ASCII, sin panickear en límites UTF-8.
pub(crate) fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    let s_bytes = s.as_bytes();
    let prefix_bytes = prefix.as_bytes();
    if s_bytes.len() < prefix_bytes.len() {
        return false;
    }
    s_bytes[..prefix_bytes.len()].eq_ignore_ascii_case(prefix_bytes)
}

/// Detecta si `s` comienza con `http://` o `https://` y retorna la longitud del prefijo derivado de literales, o `None`.
pub(crate) fn http_scheme_len(s: &str) -> Option<usize> {
    if starts_with_ignore_ascii_case(s, SCHEME_HTTP) {
        Some(SCHEME_HTTP.len())
    } else if starts_with_ignore_ascii_case(s, SCHEME_HTTPS) {
        Some(SCHEME_HTTPS.len())
    } else {
        None
    }
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

/// Clasifica una entrada de usuario como ruta local o URL remota.
pub fn classify(input: &str) -> Input {
    let trimmed = input.trim();
    let scheme_len = match http_scheme_len(trimmed) {
        Some(len) => len,
        None => return Input::Local(PathBuf::from(trimmed)),
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
            "m3u" | "pls" => {
                return Input::Url {
                    url: trimmed.to_string(),
                    kind: UrlKind::PlaylistFile,
                };
            }
            "m3u8" => {
                return Input::Url {
                    url: trimmed.to_string(),
                    kind: UrlKind::Hls,
                };
            }
            _ => {}
        }
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

/// Clasifica una cabecera HTTP `Content-Type`.
pub fn classify_content_type(header: &str) -> ContentKind {
    let mime = header
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    match mime.as_str() {
        "audio/x-mpegurl" | "audio/mpegurl" | "audio/x-scpls" | "application/pls+xml" => {
            ContentKind::Playlist
        }
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" => ContentKind::Hls,
        "application/ogg" => ContentKind::Audio,
        "text/html" | "application/xhtml+xml" => ContentKind::Html,
        _ if mime.starts_with("audio/") => ContentKind::Audio,
        _ => ContentKind::Other,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_classify_playlists_and_hls() {
        assert_eq!(
            classify("http://radio.example.org/listen.m3u"),
            Input::Url {
                url: "http://radio.example.org/listen.m3u".to_string(),
                kind: UrlKind::PlaylistFile,
            }
        );
        assert_eq!(
            classify("http://radio.example.org/listen.pls"),
            Input::Url {
                url: "http://radio.example.org/listen.pls".to_string(),
                kind: UrlKind::PlaylistFile,
            }
        );
        assert_eq!(
            classify("https://live.example.com/stream.m3u8"),
            Input::Url {
                url: "https://live.example.com/stream.m3u8".to_string(),
                kind: UrlKind::Hls,
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
            classify("https://soundcloud.com/podcast.pls"),
            Input::Url {
                url: "https://soundcloud.com/podcast.pls".to_string(),
                kind: UrlKind::PlaylistFile,
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
    fn test_classify_content_type_playlist() {
        let headers = [
            "audio/x-mpegurl",
            "audio/mpegurl",
            "audio/x-scpls",
            "application/pls+xml",
            "Audio/X-MpegUrl; charset=utf-8",
        ];
        for h in headers {
            assert_eq!(
                classify_content_type(h),
                ContentKind::Playlist,
                "Fallo en playlist header: {h}"
            );
        }
    }

    #[test]
    fn test_classify_content_type_hls() {
        let headers = [
            "application/vnd.apple.mpegurl",
            "application/x-mpegurl",
            "APPLICATION/VND.APPLE.MPEGURL; charset=utf-8",
        ];
        for h in headers {
            assert_eq!(
                classify_content_type(h),
                ContentKind::Hls,
                "Fallo en HLS header: {h}"
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
