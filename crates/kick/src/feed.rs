//! One chat's realtime connection over whichever transport Kick assigned it,
//! yielding the same frames either way.

use std::time::Duration;

use anyhow::Context;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, http::HeaderValue},
    MaybeTlsStream, WebSocketStream,
};

use crate::api::{self, Realtime};
use crate::centrifugo;
use crate::pusher::{self, PusherFrame};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long before expiry the Centrifugo token is renewed.
const REFRESH_LEAD: Duration = Duration::from_secs(120);
/// Delay before retrying a failed token renewal.
const REFRESH_RETRY: Duration = Duration::from_secs(30);

pub(crate) enum Feed {
    Pusher(Box<Socket>),
    Centrifugo {
        session: Box<centrifugo::Session<MaybeTlsStream<TcpStream>>>,
        client_id: String,
        ttl: Option<Duration>,
        refresh_at: Option<Instant>,
    },
}

impl Feed {
    /// Connects and subscribes to `primary` (confirmed before returning) plus
    /// `aliases` (best effort). Returns events that arrived during the handshake.
    pub(crate) async fn open(
        channel_id: u64,
        primary: &str,
        aliases: &[String],
    ) -> anyhow::Result<(Self, std::collections::VecDeque<PusherFrame>)> {
        let client_id = uuid::Uuid::new_v4().to_string();
        match api::fetch_realtime(channel_id, &client_id).await? {
            Realtime::Pusher { url } => {
                let (mut ws, _) = tokio_tungstenite::connect_async(url)
                    .await
                    .context("connecting to Kick Pusher")?;
                let pending = pusher::handshake(&mut ws, primary).await?;
                for alias in aliases {
                    pusher::subscribe(&mut ws, alias).await?;
                }
                tracing::debug!("kick chat {primary} over Pusher");
                Ok((Feed::Pusher(Box::new(ws)), pending))
            }
            Realtime::Centrifugo { url } => {
                let token = api::fetch_realtime_token(&client_id).await?;
                let mut request = url
                    .as_str()
                    .into_client_request()
                    .context("building the Kick realtime request")?;
                // The server refuses the upgrade from any other origin.
                request
                    .headers_mut()
                    .insert("Origin", HeaderValue::from_static("https://kick.com"));
                let (ws, _) = tokio_tungstenite::connect_async(request)
                    .await
                    .context("connecting to Kick realtime")?;
                let (mut session, connected) = centrifugo::Session::connect(ws, &token).await?;
                let pending = session.subscribe(primary, aliases).await?;
                let ttl = connected.ttl.map(Duration::from_secs);
                tracing::debug!("kick chat {primary} over Centrifugo ({url})");
                Ok((
                    Feed::Centrifugo {
                        session: Box::new(session),
                        client_id,
                        ttl,
                        refresh_at: ttl.map(refresh_deadline),
                    },
                    pending,
                ))
            }
        }
    }

    /// The next event; `Ok(None)` when the server closed the connection.
    pub(crate) async fn next_frame(&mut self) -> anyhow::Result<Option<PusherFrame>> {
        match self {
            Feed::Pusher(ws) => pusher::next_frame(ws).await,
            Feed::Centrifugo {
                session,
                client_id,
                ttl,
                refresh_at,
            } => loop {
                let due = *refresh_at;
                let refresh = async move {
                    match due {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    frame = session.next_frame() => return frame,
                    () = refresh => {
                        // A failed renewal retries; if the token runs out first, the
                        // server disconnects and the caller reconnects.
                        *refresh_at = Some(match api::fetch_realtime_token(client_id).await {
                            Ok(token) => {
                                session.refresh(&token).await?;
                                ttl.map_or(Instant::now() + REFRESH_RETRY, refresh_deadline)
                            }
                            Err(err) => {
                                tracing::warn!("kick chat token renewal failed: {err:#}");
                                Instant::now() + REFRESH_RETRY
                            }
                        });
                    }
                }
            },
        }
    }
}

fn refresh_deadline(ttl: Duration) -> Instant {
    Instant::now() + ttl.saturating_sub(REFRESH_LEAD).max(ttl / 2)
}
