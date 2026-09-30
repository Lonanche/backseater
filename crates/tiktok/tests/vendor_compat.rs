//! Regression coverage for the locally patched PirateTok transport, without TikTok traffic.
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use piratetok_live_rs::{
    structs::{
        proto::{
            frames::WebcastPushFrame,
            messages::{WebcastChatMessage, WebcastGiftMessage, WebcastMessage, WebcastResponse},
            messages_ext::WebcastEmoteChatMessage,
        },
        TikTokLiveEvent,
    },
    websocket::connection::run_websocket,
};
use prost::Message;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[tokio::test]
async fn patched_transport_preserves_history_and_acknowledges_frames() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            for expected in ["hb", "im_enter_room"] {
                let frame = socket.next().await.unwrap().unwrap().into_data();
                assert_eq!(WebcastPushFrame::decode(frame).unwrap().payload_type, expected);
            }
            let envelope = |kind: &str, payload: Vec<u8>, history| WebcastMessage {
                r#type: kind.into(), payload, is_history: history, ..Default::default()
            };
            let response = WebcastResponse {
                needs_ack: true,
                internal_ext: "test-cursor".into(),
                messages: vec![
                    envelope("WebcastChatMessage", WebcastChatMessage::default().encode_to_vec(), true),
                    envelope("WebcastEmoteChatMessage", WebcastEmoteChatMessage::default().encode_to_vec(), true),
                    envelope("WebcastGiftMessage", WebcastGiftMessage::default().encode_to_vec(), true),
                    envelope("WebcastChatMessage", WebcastChatMessage::default().encode_to_vec(), false),
                ],
                ..Default::default()
            };
            let frame = WebcastPushFrame {
                log_id: 42, payload_type: "msg".into(), payload: response.encode_to_vec(), ..Default::default()
            };
            socket.send(WsMessage::Binary(frame.encode_to_vec().into())).await.unwrap();
            let ack = socket.next().await.unwrap().unwrap().into_data();
            let ack = WebcastPushFrame::decode(ack).unwrap();
            assert_eq!(ack.payload_type, "ack");
            assert_eq!(ack.log_id, 42);
            assert_eq!(ack.payload, b"test-cursor");
            socket.close(None).await.unwrap();
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let client = tokio::spawn(async move {
            run_websocket(&url, "", "test", "123", Duration::from_secs(600), Duration::from_secs(5), None, "en", tx)
                .await.unwrap();
        });
        assert!(matches!(rx.recv().await, Some(TikTokLiveEvent::Connected { room_id }) if room_id == "123"));
        assert!(matches!(rx.recv().await, Some(TikTokLiveEvent::ChatHistory(_))));
        assert!(matches!(rx.recv().await, Some(TikTokLiveEvent::EmoteChatHistory(_))));
        assert!(matches!(rx.recv().await, Some(TikTokLiveEvent::Chat(_))));
        assert!(rx.recv().await.is_none(), "historical gifts must not fire live alerts");
        server.await.unwrap();
        client.await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn failed_handshake_does_not_emit_connected() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            drop(listener.accept().await.unwrap());
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let result = run_websocket(
            &url,
            "",
            "test",
            "123",
            Duration::from_secs(600),
            Duration::from_secs(5),
            None,
            "en",
            tx,
        )
        .await;
        assert!(result.is_err());
        assert!(rx.recv().await.is_none());
        server.await.unwrap();
    })
    .await
    .unwrap();
}
