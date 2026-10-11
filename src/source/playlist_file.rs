//! Parseo de archivos de lista de reproducción (T2): `.m3u` y `.pls`.

use std::fmt;

/// Errores durante el parseo de listas de reproducción.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistError {
    /// La lista no contiene ninguna entrada utilizable.
    Empty,
    /// Se detectó una lista HLS (#EXT-X-) que no está soportada.
    Hls,
}

impl fmt::Display for PlaylistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaylistError::Empty => write!(f, "lista de reproducción vacía"),
            PlaylistError::Hls => write!(f, "las listas HLS (.m3u8) no están soportadas"),
        }
    }
}

impl std::error::Error for PlaylistError {}

/// Compara si `s` comienza con `prefix` ignorando mayúsculas/minúsculas ASCII, sin panickear en límites UTF-8.
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    let s_bytes = s.as_bytes();
    let prefix_bytes = prefix.as_bytes();
    if s_bytes.len() < prefix_bytes.len() {
        return false;
    }
    s_bytes[..prefix_bytes.len()].eq_ignore_ascii_case(prefix_bytes)
}

/// Helper para resolver URLs relativas en listas de reproducción.
fn resolve_url(entry: &str, base_url: &str) -> String {
    let entry_trimmed = entry.trim();
    if starts_with_ignore_ascii_case(entry_trimmed, "http://")
        || starts_with_ignore_ascii_case(entry_trimmed, "https://")
    {
        return entry_trimmed.to_string();
    }

    let base_clean = match base_url.find(['?', '#']) {
        Some(idx) => &base_url[..idx],
        None => base_url,
    };

    let scheme_len = if starts_with_ignore_ascii_case(base_clean, "http://") {
        7
    } else if starts_with_ignore_ascii_case(base_clean, "https://") {
        8
    } else {
        0
    };


    let rest = &base_clean[scheme_len..];
    let host_slash_offset = rest.find('/');

    if entry_trimmed.starts_with('/') {
        let scheme_and_host = match host_slash_offset {
            Some(idx) => &base_clean[..scheme_len + idx],
            None => base_clean.trim_end_matches('/'),
        };
        format!("{scheme_and_host}{entry_trimmed}")
    } else {
        let dir = match host_slash_offset {
            Some(_) => match base_clean.rfind('/') {
                Some(idx) if idx >= scheme_len => &base_clean[..=idx],
                _ => base_clean,
            },
            None => {
                if base_clean.ends_with('/') {
                    base_clean
                } else {
                    return format!("{base_clean}/{entry_trimmed}");
                }
            }
        };
        format!("{dir}{entry_trimmed}")
    }
}

/// Parsea el contenido de una lista `.m3u`, resolviendo rutas relativas contra `base_url`.
pub fn parse_m3u(text: &str, base_url: &str) -> Result<Vec<String>, PlaylistError> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut entries = Vec::new();

    for line in text.lines() {
        let trimmed_start = line.trim_start();
        if trimmed_start.to_ascii_uppercase().starts_with("#EXT-X-") {
            return Err(PlaylistError::Hls);
        }
        if trimmed_start.starts_with('#') {
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        entries.push(resolve_url(trimmed, base_url));
    }

    if entries.is_empty() {
        return Err(PlaylistError::Empty);
    }

    Ok(entries)
}

