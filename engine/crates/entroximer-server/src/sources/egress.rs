//! The way out to the network for links that clients give the engine.
//!
//! Checking a link before it is fetched ([`net::ensure_public`]) only covers the link as given.
//! What fetches it, ffmpeg, ffprobe, yt-dlp or the engine's HTTP client, resolves its host again
//! and goes wherever the answers lead: a redirect, a media URL yt-dlp finds in a page, the
//! segments an HLS playlist lists. Each of those could point at the engine's own machine or a
//! private network.
//!
//! So all of them make their requests through an HTTP proxy inside the engine, on a loopback
//! port. It resolves every host itself, refuses those with a private address, and connects to
//! the very addresses it checked: there is no second lookup that could answer differently.
//! Whatever a link leads to, nothing connects to an address that was not checked.
//!
//! `https` goes through as a `CONNECT` tunnel. For `http` the proxy reads no more than the
//! request's head, to learn its host: it asks the server to close the connection after its
//! response (`Connection: close`) and passes everything else through as it is, so that a
//! connection is never used for a second host, and odd servers (internet radio) are not put off
//! by a proxy rewriting their responses.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use entroximer_core::producer::ffmpeg::{PROXY_EXEMPTIONS, PROXY_VARIABLE};
use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::net;

/// A request's head is read within this, and no larger than this.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HEAD_BYTES: usize = 64 << 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a response may still take once its client has stopped sending: a client that is
/// gone must not leave its connection to a silent server open.
const LINGER: Duration = Duration::from_secs(10);

/// Which addresses may be connected to.
type Permits = fn(IpAddr) -> bool;

/// The proxy, and an HTTP client that goes through it. It serves until dropped.
#[derive(Debug)]
pub struct Egress {
    proxy: Url,
    client: reqwest::Client,
    serving: JoinHandle<()>,
}

impl Egress {
    /// Starts the proxy on a free loopback port, on the Tokio runtime this is called in.
    pub fn start() -> io::Result<Self> {
        Self::start_permitting(|ip| !net::is_internal(ip))
    }

    fn start_permitting(permits: Permits) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let proxy = Url::parse(&format!("http://{}", listener.local_addr()?))
            .expect("a socket address is a valid URL authority");
        let client = net::proxied_client(&proxy).map_err(io::Error::other)?;
        Ok(Self {
            proxy,
            client,
            serving: tokio::spawn(serve(listener, permits)),
        })
    }

    /// The proxy's URL, for `--proxy` and the like.
    pub fn proxy(&self) -> &Url {
        &self.proxy
    }

    /// An HTTP client whose requests, redirects included, go through the proxy.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

