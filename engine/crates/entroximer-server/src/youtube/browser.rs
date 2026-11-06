//! Headless Chromium, started for one visit to YouTube with the session's cookies and closed
//! again: it runs for well under a minute, a few times a day.
//!
//! To keep it light, the page only gets what YouTube's scripts need to run. Images, media, fonts,
//! stylesheets and telemetry are failed before they leave the browser. Frames from other sites
//! (Google's sign-in frames among them) and workers are attached to as they start, so that their
//! requests are seen and blocked too.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Url;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::cdp::{Cdp, CdpError, Event};
use super::cookies::{Cookie, CookieJar};

/// Looked for on `PATH` in this order: Debian's headless-only build first, which is what the
/// container image ships.
const CANDIDATES: &[&str] = &[
    "chromium-headless-shell",
    "chromium",
    "chromium-browser",
    "google-chrome-stable",
    "google-chrome",
];

/// Moves the piped stdin and stdout to the descriptors `--remote-debugging-pipe` uses (3 for
/// commands, 4 for answers), then becomes the browser. Doing this in a shell keeps the engine
/// free of `unsafe` descriptor juggling between fork and exec.
const TRAMPOLINE: &str = r#"exec "$0" "$@" 3<&0 4>&1 0</dev/null 1>&2"#;

const FLAGS: &[&str] = &[
    "--remote-debugging-pipe",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-gpu",
    // containers' /dev/shm is 64 MB
    "--disable-dev-shm-usage",
    "--disable-extensions",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-sync",
    "--mute-audio",
    "--autoplay-policy=user-gesture-required",
    "--blink-settings=imagesEnabled=false",
    "--disable-component-extensions-with-background-pages",
    "--disable-features=SpareRendererForSitePerProcess,AudioServiceOutOfProcess,Translate,MediaRouter,OptimizationHints,AutofillServerCommunication",
];

/// What pages may not load: nothing YouTube's scripts need to run and sign in.
const BLOCKED_TYPES: &[&str] = &[
    "Image",
    "Media",
    "Font",
    "Stylesheet",
    "TextTrack",
    "Manifest",
    "Prefetch",
    "Ping",
    "CSPViolationReport",
];
/// Video, thumbnails, icons, ads and analytics, whatever the request's type.
const BLOCKED_HOSTS: &[&str] = &[
    "googlevideo.com",
    "ytimg.com",
    "ggpht.com",
    "fonts.gstatic.com",
    "fonts.googleapis.com",
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "google-analytics.com",
    "googletagmanager.com",
];
/// YouTube's telemetry, and its service worker, which would cost a renderer of its own.
const BLOCKED_PATHS: &[&str] = &[
    "/sw.js",
    "/api/stats/",
    "/youtubei/v1/log_event",
    "/ptracking",
    "/generate_204",
];

/// Chromium closes within this once asked to; after it, it is killed.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub(super) enum BrowserError {
    #[error("cannot start {}", program.display())]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot create the browser profile in {}", dir.display())]
    Profile {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the browser quit{}{}", last_words.as_deref().map(|l| format!(": {l}")).unwrap_or_default(), sandbox_hint(last_words.as_deref()))]
    Quit { last_words: Option<String> },
    #[error(transparent)]
    Cdp(CdpError),
    #[error("cannot open {url}: {error}")]
    Navigation { url: String, error: String },
    #[error("the page did not finish loading within {0:?}")]
    LoadTimeout(Duration),
    #[error("unexpected answer to {0}")]
    Unexpected(&'static str),
}

/// Chromium's sandbox needs user namespaces; some container setups (e.g. Docker's default
/// seccomp profile) do not allow them.
fn sandbox_hint(last_words: Option<&str>) -> &'static str {
    match last_words {
        Some(words) if words.to_ascii_lowercase().contains("sandbox") => {
            " (if the engine runs in a container, set ENTROXIMER_CHROMIUM_NO_SANDBOX=true)"
        }
        _ => "",
    }
}

