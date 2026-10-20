//! Figures out what a pasted YouTube / YouTube Music link points at: one
//! song, a playlist (or album), or a song inside a playlist where the user
//! has to choose.

use reqwest::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkKind {
    Song {
        video_id: String,
    },
    /// `source` is a canonical URL the provider can read (`playlist?list=` or an album `browse/` page).
    Playlist {
        source: String,
    },
    /// A song opened from a playlist (`watch?v=…&list=…`): song or whole playlist?
    Ambiguous {
        video_id: String,
        playlist_source: String,
    },
}

const HOSTS: &[&str] = &["youtube.com", "www.youtube.com", "m.youtube.com", "music.youtube.com"];

fn valid_video_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_list_id(id: &str) -> bool {
    (2..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn playlist_source(list_id: &str) -> String {
    format!("https://music.youtube.com/playlist?list={list_id}")
}

/// Auto-generated mixes ("Radio", "My Mix") are endless and personal; a
/// watch link inside one is really just a link to that song.
fn is_generated_mix(list_id: &str) -> bool {
    list_id.starts_with("RD")
}

pub fn classify(input: &str) -> Result<LinkKind, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Paste a YouTube or YouTube Music link");
    }
    let with_scheme = if trimmed.contains("://") { trimmed.to_string() } else { format!("https://{trimmed}") };
    let url = Url::parse(&with_scheme).map_err(|_| "That doesn't look like a link")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("That doesn't look like a link");
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let query = |key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
    let segments = url.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect::<Vec<_>>()).unwrap_or_default();

    let (video, list) = if host == "youtu.be" {
        (segments.first().map(|s| s.to_string()), query("list"))
    } else if HOSTS.contains(&host.as_str()) {
        match segments.as_slice() {
            ["watch"] => (query("v"), query("list")),
            ["shorts", id] | ["live", id] | ["embed", id] => (Some(id.to_string()), None),
            ["playlist"] => (None, query("list")),
            ["browse", id] => {
                let id = id.to_string();
                if let Some(list) = id.strip_prefix("VL") {
                    (None, Some(list.to_string()))
                } else if id.starts_with("MPRE") && valid_list_id(&id) {
                    // An album page; the provider reads it as an album.
                    return Ok(LinkKind::Playlist { source: format!("https://music.youtube.com/browse/{id}") });
                } else {
                    return Err("That YouTube Music page isn't a song, album or playlist");
                }
            }
            _ => return Err("That YouTube link isn't a song or a playlist"),
        }
    } else {
        return Err("Only YouTube and YouTube Music links are supported");
    };

    let video = video.filter(|v| valid_video_id(v));
    let list = list.filter(|l| valid_list_id(l));
    match (video, list) {
        (Some(v), Some(l)) if !is_generated_mix(&l) => Ok(LinkKind::Ambiguous { video_id: v, playlist_source: playlist_source(&l) }),
        (Some(v), _) => Ok(LinkKind::Song { video_id: v }),
        (None, Some(l)) => Ok(LinkKind::Playlist { source: playlist_source(&l) }),
        (None, None) => Err("That link has no song or playlist in it"),
    }
}

/// The canonical playlist URL inside a link (playlist or ambiguous), if any.
pub fn playlist_source_of(url: &str) -> Option<String> {
    match classify(url) {
        Ok(LinkKind::Playlist { source }) => Some(source),
        Ok(LinkKind::Ambiguous { playlist_source, .. }) => Some(playlist_source),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> LinkKind {
        LinkKind::Song { video_id: id.into() }
    }

    #[test]
    fn song_links() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&si=abc",
            "https://youtu.be/dQw4w9WgXcQ?si=xyz",
            "youtu.be/dQw4w9WgXcQ",
            "https://m.youtube.com/watch?feature=share&v=dQw4w9WgXcQ",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "  https://music.youtube.com/watch?v=dQw4w9WgXcQ  ",
        ] {
            assert_eq!(classify(url), Ok(song("dQw4w9WgXcQ")), "{url}");
        }
    }

    #[test]
    fn playlist_links() {
        assert_eq!(
            classify("https://music.youtube.com/playlist?list=OLAK5uy_abc-DEF_123&si=q"),
            Ok(LinkKind::Playlist { source: "https://music.youtube.com/playlist?list=OLAK5uy_abc-DEF_123".into() })
        );
        assert_eq!(
            classify("https://www.youtube.com/playlist?list=PLx0sYbCqOb8TBPRdmBHs5Iftvv9TPboYG"),
            Ok(LinkKind::Playlist { source: "https://music.youtube.com/playlist?list=PLx0sYbCqOb8TBPRdmBHs5Iftvv9TPboYG".into() })
        );
        assert_eq!(
            classify("https://music.youtube.com/browse/VLPLabcdef123"),
            Ok(LinkKind::Playlist { source: "https://music.youtube.com/playlist?list=PLabcdef123".into() })
        );
        assert_eq!(
            classify("https://music.youtube.com/browse/MPREb_4pL8gzRtw1p"),
            Ok(LinkKind::Playlist { source: "https://music.youtube.com/browse/MPREb_4pL8gzRtw1p".into() })
        );
    }

    #[test]
    fn song_inside_playlist_is_ambiguous_but_mixes_are_songs() {
        assert_eq!(
            classify("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=OLAK5uy_abc"),
            Ok(LinkKind::Ambiguous {
                video_id: "dQw4w9WgXcQ".into(),
                playlist_source: "https://music.youtube.com/playlist?list=OLAK5uy_abc".into()
            })
        );
        assert_eq!(classify("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RDdQw4w9WgXcQ&start_radio=1"), Ok(song("dQw4w9WgXcQ")));
        assert_eq!(classify("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDAMVMdQw4w9WgXcQ"), Ok(song("dQw4w9WgXcQ")));
    }

    #[test]
    fn invalid_links_are_rejected() {
        for url in [
            "",
            "hello there",
            "https://example.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com.evil.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/@someone",
            "https://music.youtube.com/browse/UCabcdef",
            "ftp://youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/playlist?list=bad%20id",
        ] {
            assert!(classify(url).is_err(), "{url} should be rejected");
        }
    }

    #[test]
    fn playlist_source_helpers() {
        assert!(playlist_source_of("https://youtu.be/dQw4w9WgXcQ").is_none());
        assert_eq!(
            playlist_source_of("https://youtube.com/watch?v=dQw4w9WgXcQ&list=PL123").as_deref(),
            Some("https://music.youtube.com/playlist?list=PL123")
        );
    }
}