impl Drop for Egress {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

/// Has an ffmpeg or ffprobe about to run make all its HTTP(S) requests through `proxy`: those
/// for its input, and those the input leads to. The environment's proxy is what ffmpeg uses
/// where an input's options do not reach, and the environment must not exempt hosts from it.
pub fn route_ffmpeg(command: &mut tokio::process::Command, proxy: &Url) {
    command.env(PROXY_VARIABLE, proxy.as_str());
    for exemption in PROXY_EXEMPTIONS {
        command.env_remove(exemption);
    }
}

async fn serve(listener: TcpListener, permits: Permits) {
    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                tokio::spawn(async move {
                    if let Err(e) = relay(client, permits).await {
                        tracing::debug!("a proxied connection ended: {e}");
                    }
                });
            }
            // out of file descriptors, most likely: connections in progress will free some
            Err(e) => {
                tracing::warn!("the links' proxy cannot accept a connection: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Serves one connection: one tunnel, or one request and its response.
async fn relay(mut client: TcpStream, permits: Permits) -> io::Result<()> {
    let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client)).await {
        Ok(read) => read?,
        Err(_) => None,
    };
    let request = head.and_then(|(head, rest)| Some((Request::parse(&head)?, rest)));
    let Some((request, rest)) = request else {
        return refuse(&mut client, "400 Bad Request", "not a proxy request").await;
    };
    let mut server = match connect(&request.host, request.port, permits).await {
        Ok(server) => server,
        Err(Refusal::Private) => {
            tracing::info!(
                host = request.host,
                "refused a request to a private address"
            );
            let why = format!("{} is on a private network", request.host);
            return refuse(&mut client, "403 Forbidden (private address)", &why).await;
        }
        Err(Refusal::Unreachable(e)) => {
            let why = format!("cannot connect to {}: {e}", request.host);
            return refuse(&mut client, "502 Bad Gateway", &why).await;
        }
    };
    // requests and handshakes are small writes that must not wait to be batched
    for socket in [&client, &server] {
        let _ = socket.set_nodelay(true);
    }
    match request.forward {
        Some(head) => server.write_all(&head).await?,
        None => {
            client
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await?;
        }
    }
    server.write_all(&rest).await?;

    let (mut from_client, mut to_client) = client.split();
    let (mut from_server, mut to_server) = server.split();
    let sending = async {
        tokio::io::copy(&mut from_client, &mut to_server).await?;
        to_server.shutdown().await
    };
    let receiving = tokio::io::copy(&mut from_server, &mut to_client);
    tokio::pin!(sending, receiving);
    tokio::select! {
        received = &mut receiving => received.map(drop),
        sent = &mut sending => {
            sent?;
            match tokio::time::timeout(LINGER, receiving).await {
                Ok(received) => received.map(drop),
                Err(_) => Ok(()),
            }
        }
    }
}

/// Reads up to the blank line that ends a request's head. Returns the head and what was read
/// past it, or `None` if the client sent no complete head of a reasonable size.
async fn read_head(client: &mut TcpStream) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    const END: &[u8] = b"\r\n\r\n";
    let mut read = Vec::with_capacity(1024);
    let mut searched = 0;
    loop {
        if let Some(end) = read[searched..].windows(END.len()).position(|w| w == END) {
            let rest = read.split_off(searched + end + END.len());
            return Ok(Some((read, rest)));
        }
        searched = read.len().saturating_sub(END.len() - 1);
        if read.len() >= MAX_HEAD_BYTES || client.read_buf(&mut read).await? == 0 {
            return Ok(None);
        }
    }
}

async fn refuse(client: &mut TcpStream, status: &str, why: &str) -> io::Result<()> {
    let body = format!("entroximer: {why}\n");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(response.as_bytes()).await?;
    client.shutdown().await
}

enum Refusal {
    Private,
    Unreachable(io::Error),
}

/// Connects to `host` if every address it has is permitted, without looking it up again.
async fn connect(host: &str, port: u16, permits: Permits) -> Result<TcpStream, Refusal> {
    let addresses: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .map_err(Refusal::Unreachable)?
            .collect(),
    };
    if !addresses.iter().all(|address| permits(address.ip())) {
        return Err(Refusal::Private);
    }
    let connecting = TcpStream::connect(addresses.as_slice());
    match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
        Ok(connected) => connected.map_err(Refusal::Unreachable),
        Err(_) => Err(Refusal::Unreachable(io::ErrorKind::TimedOut.into())),
    }
}

/// What a request's head asks the proxy for.
#[derive(Debug, PartialEq, Eq)]
struct Request {
    host: String,
    port: u16,
    /// The head to send the server, for a plain HTTP request; `None` for a `CONNECT` tunnel.
    forward: Option<Vec<u8>>,
}