/// How the browser is started.
#[derive(Clone, Debug)]
pub struct BrowserConfig {
    pub program: PathBuf,
    /// Chromium's sandbox needs user namespaces, which rootless containers usually lack.
    pub no_sandbox: bool,
    /// The proxy everything for YouTube goes through, so the session is always seen from the
    /// same address.
    pub proxy: Option<Url>,
}

/// The browser `configured` names (a path, or a name on `PATH`), else the first one found on
/// `PATH`.
pub fn find_chromium(configured: Option<&str>) -> Option<PathBuf> {
    match configured.map(str::trim).filter(|c| !c.is_empty()) {
        Some(program) if program.contains('/') => {
            Some(PathBuf::from(program)).filter(|p| is_executable(p))
        }
        Some(name) => on_path(name),
        None => CANDIDATES.iter().find_map(|name| on_path(name)),
    }
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// A running browser. Dropping it kills the browser and removes its profile.
pub(super) struct Browser {
    cdp: Arc<Cdp<ChildStdin>>,
    events: Option<mpsc::UnboundedReceiver<Event>>,
    child: Child,
    last_words: Arc<Mutex<Option<String>>>,
    proxy_credentials: Option<(String, String)>,
    // removed after the browser, as fields drop in order
    _profile: tempfile::TempDir,
}

impl Browser {
    /// Starts the browser with a fresh profile in `work_dir`.
    pub(super) async fn launch(
        config: &BrowserConfig,
        work_dir: &Path,
    ) -> Result<Self, BrowserError> {
        let profile = tempfile::Builder::new()
            .prefix("chromium-")
            .tempdir_in(work_dir)
            .map_err(|source| BrowserError::Profile {
                dir: work_dir.to_owned(),
                source,
            })?;
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(TRAMPOLINE)
            .arg(&config.program)
            .args(FLAGS)
            .arg(format!("--user-data-dir={}", profile.path().display()))
            // whatever it would keep in the engine's home (crash reports, certificates) goes
            // with the profile
            .env("HOME", profile.path())
            .env("XDG_CONFIG_HOME", profile.path())
            .env("XDG_CACHE_HOME", profile.path());
        if !is_headless_shell(&config.program) {
            command.arg("--headless");
        }
        if config.no_sandbox {
            command.arg("--no-sandbox");
        }
        if let Some(proxy) = &config.proxy {
            command.arg(format!("--proxy-server={}", proxy_server(proxy)));
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| BrowserError::Spawn {
                program: config.program.clone(),
                source,
            })?;
        let last_words = Arc::new(Mutex::new(None));
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(log_stderr(stderr, Arc::clone(&last_words)));
        }
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            unreachable!("both are piped");
        };
        let (cdp, events) = Cdp::new(stdout, stdin);
        Ok(Self {
            cdp: Arc::new(cdp),
            events: Some(events),
            child,
            last_words,
            proxy_credentials: config.proxy.as_ref().and_then(proxy_credentials),
            _profile: profile,
        })
    }

    /// The browser's user agent, without the `HeadlessChrome` that gives it away.
    pub(super) async fn user_agent(&self) -> Result<String, BrowserError> {
        let version = self.call("Browser.getVersion", json!({}), None).await?;
        let agent = version["userAgent"]
            .as_str()
            .ok_or(BrowserError::Unexpected("Browser.getVersion"))?;
        Ok(agent.replace("HeadlessChrome", "Chrome"))
    }

    /// Replaces the browser's cookies with `jar`'s; returns the cookies it took, which may be
    /// fewer.
    pub(super) async fn set_cookies(&self, jar: &CookieJar) -> Result<CookieJar, BrowserError> {
        let cookies: Vec<Value> = jar.iter().map(to_cdp).collect();
        self.call("Storage.clearCookies", json!({}), None).await?;
        self.call("Storage.setCookies", json!({ "cookies": cookies }), None)
            .await?;
        self.cookies().await
    }

    /// All the browser's cookies.
    pub(super) async fn cookies(&self) -> Result<CookieJar, BrowserError> {
        let result = self.call("Storage.getCookies", json!({}), None).await?;
        let cookies = result["cookies"]
            .as_array()
            .ok_or(BrowserError::Unexpected("Storage.getCookies"))?;
        Ok(jar_from_cdp(cookies))
    }

    /// The page the browser opened with (a new one if it has none), with everything heavy
    /// blocked and `user_agent` sent.
    pub(super) async fn open_page(&mut self, user_agent: &str) -> Result<Page, BrowserError> {
        let targets = self.call("Target.getTargets", json!({}), None).await?;
        let opened = targets["targetInfos"].as_array().and_then(|targets| {
            targets
                .iter()
                .find(|t| t["type"] == "page")
                .and_then(|t| t["targetId"].as_str())
                .map(str::to_owned)
        });
        let target_id = match opened {
            Some(id) => id,
            None => self
                .call("Target.createTarget", json!({ "url": "about:blank" }), None)
                .await?["targetId"]
                .as_str()
                .ok_or(BrowserError::Unexpected("Target.createTarget"))?
                .to_owned(),
        };
        let attached = self
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or(BrowserError::Unexpected("Target.attachToTarget"))?
            .to_owned();
        let events = self
            .events
            .take()
            .ok_or(BrowserError::Unexpected("a second page"))?;
        let (state, observed) = watch::channel(PageState::default());
        let state = Arc::new(state);
        let setup = Setup {
            user_agent: user_agent.to_owned(),
            proxy_credentials: self.proxy_credentials.clone(),
        };
        let page = Page {
            cdp: Arc::clone(&self.cdp),
            session: session.clone(),
            state: Arc::clone(&state),
            observed,
            interceptor: tokio::spawn(intercept(
                Arc::clone(&self.cdp),
                session,
                events,
                state,
                setup.clone(),
            )),
        };
        page.call("Page.enable", json!({}))
            .await
            .map_err(|e| self.explain(e))?;
        for (method, params) in setup.commands() {
            page.call(method, params)
                .await
                .map_err(|e| self.explain(e))?;
        }
        Ok(page)
    }

    /// Asks the browser to quit, then kills it if it lingers.
    pub(super) async fn close(mut self) {
        let _ = self.cdp.call("Browser.close", json!({}), None).await;
        if tokio::time::timeout(CLOSE_TIMEOUT, self.child.wait())
            .await
            .is_err()
        {
            tracing::debug!("the browser did not quit in time; killing it");
            let _ = self.child.kill().await;
        }
    }

    /// Turns a lost connection into what the browser last said, which explains a crash.
    pub(super) fn explain(&self, error: BrowserError) -> BrowserError {
        match error {
            BrowserError::Cdp(CdpError::Closed) => BrowserError::Quit {
                last_words: self
                    .last_words
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            },
            other => other,
        }
    }

    async fn call(
        &self,
        method: &'static str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, BrowserError> {
        self.cdp
            .call(method, params, session)
            .await
            .map_err(|e| self.explain(BrowserError::Cdp(e)))
    }
}

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser")
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

