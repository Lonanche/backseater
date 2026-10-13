//! Pusher framing and the handshake required before chat/history can start.

use std::collections::VecDeque;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Deserializer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{tungstenite::Message as WsMessage, WebSocketStream};

#[derive(Debug, Deserialize)]
pub(crate) struct PusherFrame {
    pub event: String,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default, deserialize_with = "frame_data")]
    pub data: String,
}

fn frame_data<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    // Chat usually carries encoded JSON; protocol errors/acks can carry objects.
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(data) => data,
        data => data.to_string(),
    })
}

pub(crate) async fn next_frame<S>(
    socket: &mut WebSocketStream<S>,
) -> anyhow::Result<Option<PusherFrame>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(frame) = socket.next().await {
        match frame.context("Kick websocket error")? {
            WsMessage::Text(text) => {
                let Ok(frame) = serde_json::from_str::<PusherFrame>(&text) else {
                    continue;
                };
                match frame.event.as_str() {
                    "pusher:error" | "pusher:subscription_error" => {
                        anyhow::bail!("Kick {}: {}", frame.event, frame.data);
                    }
                    "pusher:ping" => {
                        socket
                            .send(WsMessage::Text(
                                r#"{"event":"pusher:pong","data":{}}"#.into(),
                            ))
                            .await
                            .context("answering Kick Pusher ping")?;
                    }
                    _ => return Ok(Some(frame)),
                }
            }
            WsMessage::Ping(data) => {
                socket
                    .send(WsMessage::Pong(data))
                    .await
                    .context("answering Kick websocket ping")?;
            }
            WsMessage::Close(Some(close)) => {
                anyhow::bail!("Kick Pusher closed ({}): {}", close.code, close.reason);
            }
            WsMessage::Close(None) => return Ok(None),
            _ => {}
        }
    }
    Ok(None)
}

pub(crate) async fn subscribe<S>(
    socket: &mut WebSocketStream<S>,
    channel: &str,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = serde_json::json!({
        "event": "pusher:subscribe",
        "data": { "auth": "", "channel": channel }
    });
    socket
        .send(WsMessage::Text(request.to_string()))
        .await
        .with_context(|| format!("subscribing to {channel}"))
}