impl Request {
    /// `CONNECT host:port`, or a request for an absolute `http://` URL; anything else is `None`.
    fn parse(head: &[u8]) -> Option<Self> {
        let head = std::str::from_utf8(head).ok()?;
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next()?.split(' ');
        let (method, target, version) = (
            request_line.next()?,
            request_line.next()?,
            request_line.next()?,
        );
        if request_line.next().is_some() || !version.starts_with("HTTP/1.") {
            return None;
        }
        if method.eq_ignore_ascii_case("CONNECT") {
            let (host, port) = authority(target, None)?;
            return Some(Self {
                host,
                port,
                forward: None,
            });
        }

        let scheme = target.get(..7)?;
        if !scheme.eq_ignore_ascii_case("http://") {
            return None;
        }
        let after_scheme = &target[7..];
        let path_start = after_scheme.find(['/', '?']).unwrap_or(after_scheme.len());
        let (named, path) = after_scheme.split_at(path_start);
        // credentials in a URL are sent as a header, never to the server in its request line
        let named = named.rsplit_once('@').map_or(named, |(_, host)| host);
        let (host, port) = authority(named, Some(80))?;

        let mut forward = String::with_capacity(head.len() + 32);
        forward.push_str(method);
        forward.push(' ');
        if !path.starts_with('/') {
            forward.push('/');
        }
        forward.push_str(path);
        forward.push(' ');
        forward.push_str(version);
        forward.push_str("\r\n");
        let mut names_host = false;
        for line in lines.filter(|line| !line.is_empty()) {
            let name = line.split(':').next()?.trim();
            let is = |header: &str| name.eq_ignore_ascii_case(header);
            // between the client and the proxy only, or replaced below
            if is("proxy-connection")
                || is("proxy-authorization")
                || is("connection")
                || is("keep-alive")
            {
                continue;
            }
            names_host |= is("host");
            forward.push_str(line);
            forward.push_str("\r\n");
        }
        if !names_host {
            forward.push_str("Host: ");
            forward.push_str(named);
            forward.push_str("\r\n");
        }
        // one request a connection: the next one may be for another host
        forward.push_str("Connection: close\r\n\r\n");
        Some(Self {
            host,
            port,
            forward: Some(forward.into_bytes()),
        })
    }
}