/// What the page has shown so far.
#[derive(Clone, Debug, Default)]
pub(super) struct PageState {
    /// The current document's load event fired.
    pub(super) loaded: bool,
    /// Statuses of the answers to Google's cookie rotation requests.
    pub(super) rotations: Vec<u16>,
    /// Requests failed as too heavy or tracking, by type.
    pub(super) blocked: BTreeMap<String, u32>,
}

/// A page of the browser; its requests are intercepted until it is dropped.
pub(super) struct Page {
    cdp: Arc<Cdp<ChildStdin>>,
    session: String,
    state: Arc<watch::Sender<PageState>>,
    observed: watch::Receiver<PageState>,
    interceptor: JoinHandle<()>,
}

impl Page {
    /// Opens `url` and waits up to `timeout` for its load event.
    pub(super) async fn navigate(
        &mut self,
        url: &Url,
        timeout: Duration,
    ) -> Result<(), BrowserError> {
        self.state.send_modify(|s| s.loaded = false);
        let navigated = self
            .call("Page.navigate", json!({ "url": url.as_str() }))
            .await?;
        if let Some(error) = navigated["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(BrowserError::Navigation {
                url: url.to_string(),
                error: error.to_owned(),
            });
        }
        match tokio::time::timeout(timeout, self.observed.wait_for(|s| s.loaded)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err(BrowserError::Cdp(CdpError::Closed)),
            Err(_) => Err(BrowserError::LoadTimeout(timeout)),
        }
    }

    /// Waits up to `timeout` for Google to answer a cookie rotation request.
    pub(super) async fn wait_for_rotation(&mut self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.observed.wait_for(|s| !s.rotations.is_empty()))
            .await;
    }

    pub(super) fn state(&self) -> PageState {
        self.observed.borrow().clone()
    }

    /// Evaluates `expression` in the page; its value, as JSON.
    pub(super) async fn evaluate(&self, expression: &str) -> Result<Value, BrowserError> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true }),
            )
            .await?;
        if let Some(exception) = result.get("exceptionDetails") {
            let text = exception["exception"]["description"]
                .as_str()
                .or(exception["text"].as_str())
                .unwrap_or("an exception");
            return Err(BrowserError::Navigation {
                url: "the page's script".to_owned(),
                error: text.lines().next().unwrap_or(text).to_owned(),
            });
        }
        Ok(result["result"]["value"].clone())
    }

    async fn call(&self, method: &'static str, params: Value) -> Result<Value, BrowserError> {
        self.cdp
            .call(method, params, Some(&self.session))
            .await
            .map_err(BrowserError::Cdp)
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        self.interceptor.abort();
    }
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// How the page and every frame or worker it starts are set up.
#[derive(Clone, Debug)]
struct Setup {
    user_agent: String,
    proxy_credentials: Option<(String, String)>,
}

