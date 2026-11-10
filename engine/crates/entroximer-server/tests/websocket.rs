//! The client protocol over a real WebSocket: requests, responses, event ordering, errors,
//! authentication and resuming. Voice is not connected, so nothing reaches Discord.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use clap::Parser;
use entroximer_server::api;
use entroximer_server::config::{MediaConfig, MediaContext, Secret};
use entroximer_server::node::{Node, NodeSettings, TrackLimits};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const PASSWORD: &str = "test password";
const BOT: &str = "1000";
/// Longer than the fixture, so that a queue's next item is prepared right away.
const PRELOAD_LEAD: Duration = Duration::from_secs(120);

fn fixture() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("tone.webm");
        if !path.exists() {
            let status = Command::new("ffmpeg")
                .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
                .args([
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=48000:duration=60",
                ])
                .args(["-ac", "2", "-c:a", "libopus"])
                .arg(&path)
                .status()
                .expect("ffmpeg must be installed to run these tests");
            assert!(status.success());
        }
        path
    })
}

/// A short MP3 with an 800 px cover embedded, as music libraries have them.
fn covered_fixture() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("covered.mp3");
        if !path.exists() {
            let status = Command::new("ffmpeg")
                .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
                .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=3"])
                .args(["-f", "lavfi", "-i", "color=c=red:s=800x800:d=1"])
                .args(["-map", "0:a", "-map", "1:v", "-frames:v", "1"])
                .args([
                    "-c:a",
                    "libmp3lame",
                    "-c:v",
                    "mjpeg",
                    "-disposition:v",
                    "attached_pic",
                ])
                .arg(&path)
                .status()
                .expect("ffmpeg must be installed to run these tests");
            assert!(status.success());
        }
        path
    })
}

/// The width a baseline or progressive JPEG declares.
fn jpeg_width(jpeg: &[u8]) -> Option<u16> {
    let frame = jpeg
        .windows(2)
        .position(|marker| marker == [0xFF, 0xC0] || marker == [0xFF, 0xC2])?;
    let width = jpeg.get(frame + 7..frame + 9)?;
    Some(u16::from_be_bytes([width[0], width[1]]))
}

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    media: MediaConfig,
}

/// songbird's shared scheduler runs on the runtime that first asks for it. Each test has its own
/// runtime, which ends with the test, so the scheduler is made first on one that never does.
fn keep_songbird_scheduler() {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    });
    let _entered = runtime.enter();
    songbird::driver::get_default_scheduler();
}

struct Engine {
    addr: SocketAddr,
    node: Arc<Node>,
    _dirs: tempfile::TempDir,
}

impl Engine {
    async fn start(password: Option<&str>) -> Self {
        Self::start_with(password, &[]).await
    }