/// Parsea el contenido de una lista `.pls` tipo INI.
pub fn parse_pls(text: &str) -> Result<Vec<String>, PlaylistError> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut entries: Vec<(usize, String)> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            continue;
        }

        if let Some((key, val)) = trimmed.split_once('=') {
            let key_trimmed = key.trim();
            let val_trimmed = val.trim();
            if starts_with_ignore_ascii_case(key_trimmed, "file")
                && key_trimmed.len() > 4
                && let Ok(idx) = key_trimmed[4..].parse::<usize>()
                && !val_trimmed.is_empty()
            {
                entries.push((idx, val_trimmed.to_string()));
            }
        }


    }

    if entries.is_empty() {
        return Err(PlaylistError::Empty);
    }

    entries.sort_by_key(|(idx, _)| *idx);
    Ok(entries.into_iter().map(|(_, url)| url).collect())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        assert_eq!(
            PlaylistError::Empty.to_string(),
            "lista de reproducción vacía"
        );
        assert_eq!(
            PlaylistError::Hls.to_string(),
            "las listas HLS (.m3u8) no están soportadas"
        );
    }

    #[test]
    fn test_parse_m3u_empty_or_comments_only() {
        assert_eq!(parse_m3u("", "http://example.com/"), Err(PlaylistError::Empty));
        assert_eq!(
            parse_m3u("   \r\n\n  ", "http://example.com/"),
            Err(PlaylistError::Empty)
        );
        assert_eq!(
            parse_m3u("#EXTM3U\n# Comment only\n", "http://example.com/"),
            Err(PlaylistError::Empty)
        );
    }

    #[test]
    fn test_parse_m3u_bom_and_crlf() {
        let content = "\u{FEFF}#EXTM3U\r\nhttp://example.com/stream1.mp3\r\nhttp://example.com/stream2.mp3\r\n";
        let res = parse_m3u(content, "http://example.com/").unwrap();
        assert_eq!(
            res,
            vec![
                "http://example.com/stream1.mp3".to_string(),
                "http://example.com/stream2.mp3".to_string()
            ]
        );
    }

    #[test]
    fn test_parse_m3u_hls_tag_returns_error() {
        let content = "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:10,\nsegment.ts";
        assert_eq!(
            parse_m3u(content, "http://example.com/"),
            Err(PlaylistError::Hls)
        );
    }

    #[test]
    fn test_parse_m3u_relative_url_resolution() {
        let content = r#"#EXTM3U
#EXTINF:120,Direct Absolute
https://audio.example.com/absolute.mp3
#EXTINF:60,Relative Root
/streams/live.aac
#EXTINF:90,Relative Directory
subfolder/track.ogg
"#;
        let base = "http://radio.example.com:8000/music/list.m3u?token=xyz#top";
        let res = parse_m3u(content, base).unwrap();
        assert_eq!(
            res,
            vec![
                "https://audio.example.com/absolute.mp3".to_string(),
                "http://radio.example.com:8000/streams/live.aac".to_string(),
                "http://radio.example.com:8000/music/subfolder/track.ogg".to_string(),
            ]
        );
    }

    #[test]
    fn test_parse_m3u_realistic_sample() {
        let sample = "\
#EXTM3U
#EXTINF:180,Artist One - Song One
http://example.com/audio/song1.mp3

#EXTINF:210,Artist Two - Song Two
http://example.com/audio/song2.flac
";
        let res = parse_m3u(sample, "http://example.com/").unwrap();
        assert_eq!(
            res,
            vec![
                "http://example.com/audio/song1.mp3".to_string(),
                "http://example.com/audio/song2.flac".to_string(),
            ]
        );
    }

    #[test]
    fn test_parse_pls_empty() {
        assert_eq!(parse_pls(""), Err(PlaylistError::Empty));
        assert_eq!(
            parse_pls("[playlist]\nNumberOfEntries=0\nVersion=2"),
            Err(PlaylistError::Empty)
        );
    }

    #[test]
    fn test_parse_pls_ordering_and_case_insensitive() {
        let content = "\u{FEFF}[playlist]\r\n\
; Icecast comment
NumberOfEntries=3\r\n\
FILE2=http://stream.example.com/two.mp3\r\n\
Title2=Second Stream\r\n\
Length2=-1\r\n\
File10=http://stream.example.com/ten.mp3\r\n\
file1=http://stream.example.com/one.mp3   \r\n\
Title1=First Stream\r\n\
Version=2\r\n";
        let res = parse_pls(content).unwrap();
        assert_eq!(
            res,
            vec![
                "http://stream.example.com/one.mp3".to_string(),
                "http://stream.example.com/two.mp3".to_string(),
                "http://stream.example.com/ten.mp3".to_string(),
            ]
        );
    }

    #[test]
    fn test_parse_pls_realistic_shoutcast_sample() {
        let sample = "[Playlist]\r\n\
numberofentries=1\r\n\
File1=http://live.streamprovider.net:8000/mountpoint\r\n\
Title1=(#1 - 500/1500) Super Radio FM\r\n\
Length1=-1\r\n\
Version=2\r\n";
        let res = parse_pls(sample).unwrap();
        assert_eq!(
            res,
            vec!["http://live.streamprovider.net:8000/mountpoint".to_string()]
        );
    }

    #[test]
    fn test_parse_non_ascii_entries_and_keys_do_not_panic() {
        let m3u = "#EXTM3U\ncanción.mp3\nñandú.flac\nññññ.mp3\n";
        let res = parse_m3u(m3u, "http://example.com/musica/").unwrap();
        assert_eq!(
            res,
            vec![
                "http://example.com/musica/canción.mp3".to_string(),
                "http://example.com/musica/ñandú.flac".to_string(),
                "http://example.com/musica/ññññ.mp3".to_string(),
            ]
        );

        let pls = "[playlist]\nNúmero=1\nñññ1=ignored\nFile1=http://example.com/stream\n";
        let res_pls = parse_pls(pls).unwrap();
        assert_eq!(res_pls, vec!["http://example.com/stream".to_string()]);
    }

}