/// The host and port of `host`, `host:port`, `[v6]` or `[v6]:port`.
fn authority(text: &str, default_port: Option<u16>) -> Option<(String, u16)> {
    let (host, port) = match text.strip_prefix('[') {
        Some(bracketed) => {
            let (host, after) = bracketed.split_once(']')?;
            (host, after.strip_prefix(':'))
        }
        None => match text.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (text, None),
        },
    };
    let port = match port {
        Some(port) => port.parse().ok()?,
        None => default_port?,
    };
    (!host.is_empty()).then(|| (host.to_owned(), port))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use axum::Router;
    use axum::http::{HeaderMap, Uri, header};
    use axum::response::Redirect;
    use axum::routing::get;

    use super::*;

    /// The second address of the loopback network: reachable like the first, but not permitted
    /// by [`first_loopback_only`].
    const OTHER: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);

    fn first_loopback_only(ip: IpAddr) -> bool {
        ip == IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    /// Serves `router` on a free port of `ip`.
    async fn serve_on(ip: Ipv4Addr, router: Router) -> SocketAddr {
        let listener = TcpListener::bind((ip, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        address
    }

    /// Answers with the request's target and headers, as the server saw them.
    async fn echo(uri: Uri, headers: HeaderMap) -> String {
        let mut seen = format!("target {uri}\n");
        for (name, value) in &headers {
            seen.push_str(&format!("{name}: {}\n", value.to_str().unwrap()));
        }
        seen
    }

    #[test]
    fn reads_what_a_request_is_for() {
        let tunnel =
            Request::parse(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n");
        assert_eq!(
            tunnel,
            Some(Request {
                host: "example.com".to_owned(),
                port: 443,
                forward: None
            })
        );
        let v6 = Request::parse(b"CONNECT [2606:4700::1111]:8443 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("2606:4700::1111", 8443));

        let plain = Request::parse(
            b"GET http://user:secret@radio.example:8000/live?x=1 HTTP/1.1\r\n\
              Host: radio.example:8000\r\nIcy-MetaData: 1\r\nProxy-Connection: keep-alive\r\n\
              Connection: keep-alive\r\nProxy-Authorization: Basic eDp5\r\n\r\n",
        )
        .unwrap();
        assert_eq!((plain.host.as_str(), plain.port), ("radio.example", 8000));
        assert_eq!(
            String::from_utf8(plain.forward.unwrap()).unwrap(),
            "GET /live?x=1 HTTP/1.1\r\nHost: radio.example:8000\r\nIcy-MetaData: 1\r\n\
             Connection: close\r\n\r\n"
        );

        let bare = Request::parse(b"HEAD http://example.com HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!(bare.port, 80);
        assert_eq!(
            String::from_utf8(bare.forward.unwrap()).unwrap(),
            "HEAD / HTTP/1.0\r\nHost: example.com\r\nConnection: close\r\n\r\n"
        );

        for refused in [
            "GET / HTTP/1.1\r\nHost: example.com\r\n\r\n",
            "GET ftp://example.com/file HTTP/1.1\r\n\r\n",
            "GET https://example.com/ HTTP/1.1\r\n\r\n",
            "CONNECT example.com HTTP/1.1\r\n\r\n",
            "CONNECT :443 HTTP/1.1\r\n\r\n",
            "GET http://example.com:port/ HTTP/1.1\r\n\r\n",
            "GET http://example.com/ HTTP/2\r\n\r\n",
            "nonsense\r\n\r\n",
        ] {
            assert_eq!(Request::parse(refused.as_bytes()), None, "{refused}");
        }
    }

    #[tokio::test]
    async fn requests_pass_to_permitted_addresses_only() {
        let router = Router::new().route("/seen", get(echo));
        let permitted = serve_on(Ipv4Addr::LOCALHOST, router.clone()).await;
        let other = serve_on(OTHER, router).await;
        let egress = Egress::start_permitting(first_loopback_only).unwrap();

        let response = egress
            .client()
            .get(format!("http://{permitted}/seen?x=1"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let seen = response.text().await.unwrap();
        assert!(seen.contains("target /seen?x=1\n"), "{seen}");
        assert!(seen.contains(&format!("host: {permitted}\n")), "{seen}");
        assert!(seen.contains("connection: close\n"), "{seen}");
        assert!(!seen.contains("proxy"), "{seen}");

        let refused = egress
            .client()
            .get(format!("http://{other}/seen"))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 403);
        assert!(refused.text().await.unwrap().contains("private network"));
    }

    #[tokio::test]
    async fn a_redirect_to_a_private_address_is_not_followed_there() {
        let other = serve_on(OTHER, Router::new().route("/seen", get(echo))).await;
        let inwards = format!("http://{other}/seen");
        let redirecting = Router::new().route(
            "/",
            get(move || {
                let inwards = inwards.clone();
                async move { Redirect::temporary(&inwards) }
            }),
        );
        let permitted = serve_on(Ipv4Addr::LOCALHOST, redirecting).await;
        let egress = Egress::start_permitting(first_loopback_only).unwrap();

        let response = egress
            .client()
            .get(format!("http://{permitted}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "the redirect's target is refused");
        assert_eq!(response.url().as_str(), format!("http://{other}/seen"));
    }

    #[tokio::test]
    async fn tunnels_reach_permitted_addresses_only() {
        let router = Router::new().route("/seen", get(echo));
        let permitted = serve_on(Ipv4Addr::LOCALHOST, router.clone()).await;
        let other = serve_on(OTHER, router).await;
        let egress = Egress::start_permitting(first_loopback_only).unwrap();
        let proxy = egress.proxy().socket_addrs(|| None).unwrap()[0];

        let mut tunnel = TcpStream::connect(proxy).await.unwrap();
        let connect = format!("CONNECT {permitted} HTTP/1.1\r\nHost: {permitted}\r\n\r\n");
        tunnel.write_all(connect.as_bytes()).await.unwrap();
        let mut established = [0; 39];
        tunnel.read_exact(&mut established).await.unwrap();
        assert_eq!(&established, b"HTTP/1.1 200 Connection established\r\n\r\n");
        // what goes through a tunnel is the client's own business
        let request =
            format!("GET /seen HTTP/1.1\r\nHost: {permitted}\r\nConnection: close\r\n\r\n");
        tunnel.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        tunnel.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("target /seen\n"), "{response}");

        let mut tunnel = TcpStream::connect(proxy).await.unwrap();
        let connect = format!("CONNECT {other} HTTP/1.1\r\n\r\n");
        tunnel.write_all(connect.as_bytes()).await.unwrap();
        let mut response = String::new();
        tunnel.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
    }

    #[tokio::test]
    async fn the_engines_own_machine_and_private_networks_are_refused() {
        let local = serve_on(Ipv4Addr::LOCALHOST, Router::new().route("/seen", get(echo))).await;
        let egress = Egress::start().unwrap();
        for inwards in [
            format!("http://{local}/seen"),
            format!("http://localhost:{}/seen", local.port()),
            "http://10.0.0.1/".to_owned(),
            "http://169.254.169.254/latest/meta-data/".to_owned(),
            "http://[::1]/".to_owned(),
        ] {
            let response = egress.client().get(&inwards).send().await.unwrap();
            assert_eq!(response.status(), 403, "{inwards}");
        }
        let tunnelled = egress
            .client()
            .get(format!("https://localhost:{}/", local.port()))
            .send()
            .await;
        assert!(tunnelled.is_err(), "no tunnel to the engine's own machine");
    }

    /// ffmpeg's own requests, and those its input leads to, go through the proxy it is given.
    #[tokio::test]
    async fn ffmpeg_reaches_only_what_the_proxy_permits() {
        let media =
            std::env::temp_dir().join(format!("entroximer-egress-{}.mp3", std::process::id()));
        let made = tokio::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
            .args(["-c:a", "libmp3lame"])
            .arg(&media)
            .status()
            .await
            .expect("ffmpeg must be installed to run these tests");
        assert!(made.success());
        let tone = tokio::fs::read(&media).await.unwrap();
        tokio::fs::remove_file(&media).await.unwrap();
        let file = move || {
            let tone = tone.clone();
            async move { ([(header::CONTENT_TYPE, "audio/mpeg")], tone) }
        };

        let other = serve_on(OTHER, Router::new().route("/tone.mp3", get(file.clone()))).await;
        let inwards = format!("http://{other}/tone.mp3");
        let playlist =
            format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\n{inwards}\n#EXT-X-ENDLIST\n");
        let redirect = inwards.clone();
        let router = Router::new()
            .route("/tone.mp3", get(file))
            .route(
                "/inwards.m3u8",
                get(move || {
                    let playlist = playlist.clone();
                    async move {
                        (
                            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
                            playlist,
                        )
                    }
                }),
            )
            .route(
                "/moved.mp3",
                get(move || {
                    let redirect = redirect.clone();
                    async move { Redirect::temporary(&redirect) }
                }),
            );
        let permitted = serve_on(Ipv4Addr::LOCALHOST, router).await;
        let egress = Egress::start_permitting(first_loopback_only).unwrap();

        let probe = |input: String, routed: bool| {
            let mut command = tokio::process::Command::new("ffprobe");
            command
                .args(["-v", "error", "-show_entries", "stream=codec_name"])
                .args(["-of", "csv=p=0", "-i", &input]);
            // the environment's exemptions must not count
            command.env("no_proxy", "127.0.0.2,127.0.0.1");
            if routed {
                route_ffmpeg(&mut command, egress.proxy());
            }
            async move {
                let output = command.output().await.unwrap();
                let found = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                (
                    // once a stream; an HLS playlist lists it under its programme too
                    output.status.success() && found.lines().any(|codec| codec == "mp3"),
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                )
            }
        };

        let (plays, stderr) = probe(format!("http://{permitted}/tone.mp3"), true).await;
        assert!(
            plays,
            "a permitted address plays through the proxy: {stderr}"
        );
        for led_inwards in ["moved.mp3", "inwards.m3u8"] {
            let input = format!("http://{permitted}/{led_inwards}");
            let (plays, stderr) = probe(input.clone(), false).await;
            assert!(
                plays,
                "{led_inwards} plays when nothing is in the way: {stderr}"
            );
            let (plays, _) = probe(input, true).await;
            assert!(
                !plays,
                "{led_inwards} leads to an address that is not permitted"
            );
        }
        let (plays, _) = probe(inwards, true).await;
        assert!(!plays);
    }
}
