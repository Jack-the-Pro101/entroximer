//! The client WebSocket (`/v1/ws`).
//!
//! Headers: `Authorization` (the engine's password, if it has one), `User-Id` (the bot's Discord ID),
//! optionally `Client-Name` (`name/version`, for logs) and `Session-Id` (to resume).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use entroximer_core::error::display_chain;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use super::protocol::{
    ApiError, Call, ErrorCode, Outcome, Request, Response as Reply, ServerMessage, Snowflake,
};
use super::{Authorized, HttpError};
use crate::node::player::{ClipTurn, GuildPlayer};
use crate::node::session::Outbox;
use crate::node::{Node, Session, tracks};
use crate::view;

/// Queue items `queue.get` returns unless asked otherwise, and at most.
const QUEUE_PAGE: u32 = 100;
const MAX_QUEUE_PAGE: u32 = 1000;
/// Largest message a client may send: room for adding a long playlist of encoded tracks.
const MAX_MESSAGE_BYTES: usize = 4 << 20;

/// How often the engine pings clients. Clients answer pings by themselves (WebSocket
/// libraries do), so a connection that stays silent this long is gone even if TCP has not
/// noticed, e.g. after the client's network dropped. Its players then pause as for any
/// disconnect.
const PING_INTERVAL: Duration = Duration::from_secs(20);
const SILENCE_LIMIT: Duration = Duration::from_secs(60);
/// How long one message may take to go out. A client that takes nothing for this long has
/// stopped reading, even if it still sends: its connection is closed, since what the engine
/// has for it would otherwise queue up without end.
const WRITE_LIMIT: Duration = SILENCE_LIMIT;
/// How long answers to requests still running may take to go out once the client is gone.
const WRITER_GRACE: Duration = Duration::from_secs(10);
/// Requests of one connection that are in flight at once: running, or answered with the answer
/// still to go out. Beyond this the connection's next requests are read as earlier ones' answers
/// are written, so that a client firing off thousands, or one slow to take the answers, has the
/// engine hold no more than this many.
const MAX_IN_FLIGHT: usize = 256;
/// The close code of a connection whose session another connection resumed.
const CLOSE_REPLACED: u16 = 4002;

/// A message on its way to the client.
struct Outgoing {
    message: ServerMessage,
    /// A response's place among the requests in flight, given up once it has gone out.
    slot: Option<OwnedSemaphorePermit>,
}

/// The session's way into a connection: its events queue up with the responses, in order.
struct Events {
    out: mpsc::UnboundedSender<Outgoing>,
    /// Cancelled once another connection has taken the session over.
    replaced: CancellationToken,
}

impl Outbox for Events {
    fn send(&self, message: ServerMessage) -> Result<(), ServerMessage> {
        let event = Outgoing {
            message,
            slot: None,
        };
        self.out.send(event).map_err(|refused| refused.0.message)
    }

    fn replaced(&self) {
        self.replaced.cancel();
    }
}

