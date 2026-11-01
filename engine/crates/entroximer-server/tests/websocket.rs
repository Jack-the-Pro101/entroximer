//! The client protocol over a real WebSocket: requests, responses, event ordering, errors,
//! authentication and resuming. Voice is not connected, so nothing reaches Discord.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use clap::Parser;
use entroximer_server::api;
use entroximer_server::config::{MediaConfig, MediaContext};
use entroximer_server::node::{Node, NodeSettings};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
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

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    media: MediaConfig,
}

struct Engine {
    addr: SocketAddr,
    _dirs: tempfile::TempDir,
}

impl Engine {
    async fn start(password: Option<&str>) -> Self {
        let dirs = tempfile::tempdir().unwrap();
        let path = |name: &str| dirs.path().join(name).to_string_lossy().into_owned();
        let args = [
            "entroximer",
            "--data-dir",
            &path("data"),
            "--cache-dir",
            &path("cache"),
            "--allow-local-files",
        ];
        let ctx = Arc::new(MediaContext::new(Cli::parse_from(args).media).unwrap());
        let node = Node::new(
            ctx,
            NodeSettings {
                password: password.map(str::to_owned),
                player_update_interval: Duration::from_millis(300),
                stuck_after: Duration::from_secs(10),
                resume_timeout: Duration::from_secs(30),
                preload_lead: PRELOAD_LEAD,
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, api::router(node)).await });
        Self { addr, _dirs: dirs }
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
            json!({ "guildId": "1", "track": { "encoded": encoded, "userData": { "requester": "42" } }, "volume": 50 }),
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
    // a query adds what it loads; an encoded track is taken as it is
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
        json!({ "version": 1, "length": 3, "repeat": "off" })
    );
    let ids: Vec<Value> = added["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].clone())
        .collect();
    assert_eq!(added["items"][2]["track"]["userData"], json!({ "n": 3 }));

    // with nothing playing, the first item starts (maybe before the response, as it is cached)
    // and leaves the queue
    let mut events = events[1..].to_vec();
    if events.is_empty() {
        events = client.events_until("trackStart").await;
    }
    assert_eq!(kinds(&events), ["queue advanced", "trackStart"]);
    assert_eq!(
        events[0]["change"],
        json!({ "type": "advanced", "id": ids[0] })
    );
    assert_eq!(events[0]["version"], 2);
    assert_eq!(events[1]["itemId"], ids[0]);

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
    assert_eq!(response["result"]["version"], 3);
    let (_, response) = client
        .exchange("queue.move", guild(json!({ "id": "nope", "index": 0 })))
        .await;
    assert_eq!(response["error"]["code"], "not_found");

    let (events, response) = client
        .exchange("queue.configure", guild(json!({ "repeat": "track" })))
        .await;
    assert_eq!(kinds(&events), ["queue repeat"]);
    assert_eq!(response["result"]["repeat"], "track");

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
    assert_eq!(events[1]["version"], 6);
    assert_eq!(events[2]["itemId"], ids[2]);

    for op in ["queue.clear", "queue.shuffle"] {
        let (events, response) = client.exchange(op, guild(json!({}))).await;
        assert!(events.is_empty(), "nothing to {op}: {events:?}");
        assert_eq!(response["result"]["version"], 6);
    }

    let (events, _) = client.exchange("queue.skip", guild(json!({}))).await;
    assert_eq!(kinds(&events), ["trackEnd", "queueEnd"]);
    assert_eq!(events[0]["reason"], "stopped");
    let (_, player) = client.exchange("player.get", guild(json!({}))).await;
    let player = &player["result"];
    assert!(player["itemId"].is_null());
    assert_eq!(
        player["queue"],
        json!({ "version": 6, "length": 0, "repeat": "track" })
    );
}
