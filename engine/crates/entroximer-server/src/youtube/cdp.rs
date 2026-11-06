//! The Chrome DevTools Protocol, as Chromium speaks it over `--remote-debugging-pipe`: JSON
//! messages, each ending in a NUL byte. A pipe rather than a debugging port, so that no other
//! process on the machine can connect to the browser while it holds the session's cookies.
//!
//! Commands are answered by `id`; everything else Chromium sends is an event, delivered in order
//! on one channel. Messages carry account cookies: nothing here logs their contents.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

/// Long enough for a busy browser on a slow host; navigation is waited for separately.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub(super) enum CdpError {
    #[error("the browser closed the connection")]
    Closed,
    #[error("the browser did not answer {method} within {after:?}")]
    Timeout {
        method: &'static str,
        after: Duration,
    },
    #[error("{method} failed: {message}")]
    Protocol {
        method: &'static str,
        message: String,
    },
    #[error("cannot send to the browser")]
    Io(#[source] io::Error),
}

/// Something Chromium reported without being asked.
#[derive(Debug)]
pub(super) struct Event {
    pub(super) method: String,
    pub(super) params: Value,
    /// The page (or other target) it is about; `None` for the browser itself.
    pub(super) session_id: Option<String>,
}

type Reply = Result<Value, String>;

#[derive(Default)]
struct Pending {
    calls: HashMap<u64, oneshot::Sender<Reply>>,
    closed: bool,
}

pub(super) struct Cdp<W> {
    writer: tokio::sync::Mutex<W>,
    pending: Arc<Mutex<Pending>>,
    next_id: AtomicU64,
    reader: AbortHandle,
}