pub(super) async fn upgrade(
    _: Authorized,
    State(node): State<Arc<Node>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, HttpError> {
    if node.is_closing() {
        return Err(HttpError::unavailable("the engine is shutting down"));
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let user_id = header("User-Id")
        .and_then(|v| v.parse().ok())
        .map(Snowflake)
        .ok_or_else(|| HttpError::bad_request("the User-Id header must be the bot's Discord ID"))?;
    let client = header("Client-Name").unwrap_or("unknown").to_owned();
    let resume = header("Session-Id").map(str::to_owned);
    Ok(ws
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| {
            let tracked = Arc::clone(&node);
            tracked.track_connection(connection(node, socket, user_id, client, resume))
        }))
}

async fn connection(
    node: Arc<Node>,
    socket: WebSocket,
    user_id: Snowflake,
    client: String,
    resume: Option<String>,
) {
    let (session, resumed) = node.open_session(user_id, resume.as_deref());
    tracing::info!(session = %session.id, %client, resumed, "client connected");

    let (out, outgoing) = mpsc::unbounded_channel::<Outgoing>();
    let replaced = CancellationToken::new();
    let events = Events {
        out: out.clone(),
        replaced: replaced.clone(),
    };
    let connection = session.attach(events, resumed);
    let (sink, mut incoming) = socket.split();
    let mut writer = tokio::spawn(write(Arc::clone(&node), sink, outgoing, replaced.clone()));
    let mut writing = true;
    let in_flight = Arc::new(Semaphore::new(MAX_IN_FLIGHT));

    loop {
        let next = tokio::select! {
            // in this order: a writer that ended because the engine shuts down is no lost client
            biased;
            () = node.closed() => break,
            // the client found this connection dead before the engine did, and came back
            () = replaced.cancelled() => {
                tracing::info!(session = %session.id, %client, "the session continues on a newer connection");
                break;
            }
            // nothing reaches the client any more, so there is nobody to take requests from
            _ = &mut writer => {
                writing = false;
                tracing::info!(session = %session.id, %client, "client is gone or stopped reading");
                break;
            }
            next = tokio::time::timeout(SILENCE_LIMIT, incoming.next()) => next,
        };
        let message = match next {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(_)) | None) => break,
            Err(_) => {
                tracing::info!(session = %session.id, %client, "client stopped answering pings");
                break;
            }
        };
        match message {
            Message::Text(text) => {
                let Some((id, call)) = parse(&session, &text) else {
                    continue;
                };
                // a clip takes its turn on its layer as its request arrives, before it loads
                let turn = match &call {
                    Ok(Call::PlayClip(clip)) => Some(session.clip_turn(clip.guild_id, &clip.layer)),
                    _ => None,
                };
                let permit = tokio::select! {
                    permit = Arc::clone(&in_flight).acquire_owned() => {
                        permit.expect("the semaphore is never closed")
                    }
                    () = node.closed() => break,
                };
                // each request runs on its own, so a slow load never holds up a pause
                tokio::spawn(handle(
                    Arc::clone(&node),
                    Arc::clone(&session),
                    out.clone(),
                    Incoming { id, call, turn },
                    permit,
                ));
            }
            Message::Binary(_) => {
                tracing::debug!(session = %session.id, "ignoring a binary message");
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    node.connection_closed(&session, connection);
    drop(out);
    // in-flight requests may still answer; the writer ends with their senders, unless a dead
    // connection keeps it from sending
    if writing {
        let stop_writing = writer.abort_handle();
        if tokio::time::timeout(WRITER_GRACE, writer).await.is_err() {
            stop_writing.abort();
        }
    }
    tracing::info!(session = %session.id, %client, "client disconnected");
}

/// Sends the session's messages, and pings, until nothing can send any more, then closes the
/// connection: normally, as "going away" when the engine shuts down, after the messages already
/// queued (such as the players' last events), or as replaced. It also ends once the client
/// takes no more messages (see [`WRITE_LIMIT`]).
async fn write(
    node: Arc<Node>,
    mut sink: SplitSink<WebSocket, Message>,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
    replaced: CancellationToken,
) {
    let text = |message: &ServerMessage| {
        Message::Text(
            serde_json::to_string(message)
                .expect("server messages serialize")
                .into(),
        )
    };
    let mut pings = tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
    pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let (message, slot) = tokio::select! {
            biased;
            outgoing = outgoing.recv() => match outgoing {
                Some(Outgoing { message, slot }) => (text(&message), slot),
                None => break,
            },
            () = node.closed() => {
                while let Ok(Outgoing { message, .. }) = outgoing.try_recv() {
                    if !send(&mut sink, text(&message)).await {
                        return;
                    }
                }
                break;
            }
            _ = pings.tick() => (Message::Ping(Bytes::new()), None),
        };
        if !send(&mut sink, message).await {
            return;
        }
        // the response is out: its request makes room for the connection's next one
        drop(slot);
    }
    let close = if node.is_closing() {
        CloseFrame {
            code: close_code::AWAY,
            reason: "the engine is shutting down".into(),
        }
    } else if replaced.is_cancelled() {
        CloseFrame {
            code: CLOSE_REPLACED,
            reason: "the session continues on a newer connection".into(),
        }
    } else {
        CloseFrame {
            code: close_code::NORMAL,
            reason: "".into(),
        }
    };
    // fails harmlessly when the client closed first
    send(&mut sink, Message::Close(Some(close))).await;
}

/// Sends one message; `false` if the client is gone, or does not take it within [`WRITE_LIMIT`].
async fn send(sink: &mut SplitSink<WebSocket, Message>, message: Message) -> bool {
    matches!(
        tokio::time::timeout(WRITE_LIMIT, sink.send(message)).await,
        Ok(Ok(()))
    )
}

/// A request's id and what it asks for; `None` if it has no id to answer.
fn parse(session: &Session, text: &str) -> Option<(u64, Result<Call, ApiError>)> {
    let request: Request = match serde_json::from_str(text) {
        Ok(request) => request,
        Err(e) => {
            tracing::warn!(session = %session.id, "unreadable request (it needs a numeric id): {e}");
            return None;
        }
    };
    let call = serde_json::from_value::<Call>(request.call)
        .map_err(|e| ApiError::new(ErrorCode::BadRequest, e.to_string()));
    Some((request.id, call))
}

/// A request as read from the connection.
struct Incoming {
    id: u64,
    call: Result<Call, ApiError>,
    /// A `clip.play`'s turn on its layer.
    turn: Option<ClipTurn>,
}

/// Runs a request and queues its response, which keeps the request's `slot` until it is written.
async fn handle(
    node: Arc<Node>,
    session: Arc<Session>,
    out: mpsc::UnboundedSender<Outgoing>,
    request: Incoming,
    slot: OwnedSemaphorePermit,
) {
    let Incoming { id, call, turn } = request;
    let outcome = match call {
        Ok(call) => dispatch(&node, &session, call, turn).await,
        Err(error) => Err(error),
    };
    let outcome = match outcome {
        Ok(result) => Outcome::Ok { result },
        Err(error) => Outcome::Err { error },
    };
    // a closed connection has nobody to answer
    let _ = out.send(Outgoing {
        message: ServerMessage::Response(Reply { id, outcome }),
        slot: Some(slot),
    });
}

async fn dispatch(
    node: &Arc<Node>,
    session: &Arc<Session>,
    call: Call,
    turn: Option<ClipTurn>,
) -> Result<Value, ApiError> {
    match call {
        Call::LoadTrack(params) => {
            node.settings.limits.check_query(&params.query)?;
            to_value(node.tracks.load(&node.ctx, &params.query).await?)
        }
        Call::SearchTracks(params) => {
            node.settings.limits.check_query(&params.query)?;
            let found = node
                .tracks
                .search(
                    &node.ctx,
                    params.prefix.as_deref(),
                    &params.query,
                    params.limit,
                )
                .await?;
            to_value(found)
        }
        Call::DecodeTracks(params) => to_value(
            params
                .encoded
                .iter()
                .map(|encoded| tracks::decode_track(encoded))
                .collect::<Result<Vec<_>, ApiError>>()?,
        ),
        Call::BuildPreset(selection) => to_value(
            node.presets
                .build(&selection)
                .map_err(|e| ApiError::new(ErrorCode::BadRequest, display_chain(&e)))?,
        ),
        Call::GetPlayer(params) => to_value(player(session, params.guild_id)?.info()),
        Call::ViewPlayer(params) => to_value(view::player_view(
            &player(session, params.guild_id)?.info(),
            params.collapse.as_deref().unwrap_or_default(),
        )),
        Call::UpdatePlayer(update) => {
            let player = session.player_or_create(node, update.guild_id);
            to_value(player.update(node, *update).await?)
        }
        Call::DestroyPlayer(params) => {
            if session.destroy_player(params.guild_id).await {
                Ok(Value::Null)
            } else {
                Err(no_player(params.guild_id))
            }
        }
        Call::GetQueue(params) => {
            let offset = params.offset.unwrap_or(0) as usize;
            let limit = params.limit.unwrap_or(QUEUE_PAGE).min(MAX_QUEUE_PAGE) as usize;
            to_value(player(session, params.guild_id)?.queue_page(offset, limit))
        }
        Call::AddToQueue(params) => {
            // all or nothing: an invalid track adds none; queries load once they are queued
            let items = params
                .tracks
                .iter()
                .map(|selector| node.queue_item(selector))
                .collect::<Result<Vec<_>, ApiError>>()?;
            let player = session.player_or_create(node, params.guild_id);
            to_value(player.queue_add(items, params.index).await?)
        }
        Call::RemoveFromQueue(params) => to_value(
            player(session, params.guild_id)?
                .queue_remove(&params.ids)
                .await?,
        ),
        Call::MoveInQueue(params) => to_value(
            player(session, params.guild_id)?
                .queue_move(&params.id, params.index)
                .await?,
        ),
        Call::ClearQueue(params) => {
            to_value(player(session, params.guild_id)?.queue_clear().await?)
        }
        Call::ShuffleQueue(params) => {
            to_value(player(session, params.guild_id)?.queue_shuffle().await?)
        }
        Call::Skip(params) => to_value(
            player(session, params.guild_id)?
                .skip(params.to_item.as_deref())
                .await?,
        ),
        Call::ConfigureQueue(params) => to_value(
            player(session, params.guild_id)?
                .configure_queue(params.repeat, params.crossfade)
                .await?,
        ),
        Call::UpdateItem(params) => to_value(
            player(session, params.guild_id)?
                .update_item(&params)
                .await?,
        ),
        Call::UpdateLayer(update) => {
            let player = session.player_or_create(node, update.guild_id);
            to_value(player.layer_update(node, *update).await?)
        }
        Call::RemoveLayer(params) => {
            player(session, params.guild_id)?
                .layer_remove(&params.layer)
                .await?;
            Ok(Value::Null)
        }
        Call::PlayClip(clip) => {
            let player = session.player_or_create(node, clip.guild_id);
            to_value(player.clip_play(node, *clip, turn).await?)
        }
        Call::StopClips(params) => {
            to_value(player(session, params.guild_id)?.clip_stop(params).await?)
        }
        Call::ConfigureSession(settings) => to_value(session.configure(settings)),
        Call::NodeInfo => to_value(node.info().await),
        Call::Stats => to_value(node.stats()),
    }
}

fn player(session: &Session, guild_id: Snowflake) -> Result<Arc<GuildPlayer>, ApiError> {
    session.player(guild_id).ok_or_else(|| no_player(guild_id))
}

fn no_player(guild_id: Snowflake) -> ApiError {
    ApiError::new(
        ErrorCode::NotFound,
        format!("there is no player for guild {guild_id}"),
    )
}

fn to_value(value: impl Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| ApiError::new(ErrorCode::Internal, e.to_string()))
}