impl Setup {
    /// For one target: its requests paused to be decided, its responses reported, the user agent,
    /// and the same for the targets it starts, which wait until they are set up.
    fn commands(&self) -> [(&'static str, Value); 4] {
        [
            ("Network.enable", json!({})),
            (
                "Network.setUserAgentOverride",
                json!({ "userAgent": self.user_agent }),
            ),
            (
                "Fetch.enable",
                json!({
                    "patterns": [{ "urlPattern": "*" }],
                    "handleAuthRequests": self.proxy_credentials.is_some(),
                }),
            ),
            (
                "Target.setAutoAttach",
                json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
            ),
        ]
    }
}

/// Decides every request of the page and the frames and workers it starts, answers the proxy's
/// password prompt, and records what the page shows. Requests are paused until answered, so this
/// runs apart from the commands that wait for the page.
async fn intercept(
    cdp: Arc<Cdp<ChildStdin>>,
    page: String,
    mut events: mpsc::UnboundedReceiver<Event>,
    state: Arc<watch::Sender<PageState>>,
    setup: Setup,
) {
    let mut sessions = BTreeSet::from([page.clone()]);
    while let Some(event) = events.recv().await {
        let Some(session) = event.session_id.filter(|s| sessions.contains(s)) else {
            continue;
        };
        let params = &event.params;
        if event.method == "Target.attachedToTarget" {
            let Some(child) = params["sessionId"].as_str() else {
                continue;
            };
            sessions.insert(child.to_owned());
            // commands run in order, so the target only starts once it is set up; workers
            // refuse some of them, which is fine
            let mut setup_child = setup.commands().to_vec();
            setup_child.push(("Runtime.runIfWaitingForDebugger", json!({})));
            for (method, params) in setup_child {
                if cdp.send(method, params, Some(child)).await.is_err() {
                    return;
                }
            }
            continue;
        }
        let answer = match event.method.as_str() {
            "Fetch.requestPaused" => {
                let id = params["requestId"].clone();
                let kind = params["resourceType"].as_str().unwrap_or_default();
                let url = params["request"]["url"].as_str().unwrap_or_default();
                if is_blocked(kind, url) {
                    // queries may carry tokens
                    let without_query = url.split(['?', '#']).next().unwrap_or_default();
                    tracing::trace!(kind, url = without_query, "blocked");
                    state.send_modify(|s| *s.blocked.entry(kind.to_owned()).or_default() += 1);
                    Some((
                        "Fetch.failRequest",
                        json!({ "requestId": id, "errorReason": "BlockedByClient" }),
                    ))
                } else {
                    Some(("Fetch.continueRequest", json!({ "requestId": id })))
                }
            }
            "Fetch.authRequired" => {
                let id = params["requestId"].clone();
                let source = params["authChallenge"]["source"].as_str();
                let response = match (&setup.proxy_credentials, source) {
                    (Some((username, password)), Some("Proxy")) => json!({
                        "response": "ProvideCredentials",
                        "username": username,
                        "password": password,
                    }),
                    _ => json!({ "response": "CancelAuth" }),
                };
                Some((
                    "Fetch.continueWithAuth",
                    json!({ "requestId": id, "authChallengeResponse": response }),
                ))
            }
            "Page.loadEventFired" if session == page => {
                state.send_modify(|s| s.loaded = true);
                None
            }
            "Target.detachedFromTarget" => {
                if let Some(child) = params["sessionId"].as_str() {
                    sessions.remove(child);
                }
                None
            }
            "Network.responseReceived" => {
                let response = &params["response"];
                let url = response["url"].as_str().unwrap_or_default();
                if is_rotation(url) {
                    let status = response["status"].as_f64().unwrap_or_default() as u16;
                    tracing::debug!(status, "Google answered a cookie rotation");
                    state.send_modify(|s| s.rotations.push(status));
                }
                None
            }
            _ => None,
        };
        if let Some((method, params)) = answer
            && cdp.send(method, params, Some(&session)).await.is_err()
        {
            break;
        }
    }
}

/// Whether a request of `kind` for `url` is failed instead of sent.
pub(super) fn is_blocked(kind: &str, url: &str) -> bool {
    if BLOCKED_TYPES.contains(&kind) {
        return true;
    }
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default();
    let on = |site: &str| host == site || host.ends_with(&format!(".{site}"));
    BLOCKED_HOSTS.iter().any(|site| on(site))
        || on("youtube.com")
            && BLOCKED_PATHS
                .iter()
                .any(|path| url.path().starts_with(path))
        || host == "play.google.com" && url.path() == "/log"
}

/// Google's endpoint that issues fresh `__Secure-*PSIDTS` cookies.
fn is_rotation(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| {
        url.host_str().is_some_and(|h| h.starts_with("accounts."))
            && url.path().ends_with("/RotateCookies")
    })
}