    /// An engine with more flags.
    async fn start_with(password: Option<&str>, flags: &[&str]) -> Self {
        keep_songbird_scheduler();
        let dirs = tempfile::tempdir().unwrap();
        let path = |name: &str| dirs.path().join(name).to_string_lossy().into_owned();
        let (data, cache) = (path("data"), path("cache"));
        let mut args = vec![
            "entroximer",
            "--data-dir",
            &data,
            "--cache-dir",
            &cache,
            "--allow-local-files",
        ];
        args.extend(flags);
        let ctx = Arc::new(MediaContext::new(Cli::parse_from(args).media).unwrap());
        let node = Node::new(
            ctx,
            NodeSettings {
                password: password.map(Secret::new),
                player_update_interval: Duration::from_millis(300),
                stuck_after: Duration::from_secs(10),
                resume_timeout: Duration::from_secs(30),
                preload_lead: PRELOAD_LEAD,
                limits: TrackLimits {
                    queue_bytes: 16 << 10,
                    ..TrackLimits::default()
                },
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = api::router(Arc::clone(&node));
        tokio::spawn(async move { axum::serve(listener, router).await });
        Self {
            addr,
            node,
            _dirs: dirs,
        }
    }

    async fn connect(
        &self,
        password: Option<&str>,
        session: Option<&str>,
    ) -> Result<Client, tungstenite::Error> {
        let mut request = format!("ws://{}/v1/ws", self.addr).into_client_request()?;
        let headers = request.headers_mut();
        if let Some(password) = password {
            headers.insert("Authorization", password.parse().unwrap());
        }
        headers.insert("User-Id", BOT.parse().unwrap());
        headers.insert("Client-Name", "tests/1.0".parse().unwrap());
        if let Some(id) = session {
            headers.insert("Session-Id", id.parse().unwrap());
        }
        let (ws, _) = tokio_tungstenite::connect_async(request).await?;
        Ok(Client { ws, next_id: 1 })
    }
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Client {
    async fn call(&mut self, op: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({ "id": id, "op": op });
        if !params.is_null() {
            request["params"] = params;
        }
        self.ws
            .send(Message::text(request.to_string()))
            .await
            .unwrap();
        id
    }

    async fn recv(&mut self) -> Value {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(15), self.ws.next())
                .await
                .expect("no message within 15 s")
                .expect("connection closed")
                .unwrap();
            if let Message::Text(text) = message {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    /// The next message that is not a periodic `playerUpdate` or `stats` event.
    async fn recv_significant(&mut self) -> Value {
        loop {
            let message = self.recv().await;
            if !matches!(message["event"].as_str(), Some("playerUpdate" | "stats")) {
                return message;
            }
        }
    }

    async fn stats(&mut self) -> Value {
        let id = self.call("node.stats", Value::Null).await;
        self.response(id).await["result"].clone()
    }

    async fn response(&mut self, id: u64) -> Value {
        let message = self.recv_significant().await;
        assert_eq!(message["type"], "response", "{message}");
        assert_eq!(message["id"], id);
        message
    }

    /// Makes a request; returns the events that came before its response, and the response.
    async fn exchange(&mut self, op: &str, params: Value) -> (Vec<Value>, Value) {
        let id = self.call(op, params).await;
        let mut events = Vec::new();
        loop {
            let message = self.recv_significant().await;
            if message["type"] == "response" {
                assert_eq!(message["id"], id);
                return (events, message);
            }
            events.push(message);
        }
    }

    /// Significant events up to and including the first `event`.
    async fn events_until(&mut self, event: &str) -> Vec<Value> {
        let mut events = Vec::new();
        loop {
            let message = self.recv_significant().await;
            assert_eq!(message["type"], "event", "{message}");
            let done = message["event"] == event;
            events.push(message);
            if done {
                return events;
            }
        }
    }
}

/// The `type` of each `queueChanged` event's change, and the names of other events.
fn kinds(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e["event"].as_str().unwrap() {
            "queueChanged" => format!("queue {}", e["change"]["type"].as_str().unwrap()),
            other => other.to_owned(),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn load_play_stop_destroy() {
    let engine = Engine::start(Some(PASSWORD)).await;
    let mut client = engine.connect(Some(PASSWORD), None).await.unwrap();
    let ready = client.recv().await;
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["resumed"], false);

    let id = client
        .call("tracks.load", json!({ "query": fixture() }))
        .await;
    let loaded = client.response(id).await["result"].clone();
    assert_eq!(loaded["type"], "track", "{loaded}");
    let track = loaded["track"].clone();
    // an untagged file is named after itself
    assert_eq!(track["info"]["title"], "tone");
    assert_eq!(track["info"]["sourceName"], "local");
    assert_eq!(track["info"]["isSeekable"], true);
    let encoded = track["encoded"].as_str().unwrap();

    let id = client
        .call(
            "player.update",
            json!({ "guildId": "1", "track": { "encoded": encoded, "userData": { "requester": "42" } }, "volume": 50, "clipping": true }),
        )
        .await;
    // the event a request causes arrives before its response
    let started = client.recv_significant().await;
    assert_eq!(started["event"], "trackStart", "{started}");
    assert_eq!(started["guildId"], "1");
    assert_eq!(started["track"]["userData"]["requester"], "42");
    let player = client.response(id).await["result"].clone();
    assert_eq!(player["track"]["info"]["title"], "tone");
    assert_eq!(player["volume"], 50);
    assert_eq!(player["clipping"], true);
    assert_eq!(player["state"]["connected"], false);

    let id = client
        .call("player.update", json!({ "guildId": "1", "track": null }))
        .await;
    let ended = client.recv_significant().await;
    assert_eq!(ended["event"], "trackEnd");
    assert_eq!(ended["reason"], "stopped");
    assert!(client.response(id).await["result"]["track"].is_null());

    let id = client
        .call("player.destroy", json!({ "guildId": "1" }))
        .await;
    assert!(client.response(id).await["result"].is_null());
    let id = client.call("player.get", json!({ "guildId": "1" })).await;
    assert_eq!(client.response(id).await["error"]["code"], "not_found");

    let id = client.call("player.dance", json!({})).await;
    assert_eq!(client.response(id).await["error"]["code"], "bad_request");
    let id = client
        .call("player.update", json!({ "guildId": "1", "volume": 5000 }))
        .await;
    assert_eq!(client.response(id).await["error"]["code"], "bad_request");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_wrong_passwords() {
    let engine = Engine::start(Some(PASSWORD)).await;
    for password in [Some("wrong"), None] {
        match engine.connect(password, None).await {
            Err(tungstenite::Error::Http(response)) => assert_eq!(response.status(), 401),
            Err(e) => panic!("unexpected error: {e}"),
            Ok(_) => panic!("connected with password {password:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_password_every_client_is_accepted() {
    let engine = Engine::start(None).await;
    for password in [None, Some("anything")] {
        let mut client = engine.connect(password, None).await.unwrap();
        assert_eq!(client.recv().await["type"], "ready");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stats_tell_the_caches_limits() {
    let flags = [
        "--memory-total-mb",
        "64",
        "--disk-total-mb",
        "512",
        "--loop-cache-mb",
        "256",
    ];
    let engine = Engine::start_with(None, &flags).await;
    let mut client = engine.connect(None, None).await.unwrap();
    client.recv().await;
    let stats = client.stats().await;
    assert_eq!(stats["cacheMemoryLimitBytes"], 64 << 20);
    assert_eq!(stats["cacheDiskLimitBytes"], 512 << 20);
    assert_eq!(stats["loopCacheLimitBytes"], 256 << 20);
    assert_eq!(stats["loopCacheBytes"], 0);

    // without spill, nothing goes to disk
    let engine = Engine::start_with(None, &["--no-spill", "--loop-cache-mb", "256"]).await;
    let mut client = engine.connect(None, None).await.unwrap();
    client.recv().await;
    let stats = client.stats().await;
    assert_eq!(stats["cacheDiskLimitBytes"], 0);
    assert_eq!(stats["loopCacheLimitBytes"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn players_pause_while_their_client_is_away_and_continue_on_resume() {
    let engine = Engine::start(Some(PASSWORD)).await;
    let mut bot = engine.connect(Some(PASSWORD), None).await.unwrap();
    let session = bot.recv().await["sessionId"].as_str().unwrap().to_owned();
    let id = bot
        .call(
            "player.update",
            json!({ "guildId": "2", "track": { "query": fixture() } }),
        )
        .await;
    assert_eq!(bot.recv_significant().await["event"], "trackStart");
    bot.response(id).await;
    assert_eq!(bot.stats().await["playingPlayers"], 1);
    drop(bot);

    // stats are engine-wide, so another client sees the player pause once the bot is gone
    let mut observer = engine.connect(Some(PASSWORD), None).await.unwrap();
    observer.recv().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let stats = observer.stats().await;
        if stats["players"] == 1 && stats["playingPlayers"] == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the player did not pause: {stats}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // several player updates would have been sent meanwhile; they are not replayed
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let mut bot = engine
        .connect(Some(PASSWORD), Some(&session))
        .await
        .unwrap();
    let ready = bot.recv().await;
    assert_eq!(ready["resumed"], true, "{ready}");
    assert_eq!(ready["sessionId"], session.as_str());
    assert_eq!(ready["missedEvents"], 0);
    let fresh = bot.recv().await;
    assert_eq!(fresh["event"], "playerUpdate", "{fresh}");
    assert_eq!(fresh["guildId"], "2");

    assert_eq!(bot.stats().await["playingPlayers"], 1, "playback continues");
    let id = bot.call("player.get", json!({ "guildId": "2" })).await;
    let player = bot.response(id).await;
    assert_eq!(player["result"]["track"]["info"]["title"], "tone");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_comes_back_takes_its_session_over_from_its_last_connection() {
    let engine = Engine::start(Some(PASSWORD)).await;
    let mut stale = engine.connect(Some(PASSWORD), None).await.unwrap();
    let session = stale.recv().await["sessionId"].as_str().unwrap().to_owned();
    let id = stale
        .call(
            "player.update",
            json!({ "guildId": "2", "track": { "query": fixture() } }),
        )
        .await;
    assert_eq!(stale.recv_significant().await["event"], "trackStart");
    stale.response(id).await;

    // the engine has not found the first connection dead: to it, the client is still there
    let mut bot = engine
        .connect(Some(PASSWORD), Some(&session))
        .await
        .unwrap();
    let ready = bot.recv().await;
    assert_eq!(ready["resumed"], true, "{ready}");
    assert_eq!(ready["sessionId"], session.as_str());
    let fresh = bot.recv().await;
    assert_eq!(fresh["event"], "playerUpdate", "{fresh}");
    let stats = bot.stats().await;
    assert_eq!(stats["players"], 1, "{stats}");
    assert_eq!(stats["playingPlayers"], 1, "the player never paused");

    // the first connection is closed, which changes nothing for the session
    let close = loop {
        let message = tokio::time::timeout(Duration::from_secs(15), stale.ws.next())
            .await
            .expect("the replaced connection stayed open")
            .expect("the replaced connection ended without a close frame")
            .unwrap();
        if let Message::Close(close) = message {
            break close.expect("a close frame with a code");
        }
    };
    assert_eq!(u16::from(close.code), 4002, "{close:?}");
    drop(stale);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(bot.stats().await["playingPlayers"], 1);

    // a session of another bot is not there to take
    let mut request = format!("ws://{}/v1/ws", engine.addr)
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    headers.insert("Authorization", PASSWORD.parse().unwrap());
    headers.insert("User-Id", "2000".parse().unwrap());
    headers.insert("Session-Id", session.parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let mut other = Client { ws, next_id: 1 };
    let ready = other.recv().await;
    assert_eq!(ready["resumed"], false, "{ready}");
    assert_ne!(ready["sessionId"], session.as_str());
    assert_eq!(bot.stats().await["playingPlayers"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn tracks_and_queues_larger_than_the_engine_holds_are_refused() {
    // the test engine's queues hold 16 KiB; the other limits are the defaults
    let engine = Engine::start(None).await;
    let mut client = engine.connect(None, None).await.unwrap();
    client.recv().await;
    let (_, loaded) = client
        .exchange("tracks.load", json!({ "query": fixture() }))
        .await;
    let encoded = loaded["result"]["track"]["encoded"].clone();
    let refusal = |response: &Value| {
        assert_eq!(response["error"]["code"], "bad_request", "{response}");
        response["error"]["message"].as_str().unwrap().to_owned()
    };

    let (_, response) = client
        .exchange("tracks.load", json!({ "query": "x".repeat(5000) }))
        .await;
    assert!(refusal(&response).starts_with("a query may be at most 4096 bytes"));
    let add = |tracks: Value| json!({ "guildId": "4", "tracks": tracks });
    let (_, response) = client
        .exchange(
            "queue.add",
            add(json!([{ "encoded": "A".repeat(17 << 10) }])),
        )
        .await;
    assert!(refusal(&response).starts_with("an encoded track may be at most 16384 bytes"));
    let (_, response) = client
        .exchange(
            "queue.add",
            add(json!([{ "encoded": encoded, "userData": "d".repeat(5000) }])),
        )
        .await;
    assert!(refusal(&response).starts_with("`userData` may be at most 4096 bytes"));
    let (_, response) = client
        .exchange(
            "player.update",
            json!({ "guildId": "4", "track": { "encoded": encoded, "userData": "d".repeat(5000) } }),
        )
        .await;
    assert!(refusal(&response).starts_with("`userData` may be at most 4096 bytes"));

    // three fit in a queue, a fourth does not
    let sized = json!({ "encoded": encoded, "userData": "d".repeat(4000) });
    let (_, response) = client
        .exchange("queue.add", add(json!([sized, sized, sized, sized])))
        .await;
    let message = refusal(&response);
    assert!(
        message.starts_with("a queue holds at most 16384 bytes of tracks"),
        "{message}"
    );
    let (_, response) = client
        .exchange("queue.add", add(json!([sized, sized, sized])))
        .await;
    assert_eq!(
        response["result"]["items"].as_array().unwrap().len(),
        3,
        "{response}"
    );
    // the first of them starts playing, which leaves room for one more, and no more than one
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (_, page) = client
            .exchange("queue.get", json!({ "guildId": "4" }))
            .await;
        if page["result"]["queue"]["length"] == 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "nothing started: {page}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (_, response) = client.exchange("queue.add", add(json!([sized]))).await;
    assert_eq!(response["result"]["queue"]["length"], 3, "{response}");
    let (_, response) = client.exchange("queue.add", add(json!([sized]))).await;
    assert!(refusal(&response).starts_with("a queue holds at most 16384 bytes of tracks"));
    let (_, page) = client
        .exchange("queue.get", json!({ "guildId": "4" }))
        .await;
    assert_eq!(
        page["result"]["queue"]["length"], 3,
        "nothing was added: {page}"
    );

    // room made is room to add to
    let first = page["result"]["items"][0]["id"].clone();
    let (_, response) = client
        .exchange("queue.remove", json!({ "guildId": "4", "ids": [first] }))
        .await;
    assert_eq!(response["result"]["length"], 2, "{response}");
    let (_, response) = client.exchange("queue.add", add(json!([sized]))).await;
    assert_eq!(response["result"]["queue"]["length"], 3, "{response}");
}

#[tokio::test(flavor = "multi_thread")]
async fn queues_over_the_protocol() {
    let engine = Engine::start(None).await;
    let mut client = engine.connect(None, None).await.unwrap();
    client.recv().await;
    let guild = |params: Value| {
        let mut params = params;
        params["guildId"] = json!("3");
        params
    };

    let (_, response) = client.exchange("queue.get", guild(json!({}))).await;
    assert_eq!(
        response["error"]["code"], "not_found",
        "there is no player yet"
    );

    let (_, loaded) = client
        .exchange("tracks.load", json!({ "query": fixture() }))
        .await;
    let encoded = loaded["result"]["track"]["encoded"].clone();
    // a query is queued as a loading item, which it loads into; an encoded track is taken as it
    // is
    let (events, response) = client
        .exchange(
            "queue.add",
            guild(json!({ "tracks": [
                { "query": fixture() },
                { "encoded": encoded },
                { "encoded": encoded, "userData": { "n": 3 } },
            ] })),
        )
        .await;
    assert_eq!(kinds(&events[..1]), ["queue added"]);
    assert_eq!(events[0]["version"], 1);
    let added = &response["result"];
    assert_eq!(added["index"], 0);
    assert_eq!(
        added["queue"],
        json!({ "version": 1, "length": 3, "repeat": "off", "crossfade": 0 })
    );
    let ids: Vec<Value> = added["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].clone())
        .collect();
    assert_eq!(added["items"][2]["track"]["userData"], json!({ "n": 3 }));
    let loading = &added["items"][0];
    assert_eq!(loading["loading"], true);
    assert_eq!(loading["track"]["info"]["sourceName"], "query");
    assert_eq!(
        loading["track"]["info"]["title"],
        fixture().to_str().unwrap()
    );
    assert_eq!(added["items"][1]["loading"], false);

    // with nothing playing, the first item loads, starts and leaves the queue (maybe before the
    // response)
    let mut events = events[1..].to_vec();
    if !events.iter().any(|e| e["event"] == "trackStart") {
        events.extend(client.events_until("trackStart").await);
    }
    assert_eq!(
        kinds(&events),
        ["queue loaded", "queue advanced", "trackStart"]
    );
    let loaded = &events[0]["change"];
    assert_eq!(loaded["id"], ids[0]);
    assert_eq!(loaded["playlist"], Value::Null);
    assert_eq!(
        loaded["items"][0]["id"], ids[0],
        "the track keeps the item's id"
    );
    assert_eq!(loaded["items"][0]["loading"], false);
    assert_eq!(loaded["items"][0]["track"]["info"]["title"], "tone");
    assert_eq!(
        events[1]["change"],
        json!({ "type": "advanced", "id": ids[0] })
    );
    assert_eq!(events[1]["version"], 3);
    assert_eq!(events[2]["itemId"], ids[0]);

    let (_, page) = client
        .exchange("queue.get", guild(json!({ "offset": 1, "limit": 1 })))
        .await;
    let page = &page["result"];
    assert_eq!(page["queue"]["length"], 2);
    assert_eq!(page["offset"], 1);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["id"], ids[2]);

    let (events, response) = client
        .exchange("queue.move", guild(json!({ "id": ids[2], "index": 0 })))
        .await;
    assert_eq!(
        events[0]["change"],
        json!({ "type": "moved", "id": ids[2], "index": 0 })
    );
    assert_eq!(response["result"]["version"], 4);
    let (_, response) = client
        .exchange("queue.move", guild(json!({ "id": "nope", "index": 0 })))
        .await;
    assert_eq!(response["error"]["code"], "not_found");

    let (events, response) = client
        .exchange("queue.configure", guild(json!({ "repeat": "track" })))
        .await;
    assert_eq!(kinds(&events), ["queue repeat"]);
    assert_eq!(response["result"]["repeat"], "track");

    // a crossfade is the client's own setting, so no event tells it
    let (events, response) = client
        .exchange("queue.configure", guild(json!({ "crossfade": 3000 })))
        .await;
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(response["result"]["crossfade"], 3000);
    assert_eq!(response["result"]["repeat"], "track");
    let (_, response) = client
        .exchange(
            "queue.configure",
            guild(json!({ "repeat": "off", "crossfade": 20000 })),
        )
        .await;
    assert_eq!(response["error"]["code"], "bad_request");
    let (_, response) = client.exchange("queue.configure", guild(json!({}))).await;
    assert_eq!(
        (
            &response["result"]["repeat"],
            &response["result"]["crossfade"]
        ),
        (&json!("track"), &json!(3000)),
        "the refused change changed nothing"
    );

    let (events, response) = client
        .exchange("queue.remove", guild(json!({ "ids": [ids[1], "nope"] })))
        .await;
    assert_eq!(
        events[0]["change"],
        json!({ "type": "removed", "ids": [ids[1]] })
    );
    assert_eq!(response["result"]["length"], 1);

    let (_, response) = client
        .exchange("queue.skip", guild(json!({ "toItem": "nope" })))
        .await;
    assert_eq!(response["error"]["code"], "not_found");

    // repeating the track does not stop a skip; the item plays once it is loaded
    let id = client.call("queue.skip", guild(json!({}))).await;
    let (mut events, mut answered) = (Vec::new(), false);
    while !answered || !events.iter().any(|e: &Value| e["event"] == "trackStart") {
        let message = client.recv_significant().await;
        if message["type"] == "response" {
            assert_eq!(message["id"], id);
            answered = true;
        } else {
            events.push(message);
        }
    }
    assert_eq!(kinds(&events), ["trackEnd", "queue advanced", "trackStart"]);
    assert_eq!(events[0]["reason"], "replaced");
    assert_eq!(events[1]["version"], 7);
    assert_eq!(events[2]["itemId"], ids[2]);

    for op in ["queue.clear", "queue.shuffle"] {
        let (events, response) = client.exchange(op, guild(json!({}))).await;
        assert!(events.is_empty(), "nothing to {op}: {events:?}");
        assert_eq!(response["result"]["version"], 7);
    }

    let (events, _) = client.exchange("queue.skip", guild(json!({}))).await;
    assert_eq!(kinds(&events), ["trackEnd", "queueEnd"]);
    assert_eq!(events[0]["reason"], "stopped");
    let (_, player) = client.exchange("player.get", guild(json!({}))).await;
    let player = &player["result"];
    assert!(player["itemId"].is_null());
    assert_eq!(
        player["queue"],
        json!({ "version": 7, "length": 0, "repeat": "track", "crossfade": 3000 })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn presets_build_graphs_that_players_apply_and_extend() {
    let engine = Engine::start(None).await;
    let mut client = engine.connect(None, None).await.unwrap();
    client.recv().await;

    let (_, info) = client.exchange("node.info", Value::Null).await;
    let presets = info["result"]["presets"].as_array().unwrap();
    let bass_boost = presets
        .iter()
        .find(|p| p["name"] == "bass_boost")
        .expect("bass_boost is a preset");
    assert!(bass_boost["graph"]["nodes"].as_array().unwrap().len() > 1);
    assert!(
        presets.iter().any(|p| p["name"] == "nightcore"),
        "{presets:?}"
    );

    let (_, built) = client
        .exchange(
            "presets.build",
            json!({ "name": "nightcore", "params": { "rate": 1.5 } }),
        )
        .await;
    assert_eq!(
        built["result"],
        json!({ "rate": 1.5, "nodes": [], "edges": [] })
    );

    // a preset's graph is the player's graph, ready to change
    let (_, applied) = client
        .exchange(
            "player.update",
            json!({ "guildId": "1", "preset": { "name": "bass_boost" } }),
        )
        .await;
    let mut graph = applied["result"]["graph"].clone();
    let ids: Vec<&str> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"bass") && ids.contains(&"limiter"), "{ids:?}");

    // built upon: a reverb after the preset's nodes
    graph["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "id": "room", "type": "reverb", "params": { "mix": 0.2 } }));
    let (_, extended) = client
        .exchange("player.update", json!({ "guildId": "1", "graph": graph }))
        .await;
    assert_eq!(extended["result"]["graph"], graph);

    // a refused update changes nothing, not even the parts that were valid
    let (_, refused) = client
        .exchange(
            "player.update",
            json!({ "guildId": "1", "volume": 50, "graph": { "nodes": [{ "id": "x", "type": "warp" }] } }),
        )
        .await;
    assert_eq!(refused["error"]["code"], "bad_request", "{refused}");
    let (_, both) = client
        .exchange(
            "player.update",
            json!({ "guildId": "1", "graph": null, "preset": { "name": "nightcore" } }),
        )
        .await;
    assert_eq!(both["error"]["code"], "bad_request");
    let (_, player) = client
        .exchange("player.get", json!({ "guildId": "1" }))
        .await;
    assert_eq!(player["result"]["volume"], 100);
    assert_eq!(player["result"]["graph"], graph);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutting_down_ends_players_and_closes_connections() {
    let engine = Engine::start(None).await;
    let mut client = engine.connect(None, None).await.unwrap();
    client.recv().await;
    let (_, played) = client
        .exchange(
            "player.update",
            json!({ "guildId": "1", "track": { "query": fixture() } }),
        )
        .await;
    assert!(played["result"]["track"].is_object(), "{played}");
    let health = format!("http://{}/health", engine.addr);
    assert_eq!(reqwest::get(&health).await.unwrap().status(), 200);

    engine.node.shutdown().await;

    // the player's end, then a close saying why
    let mut ends = Vec::new();
    let close = loop {
        let message = tokio::time::timeout(Duration::from_secs(5), client.ws.next())
            .await
            .expect("the connection closes")
            .expect("a close frame comes first")
            .unwrap();
        match message {
            Message::Text(text) => {
                let event: Value = serde_json::from_str(&text).unwrap();
                if event["event"] == "trackEnd" {
                    ends.push(event["reason"].clone());
                }
            }
            Message::Close(frame) => break frame.expect("a close frame with a code"),
            _ => {}
        }
    };
    assert_eq!(ends, [json!("cleanup")]);
    assert_eq!(u16::from(close.code), 1001, "going away");
    assert_eq!(close.reason.as_str(), "the engine is shutting down");
    assert!(
        engine.connect(None, None).await.is_err(),
        "no new connections while shutting down"
    );
    assert_eq!(
        reqwest::get(&health).await.unwrap().status(),
        503,
        "orchestrators see the engine going away"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn embedded_covers_are_served_to_authorized_clients() {
    let engine = Engine::start(Some(PASSWORD)).await;
    let mut client = engine.connect(Some(PASSWORD), None).await.unwrap();
    client.recv().await;

    let (_, loaded) = client
        .exchange("tracks.load", json!({ "query": covered_fixture() }))
        .await;
    let artwork = loaded["result"]["track"]["info"]["artworkUrl"]
        .as_str()
        .unwrap_or_else(|| panic!("no artwork: {loaded}"))
        .to_owned();
    assert!(artwork.starts_with("/v1/artwork/"), "{artwork}");

    let http = reqwest::Client::new();
    let url = format!("http://{}{artwork}", engine.addr);
    let cover = http
        .get(&url)
        .header("Authorization", PASSWORD)
        .send()
        .await
        .unwrap();
    assert_eq!(cover.status(), 200);
    assert_eq!(cover.headers()["content-type"], "image/jpeg");
    let jpeg = cover.bytes().await.unwrap();
    assert_eq!(jpeg_width(&jpeg), Some(600), "scaled down from 800 px");
    assert_eq!(http.get(&url).send().await.unwrap().status(), 401);

    let (_, plain) = client
        .exchange("tracks.load", json!({ "query": fixture() }))
        .await;
    assert!(
        plain["result"]["track"]["info"]["artworkUrl"].is_null(),
        "a file without a cover has no artwork"
    );
    let bogus = format!("http://{}/v1/artwork/not-a-key", engine.addr);
    let refused = http
        .get(&bogus)
        .header("Authorization", PASSWORD)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn layers_and_clips_over_the_protocol() {
    let engine = Engine::start(None).await;
    let mut client = engine.connect(None, None).await.unwrap();
    client.recv().await;
    let guild = |mut params: Value| {
        params["guildId"] = json!("3");
        params
    };
    let clip = |layer: &str, id: &str| json!({ "guildId": "3", "layer": layer, "clipId": id, "track": { "query": fixture() } });

    // a refused update changes nothing
    for (params, why) in [
        (
            json!({ "layer": "main", "volume": 50 }),
            "the music's layer",
        ),
        (json!({ "layer": "Bad Name" }), "a bad name"),
        (json!({ "layer": "fx", "volume": 1001 }), "too loud"),
        (json!({ "layer": "fx", "maxClips": 9 }), "too many at once"),
        (
            json!({ "layer": "fx", "duck": { "layers": ["nope"], "volume": 25 } }),
            "ducking a layer that does not exist",
        ),
        (
            json!({ "layer": "fx", "duck": { "layers": ["fx"], "volume": 25 } }),
            "ducking itself",
        ),
        (
            json!({ "layer": "fx", "duck": { "layers": ["main"], "volume": 101 } }),
            "ducking to more than full volume",
        ),
        (
            json!({ "layer": "fx", "graph": { "nodes": [] }, "preset": { "name": "nightcore" } }),
            "a graph and a preset",
        ),
    ] {
        let (_, response) = client.exchange("layer.update", guild(params)).await;
        let code = &response["error"]["code"];
        assert!(
            code == "bad_request" || code == "not_found",
            "{why}: {response}"
        );
    }
    let (_, player) = client.exchange("player.get", guild(json!({}))).await;
    assert_eq!(player["result"]["layers"], json!([]), "no layer was made");

    // a layer ducking the music, and a clip on it, which starts before the response
    let (_, layer) = client
        .exchange(
            "layer.update",
            guild(json!({ "layer": "voice", "volume": 80, "duck": { "layers": ["main"], "volume": 25 } })),
        )
        .await;
    let layer = &layer["result"];
    assert_eq!(
        (
            &layer["mode"],
            &layer["volume"],
            &layer["clipping"],
            &layer["duck"]
        ),
        (
            &json!("queue"),
            &json!(80),
            &json!(false),
            &json!({ "layers": ["main"], "volume": 25, "attack": 50, "release": 500, "hold": 250 })
        )
    );
    let (events, response) = client.exchange("clip.play", clip("voice", "c1")).await;
    assert_eq!(kinds(&events), ["clipStart"]);
    assert_eq!(
        (&events[0]["layer"], &events[0]["clipId"]),
        (&json!("voice"), &json!("c1"))
    );
    assert_eq!(response["result"]["status"], "playing");
    let (_, player) = client.exchange("player.get", guild(json!({}))).await;
    assert!(
        player["result"]["track"].is_null(),
        "the music knows nothing of it"
    );

    // clips sent at once join their layer in the order they were sent
    let ids = [
        client.call("clip.play", clip("voice", "c2")).await,
        client.call("clip.play", clip("voice", "c3")).await,
        client.call("clip.play", clip("voice", "c4")).await,
    ];
    let mut answered = Vec::new();
    while answered.len() < ids.len() {
        let message = client.recv_significant().await;
        assert_eq!(message["type"], "response", "{message}");
        assert_eq!(message["result"]["status"], "waiting", "{message}");
        answered.push(message["id"].as_u64().unwrap());
    }
    let clips = |player: &Value| -> Vec<(String, String)> {
        player["result"]["layers"][0]["clips"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["clipId"].as_str().unwrap().to_owned(),
                    c["status"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let (_, player) = client.exchange("player.get", guild(json!({}))).await;
    let in_order = ["c1", "c2", "c3", "c4"];
    assert_eq!(
        clips(&player)
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        in_order
    );
    assert_eq!(clips(&player)[0].1, "playing");

    // clips the layer cannot take are refused
    let (_, response) = client.exchange("clip.play", clip("voice", "c2")).await;
    assert_eq!(
        response["error"]["code"], "bad_request",
        "the same id twice"
    );
    let mut looping = clip("voice", "c5");
    looping["loop"] = json!(true);
    let (_, response) = client.exchange("clip.play", looping).await;
    assert_eq!(
        response["error"]["code"], "bad_request",
        "a loop on a queue layer"
    );
    let (_, response) = client.exchange("clip.play", clip("main", "c5")).await;
    assert_eq!(
        response["error"]["code"], "bad_request",
        "clips on the music's layer"
    );
    let (_, response) = client
        .exchange(
            "layer.update",
            guild(json!({ "layer": "voice", "mode": "mix" })),
        )
        .await;
    assert_eq!(
        response["error"]["code"], "bad_request",
        "a new mode while clips play"
    );

    // stopping one that waits, then removing the layer with the rest
    let (events, response) = client
        .exchange(
            "clip.stop",
            guild(json!({ "layer": "voice", "clipId": "c3" })),
        )
        .await;
    assert_eq!(kinds(&events), ["clipEnd"]);
    assert_eq!(
        (&events[0]["clipId"], &events[0]["reason"]),
        (&json!("c3"), &json!("stopped"))
    );
    assert_eq!(response["result"]["clips"].as_array().unwrap().len(), 3);
    let (_, response) = client
        .exchange(
            "clip.stop",
            guild(json!({ "layer": "voice", "clipId": "c3" })),
        )
        .await;
    assert_eq!(response["error"]["code"], "not_found");
    let (events, response) = client
        .exchange("layer.remove", guild(json!({ "layer": "voice" })))
        .await;
    assert!(response["result"].is_null(), "{response}");
    let mut ended: Vec<_> = events
        .iter()
        .map(|e| {
            assert_eq!(
                (&e["event"], &e["reason"]),
                (&json!("clipEnd"), &json!("stopped"))
            );
            e["clipId"].as_str().unwrap().to_owned()
        })
        .collect();
    ended.sort();
    assert_eq!(ended, ["c1", "c2", "c4"], "every clip ends once");
    let (_, player) = client.exchange("player.get", guild(json!({}))).await;
    assert_eq!(player["result"]["layers"], json!([]));
}

/// Two seconds of a stereo tone as a WAV file, about 375 KiB.
fn wav() -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut out = std::io::Cursor::new(Vec::new());
    let mut writer = hound::WavWriter::new(&mut out, spec).unwrap();
    for i in 0..2 * 48_000 {
        let sample = ((i as f32 * 440.0 / 48_000.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        writer.write_sample(sample).unwrap();
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
    out.into_inner()
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_are_off_unless_enabled() {
    let engine = Engine::start(None).await;
    let refused = reqwest::Client::new()
        .post(format!("http://{}/v1/uploads", engine.addr))
        .body(wav())
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 404);
    let error: Value = serde_json::from_slice(&refused.bytes().await.unwrap()).unwrap();
    assert_eq!(error["code"], "not_found");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("ENTROXIMER_UPLOADS"),
        "{error}"
    );

    let mut client = engine.connect(None, None).await.unwrap();
    client.recv().await;
    let (_, info) = client.exchange("node.info", Value::Null).await;
    assert!(info["result"]["uploads"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_play_as_tracks_within_the_limits() {
    // room for two uploads of the tone, and no more
    let flags = [
        "--uploads",
        "--upload-max-mb",
        "1",
        "--upload-total-mb",
        "1",
    ];
    let engine = Engine::start_with(Some(PASSWORD), &flags).await;
    let mut client = engine.connect(Some(PASSWORD), None).await.unwrap();
    client.recv().await;
    let http = reqwest::Client::new();
    let url = format!("http://{}/v1/uploads", engine.addr);
    let upload = |body: Vec<u8>, title: Option<&str>| {
        let mut request = http.post(&url).header("Authorization", PASSWORD).body(body);
        if let Some(title) = title {
            request = request.query(&[("title", title)]);
        }
        async move {
            let response = request.send().await.unwrap();
            let status = response.status().as_u16();
            (
                status,
                serde_json::from_slice::<Value>(&response.bytes().await.unwrap()).unwrap(),
            )
        }
    };

    let (_, info) = client.exchange("node.info", Value::Null).await;
    assert_eq!(
        info["result"]["uploads"],
        json!({ "maxBytes": 1 << 20, "keep": 3_600_000 })
    );
    let unauthorized = http.post(&url).body(wav()).send().await.unwrap();
    assert_eq!(unauthorized.status(), 401);
    // refused before the body is read, so only the headers are sent
    let mut stream = TcpStream::connect(engine.addr).await.unwrap();
    let headers = format!(
        "POST /v1/uploads HTTP/1.1\r\nHost: engine\r\nAuthorization: {PASSWORD}\r\nContent-Length: {}\r\n\r\n",
        (1 << 20) + 1
    );
    stream.write_all(headers.as_bytes()).await.unwrap();
    let mut response = vec![0; 1024];
    let read = stream.read(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response[..read]);
    assert!(response.starts_with("HTTP/1.1 413"), "{response}");
    assert!(response.contains("too_large"), "{response}");
    let (status, error) = upload(b"only words".repeat(100), None).await;
    assert_eq!(
        (status, &error["code"]),
        (415, &json!("no_audio")),
        "{error}"
    );

    let (status, doorbell) = upload(wav(), Some("Doorbell")).await;
    assert_eq!(status, 201, "{doorbell}");
    let id = doorbell["id"].as_str().unwrap().to_owned();
    let info = &doorbell["track"]["info"];
    assert_eq!(
        (&info["title"], &info["sourceName"], &info["length"]),
        (&json!("Doorbell"), &json!("upload"), &json!(2000))
    );
    assert_eq!(info["uri"], format!("upload:{id}"));
    assert!(doorbell["expiresAt"].as_u64().unwrap() > 3_600_000);

    // it plays like any other track, and while it plays it is kept
    let guild = |mut params: Value| {
        params["guildId"] = json!("5");
        params
    };
    client
        .exchange(
            "layer.update",
            guild(json!({ "layer": "sfx", "mode": "mix" })),
        )
        .await;
    let (events, response) = client
        .exchange(
            "clip.play",
            guild(json!({ "layer": "sfx", "track": { "encoded": doorbell["track"]["encoded"] }, "loop": true })),
        )
        .await;
    assert_eq!(kinds(&events), ["clipStart"], "{response}");
    let (_, second) = upload(wav(), None).await;
    let (_, third) = upload(wav(), None).await;
    assert_eq!(third["track"]["info"]["title"], "Uploaded audio");
    let load = |query: String| json!({ "query": query });
    let (_, evicted) = client
        .exchange(
            "tracks.load",
            load(format!("upload:{}", second["id"].as_str().unwrap())),
        )
        .await;
    assert_eq!(
        evicted["error"]["code"], "unavailable",
        "the unused upload made room, not the playing one: {evicted}"
    );
    let (_, loaded) = client
        .exchange("tracks.load", load(format!("upload:{id}")))
        .await;
    assert_eq!(loaded["result"]["track"]["info"]["title"], "Doorbell");

    // with every upload playing, there is no room
    let (events, _) = client
        .exchange(
            "clip.play",
            guild(json!({ "layer": "sfx", "track": { "encoded": third["track"]["encoded"] }, "loop": true })),
        )
        .await;
    assert_eq!(kinds(&events), ["clipStart"]);
    let (status, error) = upload(wav(), None).await;
    assert_eq!((status, &error["code"]), (507, &json!("storage_full")));

    // deleting stops it being playable
    let delete = |id: &str| {
        http.delete(format!("{url}/{id}"))
            .header("Authorization", PASSWORD)
            .send()
    };
    assert_eq!(delete(&id).await.unwrap().status(), 204);
    assert_eq!(delete(&id).await.unwrap().status(), 404);
    let (_, gone) = client
        .exchange("tracks.load", load(format!("upload:{id}")))
        .await;
    assert_eq!(gone["error"]["code"], "unavailable", "{gone}");
    let (_, gone) = client
        .exchange(
            "clip.play",
            guild(json!({ "layer": "sfx", "track": { "encoded": doorbell["track"]["encoded"] } })),
        )
        .await;
    assert_eq!(gone["error"]["code"], "unavailable", "{gone}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_files_directory_confines_paths_to_it() {
    let sounds = tempfile::tempdir().unwrap();
    std::fs::copy(fixture(), sounds.path().join("ding.webm")).unwrap();
    let dir = sounds.path().to_str().unwrap();
    let engine = Engine::start_with(None, &["--local-files-dir", dir]).await;
    let mut client = engine.connect(None, None).await.unwrap();
    client.recv().await;

    let (_, loaded) = client
        .exchange("tracks.load", json!({ "query": "ding.webm" }))
        .await;
    assert_eq!(
        loaded["result"]["track"]["info"]["title"], "ding",
        "{loaded}"
    );
    let (_, refused) = client
        .exchange("tracks.load", json!({ "query": fixture() }))
        .await;
    assert_eq!(refused["error"]["code"], "not_found", "{refused}");
}
