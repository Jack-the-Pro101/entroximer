//! HTTP for providers: one shared client, and the check that keeps clients from using the engine
//! to reach its own machine or private networks.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use reqwest::Url;
use reqwest::header::USER_AGENT;
use url::Host;

use super::ResolveError;

const NAME: &str = concat!("entroximer/", env!("CARGO_PKG_VERSION"));

/// When a signed media URL stops working, as its query says: in Unix seconds as `expire`
/// (YouTube) or `expires`/`Expires`; at the start of Bandcamp's `token`; as Amazon S3 signs, so
/// long (`X-Amz-Expires`) after it was signed (`X-Amz-Date`); or in a CloudFront policy
/// (`Policy`, as SoundCloud's carry).
pub(crate) fn url_expiry(url: &str) -> Option<SystemTime> {
    let url = Url::parse(url).ok()?;
    let query: HashMap<String, String> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let unix = |seconds: u64| UNIX_EPOCH + Duration::from_secs(seconds);
    let number = |key: &str| query.get(key).and_then(|value| value.parse::<u64>().ok());
    if let Some(seconds) = ["expire", "expires", "Expires"]
        .into_iter()
        .find_map(number)
    {
        return Some(unix(seconds));
    }
    // `token` and `ts` mean other things elsewhere
    let bandcamp = url
        .host_str()
        .is_some_and(|host| host == "bcbits.com" || host.ends_with(".bcbits.com"));
    if bandcamp
        && let Some(seconds) = query
            .get("token")
            .and_then(|token| token.split('_').next())
            .and_then(|seconds| seconds.parse().ok())
    {
        return Some(unix(seconds));
    }
    if let (Some(signed), Some(seconds)) = (query.get("X-Amz-Date"), number("X-Amz-Expires")) {
        return Some(amz_date(signed)? + Duration::from_secs(seconds));
    }
    // CloudFront's base64 swaps `+`, `=` and `/` for `-`, `_` and `~`
    let policy: String = query
        .get("Policy")?
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '=',
            '~' => '/',
            c => c,
        })
        .collect();
    let policy: serde_json::Value = serde_json::from_slice(&STANDARD.decode(policy).ok()?).ok()?;
    let seconds = policy["Statement"][0]["Condition"]["DateLessThan"]["AWS:EpochTime"].as_u64()?;
    Some(unix(seconds))
}

/// A time as Amazon's signatures give it, e.g. `20251104T120000Z`.
fn amz_date(text: &str) -> Option<SystemTime> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let digits = date.len() == 8 && time.len() == 6;
    if !digits || !date.bytes().chain(time.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let field = |text: &str| text.parse::<i64>().ok();
    let (year, month, day) = (field(&date[..4])?, field(&date[4..6])?, field(&date[6..])?);
    let (hour, minute, second) = (field(&time[..2])?, field(&time[2..4])?, field(&time[4..])?);
    let valid = (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour < 24
        && minute < 60
        && second <= 60;
    if !valid {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).ok()?))
}

/// Days from 1970-01-01 to a date of the Gregorian calendar (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    // counted from March, so that a leap day ends the year
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub(crate) fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        builder()
            .build()
            .expect("the HTTP client's settings are valid")
    })
}

/// A client like [`client`] whose requests all go through `proxy`.
pub(crate) fn proxied_client(proxy: &Url) -> reqwest::Result<reqwest::Client> {
    builder().proxy(reqwest::Proxy::all(proxy.clone())?).build()
}

fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().default_headers(
        [(USER_AGENT, NAME.parse().expect("valid header"))]
            .into_iter()
            .collect(),
    )
}