fn is_headless_shell(program: &Path) -> bool {
    program
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains("headless-shell"))
}

/// `--proxy-server` takes no credentials; they are given when the proxy asks.
fn proxy_server(proxy: &Url) -> String {
    let host = proxy.host_str().unwrap_or_default();
    match proxy.port_or_known_default() {
        Some(port) => format!("{}://{host}:{port}", proxy.scheme()),
        None => format!("{}://{host}", proxy.scheme()),
    }
}

fn proxy_credentials(proxy: &Url) -> Option<(String, String)> {
    if proxy.username().is_empty() {
        return None;
    }
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8_lossy()
            .into_owned()
    };
    Some((
        decode(proxy.username()),
        decode(proxy.password().unwrap_or_default()),
    ))
}

/// Logs what the browser prints, and keeps its last line to explain a crash.
async fn log_stderr(stderr: tokio::process::ChildStderr, last_words: Arc<Mutex<Option<String>>>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        tracing::debug!(target: "entroximer_server::youtube::chromium", "{line}");
        *last_words.lock().unwrap_or_else(|e| e.into_inner()) = Some(line.to_owned());
    }
}

/// A cookie as `Storage.setCookies` takes it. A host-only cookie is given by URL: one given a
/// domain would apply to its subdomains too, and `__Host-` cookies must not have one.
fn to_cdp(cookie: &Cookie) -> Value {
    let mut param = json!({
        "name": cookie.name,
        "value": cookie.value,
        "path": cookie.path,
        "secure": cookie.secure,
        "httpOnly": cookie.http_only,
    });
    if cookie.include_subdomains {
        param["domain"] = json!(cookie.domain);
    } else {
        let scheme = if cookie.secure { "https" } else { "http" };
        param["url"] = json!(format!("{scheme}://{}{}", cookie.domain, cookie.path));
    }
    if cookie.expires > 0 {
        param["expires"] = json!(cookie.expires);
    }
    // cookies.txt has no SameSite; Google's third-party cookies need theirs to be sent from
    // YouTube's pages to Google
    if cookie.name.starts_with("__Secure-3P") {
        param["sameSite"] = json!("None");
    }
    param
}

