//! HTTP for providers: one shared client, and the check that keeps clients from using the engine
//! to reach its own machine or private networks.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::OnceLock;

use reqwest::Url;
use reqwest::header::USER_AGENT;
use url::Host;

use super::ResolveError;

const NAME: &str = concat!("entroximer/", env!("CARGO_PKG_VERSION"));

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
    use super::*;

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
