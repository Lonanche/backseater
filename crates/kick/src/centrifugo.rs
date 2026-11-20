//! Kick's Centrifugo feed (JSON protocol), what kick.com itself has used since
//! October 2026. Same channel names and `App\Events\…` events as the Pusher feed;
//! only the framing and the guest-token handshake differ:
//!
//! - `{"id":1,"connect":{"token":…,"name":"js"}}` → a reply with the token's
//!   `ttl` (2700 s), renewed by a `refresh` command before it runs out.
//! - One `subscribe` per channel. A connection holds at most 128 (error 106
//!   past that), plenty for one chat.
//! - `{}` from the server is a ping, answered with `{}`.
//! - Several replies can share one websocket frame, one per line.
//! - Events arrive as `{"push":{"channel":…,"pub":{"data":{"event":…,"data":…}}}}`,
//!   where the inner `data` is usually a JSON string, like Pusher's.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{tungstenite::Message as WsMessage, WebSocketStream};

use crate::pusher::PusherFrame;

pub(crate) struct Session<S> {
    socket: WebSocketStream<S>,
    lines: VecDeque<String>,
    next_id: u64,
    /// Subscribe command ids awaiting a reply, by channel.
    subscribing: HashMap<u64, String>,
    /// How long the socket may stay without any frame before it counts as dead.
    silence: Duration,
}

/// Kick pings every 25 s; this long with no frame at all means the
/// connection died without closing.
const SILENCE: Duration = Duration::from_secs(60);

/// What the connect reply said about the token's lifetime.
pub(crate) struct Connected {
    /// Seconds until the token expires, when it does.
    pub ttl: Option<u64>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Session<S> {
    /// Sends the connect command and waits for its reply.
    pub(crate) async fn connect(
        socket: WebSocketStream<S>,
        token: &str,
    ) -> anyhow::Result<(Self, Connected)> {
        Self::connect_with_silence(socket, token, SILENCE).await
    }

    async fn connect_with_silence(
        socket: WebSocketStream<S>,
        token: &str,
        silence: Duration,
    ) -> anyhow::Result<(Self, Connected)> {
        let mut session = Self {
            socket,
            lines: VecDeque::new(),
            next_id: 1,
            subscribing: HashMap::new(),
            silence,
        };
        let id = session
            .command(
                "connect",
                serde_json::json!({ "token": token, "name": "js" }),
            )
            .await?;
        loop {
            let reply = session
                .next_reply()
                .await?
                .context("Kick realtime closed before confirming the connection")?;
            if reply_id(&reply) != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                anyhow::bail!("Kick realtime refused the connection: {error}");
            }
            let connect = reply.get("connect").cloned().unwrap_or_default();
            let expires = connect.get("expires").and_then(Value::as_bool) == Some(true);
            let ttl = connect
                .get("ttl")
                .and_then(Value::as_u64)
                .filter(|_| expires);
            return Ok((session, Connected { ttl }));
        }
    }

    /// Subscribes to `primary` and `aliases` together, then waits until
    /// `primary` is confirmed. A refused alias is only logged: Kick refuses
    /// feeds it doesn't have, and chat still works without them. Events that
    /// arrive before the confirmation are returned so none are lost.
    pub(crate) async fn subscribe(
        &mut self,
        primary: &str,
        aliases: &[String],
    ) -> anyhow::Result<VecDeque<PusherFrame>> {
        let mut commands = Vec::with_capacity(aliases.len() + 1);
        let primary_id = self.next_id;
        for channel in std::iter::once(primary).chain(aliases.iter().map(String::as_str)) {
            let id = self.take_id();
            self.subscribing.insert(id, channel.to_owned());
            commands.push(
                serde_json::json!({ "id": id, "subscribe": { "channel": channel } }).to_string(),
            );
        }
        self.send(commands.join("\n")).await?;
        let mut early = VecDeque::new();
        loop {
            let message = self
                .next_reply()
                .await?
                .context("Kick realtime closed before confirming the chat subscription")?;
            if reply_id(&message) == Some(primary_id) {
                self.subscribing.remove(&primary_id);
                if let Some(error) = message.get("error") {
                    anyhow::bail!("Kick realtime refused {primary}: {error}");
                }
                return Ok(early);
            }
            if let Some(frame) = self.handle(message)? {
                early.push_back(frame);
            }
        }
    }

    /// Renews the connection token before it expires.
    pub(crate) async fn refresh(&mut self, token: &str) -> anyhow::Result<()> {
        self.command("refresh", serde_json::json!({ "token": token }))
            .await
            .map(drop)
    }