/// Fails if `url` points at this machine or a private network: a client could otherwise make the
/// engine fetch internal services. Hosts are resolved, so names pointing inwards are caught too.
pub(crate) async fn ensure_public(url: &Url) -> Result<(), ResolveError> {
    let refuse = || ResolveError::PrivateAddress(url.host_str().unwrap_or_default().to_owned());
    let addresses: Vec<IpAddr> = match url.host() {
        Some(Host::Ipv4(ip)) => vec![ip.into()],
        Some(Host::Ipv6(ip)) => vec![ip.into()],
        Some(Host::Domain(domain)) => {
            let port = url.port_or_known_default().unwrap_or(443);
            tokio::net::lookup_host((domain, port))
                .await
                .map_err(|source| ResolveError::Lookup {
                    host: domain.to_owned(),
                    source,
                })?
                .map(|address| address.ip())
                .collect()
        }
        None => return Err(refuse()),
    };
    if addresses.iter().copied().any(is_internal) {
        return Err(refuse());
    }
    Ok(())
}

fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_internal_v4(ip),
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.to_ipv4_mapped().is_some_and(is_internal_v4)
        }
    }
}

fn is_internal_v4(ip: Ipv4Addr) -> bool {
    let shared = ip.octets()[0] == 100 && (ip.octets()[1] & 0b1100_0000) == 64; // 100.64.0.0/10
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || shared
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn signed_urls_tell_when_they_expire() {
        let at = |seconds| Some(UNIX_EPOCH + Duration::from_secs(seconds));
        assert_eq!(
            url_expiry(
                "https://rr1.googlevideo.com/videoplayback?expire=1762300000&ei=x&ip=1.2.3.4"
            ),
            at(1_762_300_000)
        );
        assert_eq!(
            url_expiry("https://cdn.example/a.mp3?Expires=1762300001"),
            at(1_762_300_001)
        );
        // a CloudFront policy, as SoundCloud's playlists carry besides `expires`
        let policy = STANDARD
            .encode(r#"{"Statement":[{"Resource":"https://x/*","Condition":{"DateLessThan":{"AWS:EpochTime":1762300002}}}]}"#)
            .replace('+', "-")
            .replace('=', "_")
            .replace('/', "~");
        assert_eq!(
            url_expiry(&format!(
                "https://playback.example/a.m3u8?Policy={policy}&Signature=s"
            )),
            at(1_762_300_002)
        );
        assert_eq!(url_expiry("https://example.com/a.mp3"), None);
        assert_eq!(url_expiry("not a url"), None);
    }

    #[test]
    fn bandcamp_and_s3_urls_tell_when_they_expire() {
        let at = |seconds| Some(UNIX_EPOCH + Duration::from_secs(seconds));
        assert_eq!(
            url_expiry(
                "https://t4.bcbits.com/stream/abc/mp3-128/1?p=0&ts=1793897691&t=x&token=1793897691_3f2e"
            ),
            at(1_793_897_691)
        );
        assert_eq!(
            url_expiry("https://example.com/a.mp3?token=1793897691_3f2e"),
            None,
            "a token elsewhere means something else"
        );
        assert_eq!(
            url_expiry(
                "https://bucket.s3.amazonaws.com/a.mp3?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Date=20251104T120000Z&X-Amz-Expires=3600&X-Amz-Signature=s"
            ),
            at(1_762_257_600 + 3600)
        );
        assert_eq!(
            amz_date("20240229T235959Z"),
            at(1_709_251_199),
            "a leap day"
        );
        assert_eq!(amz_date("20241301T000000Z"), None);
        assert_eq!(amz_date("2024-02-29T23:59:59Z"), None);
    }

    #[test]
    fn recognises_internal_addresses() {
        for internal in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.10",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.0.1",
        ] {
            assert!(is_internal(internal.parse().unwrap()), "{internal}");
        }
        for public in ["1.1.1.1", "142.250.80.46", "2606:4700::1111", "100.128.0.1"] {
            assert!(!is_internal(public.parse().unwrap()), "{public}");
        }
    }

    #[tokio::test]
    async fn refuses_urls_to_internal_hosts() {
        for url in [
            "http://127.0.0.1:2333/",
            "http://[::1]/",
            "http://localhost/file.mp3",
        ] {
            let result = ensure_public(&url.parse().unwrap()).await;
            assert!(
                matches!(result, Err(ResolveError::PrivateAddress(_))),
                "{url}"
            );
        }
    }
}