/// A cookie as `Storage.getCookies` reports it, and whether it is partitioned (CHIPS), which
/// cookies.txt cannot express.
fn from_cdp(value: &Value) -> Option<(Cookie, bool)> {
    let domain = value["domain"].as_str()?.to_owned();
    let expires = match value["expires"].as_f64() {
        _ if value["session"].as_bool() == Some(true) => 0,
        Some(e) if e > 0.0 => e.round() as u64,
        _ => 0,
    };
    let cookie = Cookie {
        include_subdomains: domain.starts_with('.'),
        domain,
        path: value["path"].as_str().unwrap_or("/").to_owned(),
        secure: value["secure"].as_bool().unwrap_or_default(),
        http_only: value["httpOnly"].as_bool().unwrap_or_default(),
        expires,
        name: value["name"].as_str()?.to_owned(),
        value: value["value"].as_str().unwrap_or_default().to_owned(),
    };
    Some((
        cookie,
        value.get("partitionKey").is_some_and(|k| !k.is_null()),
    ))
}

/// The browser's cookies as a jar. Where a partitioned and an unpartitioned cookie share a name,
/// the unpartitioned one is kept: cookies.txt can hold only one, and that is the one sent
/// everywhere.
fn jar_from_cdp(values: &[Value]) -> CookieJar {
    let mut cookies: Vec<(Cookie, bool)> = values.iter().filter_map(from_cdp).collect();
    // unpartitioned first, so they win
    cookies.sort_by_key(|(_, partitioned)| *partitioned);
    let mut seen = BTreeSet::new();
    let unique = cookies
        .into_iter()
        .filter(|(c, _)| seen.insert((c.domain.clone(), c.path.clone(), c.name.clone())))
        .map(|(c, _)| c);
    CookieJar::from_cookies(unique.collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::youtube::cookies::tests::SIGNED_IN;

    /// What `Storage.getCookies` reports for cookies set from `to_cdp`'s parameters.
    fn reported(param: &Value) -> Value {
        let mut cookie = param.clone();
        let object = cookie.as_object_mut().unwrap();
        if let Some(url) = object.remove("url") {
            let url = Url::parse(url.as_str().unwrap()).unwrap();
            object.insert("domain".into(), json!(url.host_str().unwrap()));
        }
        let session = !object.contains_key("expires");
        object.insert("session".into(), json!(session));
        object.entry("expires").or_insert(json!(-1));
        cookie
    }

    #[test]
    fn cookies_survive_a_trip_through_the_browser() {
        let mut jar: CookieJar = SIGNED_IN.parse().unwrap();
        jar.retain_sites(&["youtube.com", "google.com"]);
        let seen: Vec<Value> = jar.iter().map(|c| reported(&to_cdp(c))).collect();
        assert_eq!(jar_from_cdp(&seen), jar);

        let host_only = Cookie {
            domain: "accounts.google.com".into(),
            include_subdomains: false,
            path: "/".into(),
            secure: true,
            http_only: true,
            expires: 1_830_000_000,
            name: "__Host-GAPS".into(),
            value: "v".into(),
        };
        let param = to_cdp(&host_only);
        assert_eq!(param["url"], "https://accounts.google.com/");
        assert!(
            param.get("domain").is_none(),
            "__Host- cookies have no domain"
        );
        let back = jar_from_cdp(&[reported(&param)]);
        assert_eq!(back.iter().next(), Some(&host_only));
    }

    #[test]
    fn unpartitioned_cookies_win() {
        let cookie = |value: &str, partition: Value| {
            json!({
                "domain": ".youtube.com", "path": "/", "name": "VISITOR_INFO1_LIVE",
                "value": value, "secure": true, "httpOnly": true, "expires": 1.83e9,
                "session": false, "partitionKey": partition,
            })
        };
        let jar = jar_from_cdp(&[
            cookie(
                "partitioned",
                json!({ "topLevelSite": "https://youtube.com" }),
            ),
            cookie("plain", Value::Null),
        ]);
        assert_eq!(jar.len(), 1);
        assert_eq!(jar.iter().next().unwrap().value, "plain");
    }

    #[test]
    fn heavy_and_tracking_requests_are_blocked() {
        assert!(is_blocked("Media", "https://www.youtube.com/whatever"));
        assert!(is_blocked("Font", "https://www.youtube.com/s/font.woff2"));
        assert!(is_blocked(
            "Fetch",
            "https://rr2---sn-q4flrnes.googlevideo.com/videoplayback?x=1"
        ));
        assert!(is_blocked(
            "Fetch",
            "https://fonts.gstatic.com/s/i/youtube_outline/search/v15/24px.svg"
        ));
        assert!(is_blocked(
            "Fetch",
            "https://www.youtube.com/youtubei/v1/log_event?alt=json"
        ));
        assert!(is_blocked("XHR", "https://play.google.com/log?format=json"));

        assert!(!is_blocked("Document", "https://www.youtube.com/"));
        assert!(!is_blocked(
            "Script",
            "https://www.youtube.com/s/desktop/base.js"
        ));
        assert!(!is_blocked(
            "Fetch",
            "https://www.youtube.com/youtubei/v1/browse"
        ));
        assert!(!is_blocked(
            "XHR",
            "https://accounts.youtube.com/RotateCookies"
        ));
        assert!(!is_blocked("Fetch", "https://www.youtube.com/logout_page"));
    }

    #[test]
    fn rotation_requests_are_recognised() {
        assert!(is_rotation("https://accounts.google.com/RotateCookies"));
        assert!(is_rotation("https://accounts.youtube.com/RotateCookies"));
        assert!(!is_rotation(
            "https://accounts.youtube.com/RotateCookiesPage?origin=x"
        ));
        assert!(!is_rotation("https://www.youtube.com/RotateCookies"));
    }

    #[test]
    fn proxies_lose_their_credentials_to_a_prompt() {
        let proxy = Url::parse("http://me:p%40ss@proxy.example:3128").unwrap();
        assert_eq!(proxy_server(&proxy), "http://proxy.example:3128");
        assert_eq!(
            proxy_credentials(&proxy),
            Some(("me".to_owned(), "p@ss".to_owned()))
        );
        assert_eq!(
            proxy_credentials(&Url::parse("http://proxy.example:3128").unwrap()),
            None
        );
    }

    #[test]
    fn browsers_are_found_by_name_or_path() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("chromium");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        assert_eq!(find_chromium(program.to_str()), None, "not executable");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_chromium(program.to_str()), Some(program.clone()));
        assert_eq!(find_chromium(Some("/nonexistent/chromium")), None);
        assert!(is_headless_shell(Path::new(
            "/usr/bin/chromium-headless-shell"
        )));
        assert!(!is_headless_shell(&program));
    }
}