    /// The next event, answering pings on the way. `Ok(None)` = the socket closed.
    pub(crate) async fn next_frame(&mut self) -> anyhow::Result<Option<PusherFrame>> {
        while let Some(message) = self.next_reply().await? {
            if let Some(frame) = self.handle(message)? {
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }

    /// Interprets one reply or push; `Some` for an event.
    fn handle(&mut self, message: Value) -> anyhow::Result<Option<PusherFrame>> {
        if let Some(id) = reply_id(&message) {
            let channel = self.subscribing.remove(&id);
            if let Some(error) = message.get("error") {
                match channel {
                    Some(channel) => tracing::debug!("kick realtime refused {channel}: {error}"),
                    None => anyhow::bail!("Kick realtime command failed: {error}"),
                }
            }
            return Ok(None);
        }
        let Some(push) = message.get("push") else {
            return Ok(None);
        };
        if let Some(disconnect) = push.get("disconnect") {
            anyhow::bail!("Kick realtime disconnected: {disconnect}");
        }
        let channel = push
            .get("channel")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if push.get("unsub").is_some() {
            anyhow::bail!(
                "Kick realtime unsubscribed {}",
                channel.as_deref().unwrap_or("a channel")
            );
        }
        let Some(data) = push.get("pub").and_then(|p| p.get("data")) else {
            return Ok(None);
        };
        let Some(event) = data.get("event").and_then(Value::as_str) else {
            return Ok(None);
        };
        let data = match data.get("data") {
            Some(Value::String(data)) => data.clone(),
            Some(data) => data.to_string(),
            None => String::new(),
        };
        Ok(Some(PusherFrame {
            event: event.to_owned(),
            channel,
            data,
        }))
    }

    /// The next JSON line from the server, answering `{}` pings.
    async fn next_reply(&mut self) -> anyhow::Result<Option<Value>> {
        loop {
            if let Some(line) = self.lines.pop_front() {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if value.as_object().is_some_and(|o| o.is_empty()) {
                    self.send("{}".to_owned())
                        .await
                        .context("answering Kick realtime ping")?;
                    continue;
                }
                return Ok(Some(value));
            }
            // Any frame counts, pings included: a quiet chat can go minutes
            // without an event while the socket is healthy.
            let Some(frame) = tokio::time::timeout(self.silence, self.socket.next())
                .await
                .map_err(|_| anyhow::anyhow!("Kick realtime went silent"))?
            else {
                return Ok(None);
            };
            match frame.context("Kick realtime websocket error")? {
                WsMessage::Text(text) => self.lines.extend(
                    text.split('\n')
                        .filter(|line| !line.trim().is_empty())
                        .map(str::to_owned),
                ),
                WsMessage::Ping(data) => self
                    .socket
                    .send(WsMessage::Pong(data))
                    .await
                    .context("answering Kick websocket ping")?,
                WsMessage::Close(Some(close)) => {
                    anyhow::bail!("Kick realtime closed ({}): {}", close.code, close.reason);
                }
                WsMessage::Close(None) => return Ok(None),
                _ => {}
            }
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn command(&mut self, name: &str, body: Value) -> anyhow::Result<u64> {
        let id = self.take_id();
        let mut command = serde_json::Map::new();
        command.insert("id".into(), id.into());
        command.insert(name.into(), body);
        self.send(Value::Object(command).to_string())
            .await
            .with_context(|| format!("sending Kick realtime {name}"))?;
        Ok(id)
    }

    async fn send(&mut self, text: String) -> anyhow::Result<()> {
        self.socket
            .send(WsMessage::Text(text))
            .await
            .context("Kick realtime send failed")
    }
}

fn reply_id(message: &Value) -> Option<u64> {
    message.get("id").and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;
    use tokio_tungstenite::tungstenite::protocol::Role;

    async fn sockets() -> (WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>) {
        let (client, server) = tokio::io::duplex(16384);
        tokio::join!(
            WebSocketStream::from_raw_socket(client, Role::Client, None),
            WebSocketStream::from_raw_socket(server, Role::Server, None)
        )
    }

    async fn send(socket: &mut WebSocketStream<DuplexStream>, text: &str) {
        socket.send(WsMessage::Text(text.into())).await.unwrap();
    }

    async fn recv(socket: &mut WebSocketStream<DuplexStream>) -> String {
        socket.next().await.unwrap().unwrap().into_text().unwrap()
    }

    #[tokio::test]
    async fn connects_subscribes_and_reads_events() {
        let (client, mut server) = sockets().await;
        let server_task = async {
            let connect: Value = serde_json::from_str(&recv(&mut server).await).unwrap();
            assert_eq!(connect["connect"]["token"], "tok");
            send(
                &mut server,
                r#"{"id":1,"connect":{"client":"c","expires":true,"ttl":2700,"ping":25}}"#,
            )
            .await;
            let subs = recv(&mut server).await;
            let subs: Vec<Value> = subs
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            assert_eq!(subs[0]["subscribe"]["channel"], "chatrooms.1.v2");
            assert_eq!(subs[1]["subscribe"]["channel"], "chatroom_1");
            // An event before the ack, an alias refusal and the ack in one frame.
            send(
                &mut server,
                concat!(
                    r#"{"push":{"channel":"chatrooms.1.v2","pub":{"data":{"event":"App\\Events\\ChatMessageEvent","data":"{\"id\":\"early\"}"}}}}"#,
                    "\n",
                    r#"{"id":3,"error":{"code":103,"message":"permission denied"}}"#,
                    "\n",
                    r#"{"id":2,"subscribe":{}}"#
                ),
            )
            .await;
            send(&mut server, "{}").await;
            assert_eq!(recv(&mut server).await, "{}");
            send(
                &mut server,
                r#"{"push":{"channel":"chatroom_1","pub":{"data":{"event":"GiftedSubscriptionsEvent","data":{"gifter_username":"a"}}}}}"#,
            )
            .await;
        };
        let client_task = async {
            let (mut session, connected) = Session::connect(client, "tok").await.unwrap();
            assert_eq!(connected.ttl, Some(2700));
            let early = session
                .subscribe("chatrooms.1.v2", &["chatroom_1".to_owned()])
                .await
                .unwrap();
            assert_eq!(early.len(), 1);
            assert_eq!(early[0].event, r"App\Events\ChatMessageEvent");
            assert_eq!(early[0].data, r#"{"id":"early"}"#);
            let frame = session.next_frame().await.unwrap().unwrap();
            assert_eq!(frame.event, "GiftedSubscriptionsEvent");
            assert_eq!(frame.channel.as_deref(), Some("chatroom_1"));
            assert_eq!(frame.data, r#"{"gifter_username":"a"}"#);
        };
        tokio::join!(server_task, client_task);
    }

    #[tokio::test]
    async fn refused_token_and_chat_subscription_are_errors() {
        let (client, mut server) = sockets().await;
        let server_task = async {
            recv(&mut server).await;
            send(
                &mut server,
                r#"{"id":1,"error":{"code":109,"message":"token expired"}}"#,
            )
            .await;
        };
        let (result, ()) = tokio::join!(Session::connect(client, "old"), server_task);
        assert!(result.err().unwrap().to_string().contains("token expired"));

        let (client, mut server) = sockets().await;
        let server_task = async {
            recv(&mut server).await;
            send(&mut server, r#"{"id":1,"connect":{}}"#).await;
            recv(&mut server).await;
            send(
                &mut server,
                r#"{"id":2,"error":{"code":106,"message":"limit exceeded"}}"#,
            )
            .await;
        };
        let client_task = async {
            let (mut session, connected) = Session::connect(client, "tok").await.unwrap();
            assert_eq!(connected.ttl, None);
            session.subscribe("chatrooms.1.v2", &[]).await
        };
        let ((), result) = tokio::join!(server_task, client_task);
        assert!(result.unwrap_err().to_string().contains("limit exceeded"));
    }

    #[tokio::test]
    async fn pings_keep_a_quiet_chat_alive_and_silence_ends_it() {
        let silence = Duration::from_millis(300);
        let (client, mut server) = sockets().await;
        let server_task = async {
            recv(&mut server).await;
            send(&mut server, r#"{"id":1,"connect":{}}"#).await;
            // Pings only, for longer than the silence window, then an event.
            for _ in 0..8 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                send(&mut server, "{}").await;
                assert_eq!(recv(&mut server).await, "{}");
            }
            send(
                &mut server,
                r#"{"push":{"channel":"c","pub":{"data":{"event":"E","data":"{}"}}}}"#,
            )
            .await;
            // Then nothing at all; keep the socket open while the client waits.
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        let client_task = async {
            let (mut session, _) = Session::connect_with_silence(client, "tok", silence)
                .await
                .unwrap();
            let frame = session.next_frame().await.unwrap().unwrap();
            assert_eq!(frame.event, "E");
            session.next_frame().await
        };
        let (result, ()) = tokio::select! {
            result = client_task => (result, ()),
            () = server_task => panic!("client never gave up on a silent socket"),
        };
        assert!(result.unwrap_err().to_string().contains("went silent"));
    }

    #[tokio::test]
    async fn server_disconnect_push_ends_the_session() {
        let (client, mut server) = sockets().await;
        let server_task = async {
            recv(&mut server).await;
            send(&mut server, r#"{"id":1,"connect":{}}"#).await;
            send(
                &mut server,
                r#"{"push":{"disconnect":{"code":3005,"reason":"connection limit"}}}"#,
            )
            .await;
        };
        let client_task = async {
            let (mut session, _) = Session::connect(client, "tok").await.unwrap();
            session.next_frame().await
        };
        let ((), result) = tokio::join!(server_task, client_task);
        assert!(result.unwrap_err().to_string().contains("connection limit"));
    }
}