impl<W: AsyncWrite + Unpin> Cdp<W> {
    /// Talks to the browser over `reader` and `writer`; its events arrive on the receiver.
    pub(super) fn new<R>(reader: R, writer: W) -> (Self, mpsc::UnboundedReceiver<Event>)
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (events, received) = mpsc::unbounded_channel();
        let reader = tokio::spawn(read_messages(reader, Arc::clone(&pending), events));
        let cdp = Self {
            writer: tokio::sync::Mutex::new(writer),
            pending,
            next_id: AtomicU64::new(1),
            reader: reader.abort_handle(),
        };
        (cdp, received)
    }

    /// Sends a command and waits for its result. `session` addresses a page attached with
    /// `Target.attachToTarget`; `None` addresses the browser.
    pub(super) async fn call(
        &self,
        method: &'static str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, CdpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        {
            let mut pending = self.pending();
            if pending.closed {
                return Err(CdpError::Closed);
            }
            pending.calls.insert(id, reply);
        }
        // forgets the call however this future ends, e.g. dropped on a caller's timeout
        let _forget = Forget {
            pending: &self.pending,
            id,
        };
        self.write(id, method, &params, session).await?;
        match tokio::time::timeout(CALL_TIMEOUT, answer).await {
            Err(_) => Err(CdpError::Timeout {
                method,
                after: CALL_TIMEOUT,
            }),
            Ok(Err(_)) => Err(CdpError::Closed),
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(message))) => Err(CdpError::Protocol { method, message }),
        }
    }

    /// Sends a command without waiting for its result, e.g. to let a paused request go on from
    /// the task that handles events.
    pub(super) async fn send(
        &self,
        method: &'static str,
        params: Value,
        session: Option<&str>,
    ) -> Result<(), CdpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.write(id, method, &params, session).await
    }

    async fn write(
        &self,
        id: u64,
        method: &'static str,
        params: &Value,
        session: Option<&str>,
    ) -> Result<(), CdpError> {
        let mut frame = serde_json::to_vec(&Command {
            id,
            method,
            params,
            session_id: session,
        })
        .expect("commands serialize");
        frame.push(0);
        let mut writer = self.writer.lock().await;
        match writer.write_all(&frame).await {
            Ok(()) => writer.flush().await.map_err(CdpError::Io),
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Err(CdpError::Closed),
            Err(e) => Err(CdpError::Io(e)),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl<W> Drop for Cdp<W> {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl<W> std::fmt::Debug for Cdp<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cdp").finish_non_exhaustive()
    }
}

struct Forget<'a> {
    pending: &'a Mutex<Pending>,
    id: u64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.calls.remove(&self.id);
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Command<'a> {
    id: u64,
    method: &'a str,
    params: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Incoming {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<ProtocolError>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct ProtocolError {
    message: String,
}

/// Hands answers to their callers and events to the channel until the browser hangs up, then
/// fails every call still waiting.
async fn read_messages<R: AsyncRead + Unpin>(
    reader: R,
    pending: Arc<Mutex<Pending>>,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut reader = BufReader::new(reader);
    let mut message = Vec::new();
    loop {
        message.clear();
        match reader.read_until(0, &mut message).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(error = %e, "cannot read from the browser");
                break;
            }
        }
        if message.last() == Some(&0) {
            message.pop();
        }
        let incoming: Incoming = match serde_json::from_slice(&message) {
            Ok(incoming) => incoming,
            // the error would quote the message, which may hold cookies
            Err(_) => {
                tracing::debug!(bytes = message.len(), "ignored a malformed browser message");
                continue;
            }
        };
        match (incoming.id, incoming.method) {
            (Some(id), _) => {
                let reply = match incoming.error {
                    Some(error) => Err(error.message),
                    None => Ok(incoming.result.unwrap_or(Value::Null)),
                };
                let caller = pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .calls
                    .remove(&id);
                // answers to `send` have nobody waiting
                if let Some(caller) = caller {
                    let _ = caller.send(reply);
                }
            }
            (None, Some(method)) => {
                // nobody listening any more is fine
                let _ = events.send(Event {
                    method,
                    params: incoming.params,
                    session_id: incoming.session_id,
                });
            }
            (None, None) => {}
        }
    }
    let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
    pending.closed = true;
    // dropping the senders fails the calls
    pending.calls.clear();
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::io::{AsyncReadExt, DuplexStream, duplex};

    use super::*;

    /// A client and the browser's end of its pipes.
    fn connect() -> (
        Cdp<DuplexStream>,
        mpsc::UnboundedReceiver<Event>,
        DuplexStream,
        DuplexStream,
    ) {
        let (to_browser, browser_reads) = duplex(1 << 16);
        let (browser_writes, from_browser) = duplex(1 << 16);
        let (cdp, events) = Cdp::new(from_browser, to_browser);
        (cdp, events, browser_reads, browser_writes)
    }

    async fn read_command(from: &mut DuplexStream) -> Value {
        let mut bytes = Vec::new();
        loop {
            let byte = from.read_u8().await.unwrap();
            if byte == 0 {
                return serde_json::from_slice(&bytes).unwrap();
            }
            bytes.push(byte);
        }
    }

    #[tokio::test]
    async fn answers_reach_their_callers_in_any_order() {
        let (cdp, _events, mut commands, mut replies) = connect();
        let browser = async {
            let first = read_command(&mut commands).await;
            let second = read_command(&mut commands).await;
            assert_eq!(first["method"], "Browser.getVersion");
            assert!(
                first.get("sessionId").is_none(),
                "the browser takes no session id"
            );
            assert_eq!(second["sessionId"], "page");
            // the second answer first, split across writes, and one with an error
            let answer = format!(
                "{}\0{}\0",
                json!({ "id": second["id"], "result": { "n": 2 } }),
                json!({ "id": first["id"], "error": { "code": -32000, "message": "nope" } })
            );
            let (a, b) = answer.as_bytes().split_at(7);
            replies.write_all(a).await.unwrap();
            replies.flush().await.unwrap();
            replies.write_all(b).await.unwrap();
        };
        let (first, second, ()) = tokio::join!(
            cdp.call("Browser.getVersion", json!({}), None),
            cdp.call("Page.enable", json!({}), Some("page")),
            browser
        );
        assert!(matches!(
            first,
            Err(CdpError::Protocol { method: "Browser.getVersion", message }) if message == "nope"
        ));
        assert_eq!(second.unwrap(), json!({ "n": 2 }));
    }

    #[tokio::test]
    async fn events_arrive_in_order_with_their_session() {
        let (_cdp, mut events, _commands, mut replies) = connect();
        let sent = format!(
            "{}\0{}\0",
            json!({ "method": "Page.loadEventFired", "params": { "timestamp": 1 }, "sessionId": "page" }),
            json!({ "method": "Target.targetCreated", "params": {} })
        );
        replies.write_all(sent.as_bytes()).await.unwrap();
        let first = events.recv().await.unwrap();
        assert_eq!(first.method, "Page.loadEventFired");
        assert_eq!(first.session_id.as_deref(), Some("page"));
        assert_eq!(first.params["timestamp"], 1);
        let second = events.recv().await.unwrap();
        assert_eq!(second.method, "Target.targetCreated");
        assert_eq!(second.session_id, None);
    }

    #[tokio::test]
    async fn calls_fail_once_the_browser_hangs_up() {
        let (cdp, _events, mut commands, replies) = connect();
        let call = cdp.call("Browser.close", json!({}), None);
        let hang_up = async {
            read_command(&mut commands).await;
            drop(replies);
        };
        let (result, ()) = tokio::join!(call, hang_up);
        assert!(matches!(result, Err(CdpError::Closed)));
        assert!(matches!(
            cdp.call("Browser.close", json!({}), None).await,
            Err(CdpError::Closed)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered_calls_time_out_and_are_forgotten() {
        let (cdp, _events, _commands, _replies) = connect();
        let result = cdp.call("Page.navigate", json!({}), Some("page")).await;
        assert!(matches!(
            result,
            Err(CdpError::Timeout {
                method: "Page.navigate",
                ..
            })
        ));
        assert!(cdp.pending().calls.is_empty());
    }
}
