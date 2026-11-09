//! The web player's own script, read for the current hash of one of its queries when Spotify no
//! longer runs the one the engine has (see [`super::web`]). open.spotify.com lists the desktop
//! player's files (`__CDN_FILE_URLS__`, even on the mobile site it serves the engine), and the
//! player's main script registers each query by name, kind and hash, as in
//! `"fetchPlaylist","query","<hash>"`.

use std::collections::HashMap;
use std::io::Read as _;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::read::GzDecoder;
use reqwest::header::CONTENT_ENCODING;
use reqwest::{StatusCode, Url};

use super::super::net;
use super::api::SpotifyError;

const SITE: &str = "https://open.spotify.com/";
/// Where the site's list of the player's files starts, base64-encoded JSON.
const FILES_START: &str = r#"<script id="__CDN_FILE_URLS__" type="text/plain">"#;
/// The player's main script, as the list names it.
const MAIN_SCRIPT: &str = "build/web-player/web-player.js";
/// Where the player's files are; nothing else is fetched.
const CDN_HOST: &str = "open.spotifycdn.com";
/// The site's page is about 300 KiB.
const MOST_PAGE_BYTES: usize = 4 << 20;
/// The main script is about 4.5 MiB (1.3 MiB as served, compressed).
const MOST_SCRIPT_BYTES: usize = 32 << 20;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The hash the web player's current main script registers the query `name` with.
pub(super) async fn query_hash(name: &str) -> Result<String, SpotifyError> {
    let site = Url::parse(SITE).expect("the site's URL is valid");
    let page = fetch(site, MOST_PAGE_BYTES).await?;
    let script = main_script(&String::from_utf8_lossy(&page))?;
    let code = fetch(script, MOST_SCRIPT_BYTES).await?;
    find_hash(&String::from_utf8_lossy(&code), name).ok_or_else(|| {
        SpotifyError::Web(format!(
            "the web player's script no longer registers the query `{name}`"
        ))
    })
}

/// The body at `url`, uncompressed, of at most `most` bytes.
async fn fetch(url: Url, most: usize) -> Result<Vec<u8>, SpotifyError> {
    let mut response = net::client()
        .get(url.clone())
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(SpotifyError::Http)?;
    if response.status() != StatusCode::OK {
        return Err(SpotifyError::Web(format!(
            "`{url}` answered HTTP {}",
            response.status()
        )));
    }
    // Spotify's CDN compresses its scripts whatever the request accepts
    let gzipped = response
        .headers()
        .get(CONTENT_ENCODING)
        .is_some_and(|encoding| encoding.as_bytes().eq_ignore_ascii_case(b"gzip"));
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(SpotifyError::Http)? {
        body.extend_from_slice(&chunk);
        if body.len() > most {
            return Err(SpotifyError::Web(format!("`{url}` is too large")));
        }
    }
    if !gzipped {
        return Ok(body);
    }
    tokio::task::spawn_blocking(move || gunzip(&body, most))
        .await
        .expect("decompressing does not panic")
        .ok_or_else(|| SpotifyError::Web(format!("`{url}` is not valid gzip or too large")))
}

/// `data` uncompressed, unless it is not gzip or comes to more than `most` bytes.
fn gunzip(data: &[u8], most: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    GzDecoder::new(data)
        .take(most as u64 + 1)
        .read_to_end(&mut out)
        .ok()?;
    (out.len() <= most).then_some(out)
}

/// The URL of the player's main script, from the site's list of its files.
fn main_script(page: &str) -> Result<Url, SpotifyError> {
    let missing =
        || SpotifyError::Web("open.spotify.com no longer lists the web player's files".into());
    let encoded = page
        .split_once(FILES_START)
        .and_then(|(_, rest)| rest.split_once("</script>"))
        .map(|(encoded, _)| encoded.trim())
        .ok_or_else(missing)?;
    let files: HashMap<String, String> = STANDARD
        .decode(encoded)
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .ok_or_else(missing)?;
    let url = files
        .get(MAIN_SCRIPT)
        .and_then(|url| Url::parse(url).ok())
        .ok_or_else(missing)?;
    if url.scheme() != "https" || url.host_str() != Some(CDN_HOST) {
        return Err(SpotifyError::Web(format!(
            "refusing to fetch the web player's script from `{url}`"
        )));
    }
    Ok(url)
}

/// The hash `code` registers the query `name` with.
fn find_hash(code: &str, name: &str) -> Option<String> {
    let registered = format!(r#""{name}","query",""#);
    let (_, rest) = code.split_once(&registered)?;
    let (hash, _) = rest.split_once('"')?;
    let valid = hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    valid.then(|| hash.to_owned())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    const HASH: &str = "21ea11a6ac75a5f3e01fd365d29c18e0a4a8b7dc09f89bbb5d73910190c6acaf";

    #[test]
    fn finds_the_main_script_in_the_sites_list_of_files() {
        let files = STANDARD.encode(
            r#"{"build/web-player/1055.js": "https://open.spotifycdn.com/cdn/build/web-player/1055.66b17af3.js",
                "build/web-player/web-player.js": "https://open.spotifycdn.com/cdn/build/web-player/web-player.632a438d.js"}"#,
        );
        let page = format!(r#"<html><head>{FILES_START}{files}</script></head></html>"#);
        assert_eq!(
            main_script(&page).unwrap().as_str(),
            "https://open.spotifycdn.com/cdn/build/web-player/web-player.632a438d.js"
        );

        let elsewhere = STANDARD
            .encode(r#"{"build/web-player/web-player.js": "https://evil.example/web-player.js"}"#);
        let page = format!("{FILES_START}{elsewhere}</script>");
        assert!(
            main_script(&page)
                .unwrap_err()
                .to_string()
                .contains("refusing")
        );
        assert!(main_script("<html></html>").is_err());
    }

    #[test]
    fn finds_registered_queries() {
        let code = format!(
            r#"x=new o.l("editablePlaylists","query","d5c4b8096437dcc2ac9528c91dfcd299e35b747cda2f8f75d28f41f49c5092ba",null),y=new o.l("fetchPlaylist","query","{HASH}",null)"#
        );
        assert_eq!(find_hash(&code, "fetchPlaylist").as_deref(), Some(HASH));
        assert_eq!(find_hash(&code, "fetchAlbum"), None);
        assert_eq!(
            find_hash(r#""fetchPlaylist","query","abc""#, "fetchPlaylist"),
            None
        );
    }

    #[test]
    fn decompresses_within_bounds() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&[b'a'; 1000]).unwrap();
        let gzipped = encoder.finish().unwrap();
        assert_eq!(gunzip(&gzipped, 1000).unwrap().len(), 1000);
        assert!(gunzip(&gzipped, 999).is_none(), "too large");
        assert!(gunzip(b"plain text", 1000).is_none());
    }

    /// Reads the current hash of the playlist query from the real web player. Needs the network,
    /// so it only runs when asked: `cargo test -- --ignored web_player_script`.
    #[tokio::test]
    #[ignore = "reads open.spotify.com"]
    async fn reads_the_playlist_query_from_the_real_web_player_script() {
        let hash = query_hash("fetchPlaylist").await.unwrap();
        assert_eq!(hash.len(), 64, "{hash}");
    }
}