pub(crate) async fn handshake<S>(
    socket: &mut WebSocketStream<S>,
    chatroom: &str,
) -> anyhow::Result<VecDeque<PusherFrame>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let frame = next_frame(socket)
            .await?
            .context("Kick Pusher closed before connection confirmation")?;
        if frame.event == "pusher:connection_established" {
            break;
        }
    }
    // Subscribe only to the primary feed until it is confirmed. Older Kick
    // acknowledgements omit `channel`; with one pending subscription it is clear
    // which feed succeeded, and an alias cannot mask a failed chat subscription.
    subscribe(socket, chatroom).await?;
    let mut pending = VecDeque::new();
    loop {
        let frame = next_frame(socket)
            .await?
            .context("Kick Pusher closed before chat subscription confirmation")?;
        if frame.event == "pusher_internal:subscription_succeeded" {
            if frame
                .channel
                .as_deref()
                .is_none_or(|channel| channel == chatroom)
            {
                return Ok(pending);
            }
        } else {
            pending.push_back(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;
    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame, Role};

    async fn sockets() -> (WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>) {
        let (client, server) = tokio::io::duplex(8192);
        tokio::join!(
            WebSocketStream::from_raw_socket(client, Role::Client, None),
            WebSocketStream::from_raw_socket(server, Role::Server, None)
        )
    }

    async fn send(socket: &mut WebSocketStream<DuplexStream>, text: &str) {
        socket.send(WsMessage::Text(text.into())).await.unwrap();
    }

    async fn join_chatroom(
        client: &mut WebSocketStream<DuplexStream>,
    ) -> anyhow::Result<VecDeque<PusherFrame>> {
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            handshake(client, "chatrooms.12.v2"),
        )
        .await
        .expect("handshake did not finish")
    }

    #[test]
    fn accepts_encoded_chat_and_object_protocol_data() {
        for raw in [
            r#"{"event":"ChatMessageEvent","data":"{\"id\":\"m1\"}"}"#,
            r#"{"event":"ChatMessageEvent","data":{"id":"m1"}}"#,
        ] {
            let frame: PusherFrame = serde_json::from_str(raw).unwrap();
            let data: serde_json::Value = serde_json::from_str(&frame.data).unwrap();
            assert_eq!(data["id"], "m1");
        }
    }

    #[tokio::test]
    async fn rejected_app_key_fails_the_handshake_instead_of_starting_history() {
        let (mut client, mut server) = sockets().await;
        send(&mut server, r#"{"event":"pusher:error","data":{"code":4001,"message":"App key old not in this cluster"}}"#).await;
        let error = join_chatroom(&mut client).await.unwrap_err().to_string();
        assert!(error.contains("4001"));
        assert!(error.contains("not in this cluster"));
    }

    #[tokio::test]
    async fn handshake_waits_for_primary_ack_and_preserves_early_chat() {
        let (mut client, mut server) = sockets().await;
        let server_task = async {
            send(
                &mut server,
                r#"{"event":"pusher:connection_established","data":"{\"socket_id\":\"1.2\"}"}"#,
            )
            .await;
            let request = server.next().await.unwrap().unwrap().into_text().unwrap();
            let request: serde_json::Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["event"], "pusher:subscribe");
            assert_eq!(request["data"]["channel"], "chatrooms.12.v2");
            send(&mut server, r#"{"event":"pusher_internal:subscription_succeeded","channel":"channel.99","data":{}}"#).await;
            send(&mut server, r#"{"event":"pusher:ping","data":{}}"#).await;
            let pong = server.next().await.unwrap().unwrap().into_text().unwrap();
            let pong: serde_json::Value = serde_json::from_str(&pong).unwrap();
            assert_eq!(pong["event"], "pusher:pong");
            server.send(WsMessage::Ping(vec![1, 2])).await.unwrap();
            assert_eq!(
                server.next().await.unwrap().unwrap(),
                WsMessage::Pong(vec![1, 2])
            );
            send(
                &mut server,
                r#"{"event":"ChatMessageEvent","data":{"id":"early"}}"#,
            )
            .await;
            send(&mut server, r#"{"event":"pusher_internal:subscription_succeeded","channel":"chatrooms.12.v2","data":{}}"#).await;
        };
        let (result, ()) = tokio::join!(join_chatroom(&mut client), server_task);
        let frames = result.unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "ChatMessageEvent");
        assert_eq!(frames[0].data, r#"{"id":"early"}"#);
    }

    #[tokio::test]
    async fn legacy_ack_without_channel_is_unambiguous() {
        let (mut client, mut server) = sockets().await;
        send(
            &mut server,
            r#"{"event":"pusher:connection_established","data":{}}"#,
        )
        .await;
        send(
            &mut server,
            r#"{"event":"pusher_internal:subscription_succeeded","data":"{}"}"#,
        )
        .await;
        assert!(join_chatroom(&mut client).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn subscription_rejection_is_an_error() {
        let (mut client, mut server) = sockets().await;
        send(
            &mut server,
            r#"{"event":"pusher:connection_established","data":{}}"#,
        )
        .await;
        send(
            &mut server,
            r#"{"event":"pusher:subscription_error","data":"{\"message\":\"denied\"}"}"#,
        )
        .await;
        let error = join_chatroom(&mut client).await.unwrap_err().to_string();
        assert!(error.contains("subscription_error"));
        assert!(error.contains("denied"));
    }

    #[tokio::test]
    async fn close_preserves_the_server_code_and_reason() {
        let (mut client, mut server) = sockets().await;
        server
            .send(WsMessage::Close(Some(CloseFrame {
                code: CloseCode::Library(4001),
                reason: "App key rejected".into(),
            })))
            .await
            .unwrap();
        let error = next_frame(&mut client).await.unwrap_err().to_string();
        assert!(error.contains("4001"));
        assert!(error.contains("App key rejected"));
    }
}
