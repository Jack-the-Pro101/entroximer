//! The client WebSocket (`/v1/ws`).
//!
//! Headers: `Authorization` (the engine's password, if it has one), `User-Id` (the bot's Discord ID),
//! optionally `Client-Name` (`name/version`, for logs) and `Session-Id` (to resume).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use entroximer_core::error::display_chain;
use futures_util::future::try_join_all;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};

use super::protocol::{
    ApiError, Call, ErrorCode, Outcome, Request, Response as Reply, ServerMessage, Snowflake,
};
use super::{Authorized, HttpError};
use crate::node::player::GuildPlayer;
use crate::node::{Node, Session, tracks};

/// Queue items `queue.get` returns unless asked otherwise, and at most.
const QUEUE_PAGE: u32 = 100;
const MAX_QUEUE_PAGE: u32 = 1000;

/// How often the engine pings clients. Clients answer pings by themselves (WebSocket
/// libraries do), so a connection that stays silent this long is gone even if TCP has not
/// noticed, e.g. after the client's network dropped. Its players then pause as for any
/// disconnect.
const PING_INTERVAL: Duration = Duration::from_secs(20);
const SILENCE_LIMIT: Duration = Duration::from_secs(60);
/// How long answers to requests still running may take to go out once the client is gone.
const WRITER_GRACE: Duration = Duration::from_secs(10);

pub(super) async fn upgrade(
    _: Authorized,
    State(node): State<Arc<Node>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, HttpError> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let user_id = header("User-Id")
        .and_then(|v| v.parse().ok())
        .map(Snowflake)
        .ok_or_else(|| HttpError::bad_request("the User-Id header must be the bot's Discord ID"))?;
    let client = header("Client-Name").unwrap_or("unknown").to_owned();
    let resume = header("Session-Id").map(str::to_owned);
    Ok(ws.on_upgrade(move |socket| connection(node, socket, user_id, client, resume)))
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

    let (out, mut outgoing) = mpsc::unbounded_channel::<ServerMessage>();
    let connection = session.attach(out.clone(), resumed);
    let (mut sink, mut incoming) = socket.split();
    let writer = tokio::spawn(async move {
        let mut pings = tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
        pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let message = tokio::select! {
                message = outgoing.recv() => match message {
                    Some(message) => Message::Text(
                        serde_json::to_string(&message)
                            .expect("server messages serialize")
                            .into(),
                    ),
                    None => break,
                },
                _ = pings.tick() => Message::Ping(Bytes::new()),
            };
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });

    loop {
        let message = match tokio::time::timeout(SILENCE_LIMIT, incoming.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(_)) | None) => break,
            Err(_) => {
                tracing::info!(session = %session.id, %client, "client stopped answering pings");
                break;
            }
        };
        match message {
            Message::Text(text) => {
                // each request runs on its own, so a slow load never holds up a pause
                tokio::spawn(handle(
                    Arc::clone(&node),
                    Arc::clone(&session),
                    out.clone(),
                    text.to_string(),
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
    let stop_writing = writer.abort_handle();
    if tokio::time::timeout(WRITER_GRACE, writer).await.is_err() {
        stop_writing.abort();
    }
    tracing::info!(session = %session.id, %client, "client disconnected");
}

async fn handle(
    node: Arc<Node>,
    session: Arc<Session>,
    out: mpsc::UnboundedSender<ServerMessage>,
    text: String,
) {
    let request: Request = match serde_json::from_str(&text) {
        Ok(request) => request,
        Err(e) => {
            tracing::warn!(session = %session.id, "unreadable request (it needs a numeric id): {e}");
            return;
        }
    };
    let outcome = match serde_json::from_value::<Call>(request.call) {
        Ok(call) => dispatch(&node, &session, call).await,
        Err(e) => Err(ApiError::new(ErrorCode::BadRequest, e.to_string())),
    };
    let outcome = match outcome {
        Ok(result) => Outcome::Ok { result },
        Err(error) => Outcome::Err { error },
    };
    // a closed connection has nobody to answer
    let _ = out.send(ServerMessage::Response(Reply {
        id: request.id,
        outcome,
    }));
}

async fn dispatch(node: &Arc<Node>, session: &Arc<Session>, call: Call) -> Result<Value, ApiError> {
    match call {
        Call::LoadTrack(params) => to_value(node.tracks.load(&node.ctx, &params.query).await?),
        Call::SearchTracks(params) => to_value(
            node.tracks
                .search(
                    &node.ctx,
                    params.prefix.as_deref(),
                    &params.query,
                    params.limit,
                )
                .await?,
        ),
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
            // all or nothing: a query that finds nothing adds none of the tracks
            let tracks = try_join_all(params.tracks.iter().map(|s| node.queue_tracks(s))).await?;
            let player = session.player_or_create(node, params.guild_id);
            to_value(
                player
                    .queue_add(tracks.into_iter().flatten().collect(), params.index)
                    .await,
            )
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
        Call::ConfigureQueue(params) => {
            let player = player(session, params.guild_id)?;
            match params.repeat {
                Some(repeat) => to_value(player.set_repeat(repeat).await?),
                None => to_value(player.queue_state()),
            }
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
