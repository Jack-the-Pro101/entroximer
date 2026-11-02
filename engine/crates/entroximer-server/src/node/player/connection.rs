//! The voice connection to Discord: connecting with the details a client sends, and reporting
//! what becomes of it.

use super::*;

impl GuildPlayer {
    /// Checks the voice details a client sent and puts them the way songbird takes them.
    pub(super) fn connection_info(&self, voice: &VoiceState) -> Result<ConnectionInfo, ApiError> {
        let id = |snowflake: Snowflake, what: &str| {
            NonZeroU64::new(snowflake.0).ok_or_else(|| bad_request(format!("{what} must not be 0")))
        };
        Ok(ConnectionInfo {
            channel_id: ChannelId(id(voice.channel_id, "channelId")?),
            endpoint: voice.endpoint.clone(),
            guild_id: GuildId(id(self.guild_id, "guildId")?),
            session_id: voice.session_id.clone(),
            token: voice.token.clone(),
            user_id: UserId(id(self.user_id, "the bot's user ID")?),
        })
    }

    /// Connects to voice with new details; unchanged ones keep the connection as it is.
    ///
    /// Discord answers asynchronously: the outcome arrives as a driver event (see
    /// [`VoiceEvents`]), not in the response. A connection that fails or that Discord closes
    /// forgets its details, so that sending the same ones again retries.
    pub(super) async fn connect(&self, voice: VoiceState, info: ConnectionInfo) {
        {
            let mut state = self.state();
            if state.voice.as_ref() == Some(&voice) {
                return;
            }
            tracing::info!(
                guild_id = %self.guild_id,
                channel_id = %voice.channel_id,
                endpoint = %voice.endpoint,
                session = short(&voice.session_id),
                "connecting to voice"
            );
            state.voice = Some(voice);
            state.connected = false;
        }
        // songbird keeps connecting when the outcome is not awaited
        drop(self.driver.lock().await.connect(info));
    }
}

/// Reports the voice connection's state.
pub(super) struct VoiceEvents(pub(super) Weak<GuildPlayer>);

#[async_trait]
impl songbird::EventHandler for VoiceEvents {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<songbird::Event> {
        let player = self.0.upgrade()?;
        match ctx {
            EventContext::DriverConnect(data) | EventContext::DriverReconnect(data) => {
                tracing::info!(
                    guild_id = %player.guild_id,
                    channel_id = data.channel_id.0.get(),
                    server = data.server,
                    session = short(data.session_id),
                    reconnected = matches!(ctx, EventContext::DriverReconnect(_)),
                    "voice connected"
                );
                player.state().connected = true;
                player.sync_output();
            }
            EventContext::DriverDisconnect(data) => {
                let requested = matches!(data.reason, Some(DisconnectReason::Requested));
                {
                    let mut state = player.state();
                    state.connected = false;
                    // songbird gave up on these details; the same ones sent again retry
                    if state
                        .voice
                        .as_ref()
                        .is_some_and(|v| v.session_id == data.session_id)
                    {
                        state.voice = None;
                    }
                }
                let (code, by_remote) = match data.reason {
                    Some(DisconnectReason::WsClosed(code)) => (code.map(|c| c as u16), true),
                    _ => (None, false),
                };
                let reason = disconnect_reason(data);
                if requested {
                    tracing::debug!(guild_id = %player.guild_id, "left voice");
                } else {
                    // 4006 right after connecting usually means another gateway session of the
                    // bot joined this guild's voice since (e.g. the bot runs twice)
                    tracing::warn!(
                        guild_id = %player.guild_id,
                        code,
                        session = short(data.session_id),
                        "voice connection lost: {reason}"
                    );
                }
                player.emit(Event::VoiceClosed {
                    guild_id: player.guild_id,
                    code,
                    reason,
                    by_remote,
                });
            }
            _ => {}
        }
        None
    }
}

/// Why songbird dropped a voice connection, for people.
fn disconnect_reason(data: &songbird::events::context_data::DisconnectData<'_>) -> String {
    use songbird::events::context_data::DisconnectKind;
    let cause = match data.reason {
        None | Some(DisconnectReason::Requested) => return "left the voice channel".to_owned(),
        Some(DisconnectReason::WsClosed(Some(code))) => {
            format!("Discord closed it with {} ({code:?})", code as u16)
        }
        Some(DisconnectReason::WsClosed(None)) => "Discord closed it".to_owned(),
        Some(DisconnectReason::TimedOut) => "Discord did not answer in time".to_owned(),
        Some(DisconnectReason::Io) => "a network error".to_owned(),
        Some(other) => format!("{other:?}"),
    };
    match data.kind {
        DisconnectKind::Connect => format!("could not connect: {cause}"),
        DisconnectKind::Reconnect => format!("could not reconnect: {cause}"),
        _ => cause,
    }
}

/// The start of a voice session ID: enough to tell sessions apart in logs.
fn short(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}
