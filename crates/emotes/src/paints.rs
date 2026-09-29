//! 7TV cosmetics: per-user **paints** (gradient/solid name colors) and **badges**.
//!
//! Both come from the same 7TV cosmetics system. The active paint/badge ids for a
//! user are read from `GET /v3/users/twitch/{twitch_id}` (`user.style.paint_id` /
//! `style.badge_id`); the definitions are resolved from the v3 GraphQL
//! `cosmetics(list:)` query. Everything is cached process-wide so a chatter is
//! looked up once per cache lifetime, with bounded retention and shared requests.
//!
//! This mirrors the C++ Backseater's `SeventvPaints`/`SeventvBadges`, minus the
//! real-time EventAPI (we resolve lazily per chatter instead of subscribing to a
//! cosmetics websocket). URL/image paints can't be drawn as a text gradient, so
//! they collapse to a representative solid color.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bks_core::{Badge, NamePaint, PaintKind, PaintStop};
use serde::Deserialize;

use crate::http::shared_client;
use crate::seventv::{largest_url, Host};

const USER_URL: &str = "https://7tv.io/v3/users/twitch/";
const GQL_URL: &str = "https://7tv.io/v3/gql";

/// How long a cached user→cosmetic-ids lookup stays fresh. A user rarely changes
/// their paint mid-session; this just lets a long-running app pick up changes.
pub const COSMETICS_TTL: Duration = Duration::from_secs(1800);

/// Whether 7TV name paints + badges are applied. Flipped by the settings toggle
/// (process-wide, so already-running connections react without re-plumbing). When
/// off, [`resolve`] returns nothing immediately and does no network work.
static ENABLED: AtomicBool = AtomicBool::new(true);
static FETCHES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

/// Sets whether 7TV cosmetics are resolved/applied (the settings toggle).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether 7TV cosmetics are currently enabled.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The resolved 7TV cosmetics for one chatter: a name paint and/or a badge,
/// either of which may be absent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cosmetics {
    pub paint: Option<NamePaint>,
    pub badge: Option<Badge>,
}

impl Cosmetics {
    /// Whether the user has neither cosmetic (so the bridge can skip emitting).
    pub fn is_empty(&self) -> bool {
        self.paint.is_none() && self.badge.is_none()
    }
}

#[derive(Clone, Default)]
struct UserStyle {
    paint_id: Option<String>,
    badge_id: Option<String>,
}

/// FIFO expiry keeps both the values and their eviction metadata bounded.
struct TimedCache<T> {
    values: HashMap<String, T>,
    order: VecDeque<(Instant, String)>,
    capacity: usize,
}

impl<T: Clone> TimedCache<T> {
    fn new(capacity: usize) -> Self {
        Self {
            values: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn get_or_insert(&mut self, key: &str, now: Instant, create: impl FnOnce() -> T) -> T {
        while self
            .order
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= COSMETICS_TTL)
        {
            let (_, key) = self.order.pop_front().unwrap();
            self.values.remove(&key);
        }
        if let Some(value) = self.values.get(key) {
            return value.clone();
        }
        while self.values.len() >= self.capacity {
            let (_, key) = self.order.pop_front().unwrap();
            self.values.remove(&key);
        }
        let value = create();
        self.values.insert(key.to_string(), value.clone());
        self.order.push_back((now, key.to_string()));
        value
    }
}

type Cache<T> = Mutex<TimedCache<Arc<tokio::sync::OnceCell<T>>>>;

fn cache_cell<T>(cache: &Cache<T>, key: &str) -> Arc<tokio::sync::OnceCell<T>> {
    cache
        .lock()
        .unwrap()
        .get_or_insert(key, Instant::now(), || {
            Arc::new(tokio::sync::OnceCell::new())
        })
}

async fn cached<T: Clone, F: std::future::Future<Output = T>>(
    cache: &Cache<T>,
    key: &str,
    fetch: impl FnOnce() -> F,
) -> T {
    cache_cell(cache, key).get_or_init(fetch).await.clone()
}

/// A shared pending cell coalesces the same user across channels. The permit
/// covers the entire resolution, including paint/badge lookups.
pub async fn resolve(twitch_id: &str) -> Cosmetics {
    if !enabled() || twitch_id.is_empty() {
        return Cosmetics::default();
    }
    static USERS: OnceLock<Cache<Cosmetics>> = OnceLock::new();
    let cache = USERS.get_or_init(|| Mutex::new(TimedCache::new(4096)));
    cache_cell(cache, twitch_id)
        .get_or_try_init(|| async {
            let _permit = FETCHES.acquire().await.map_err(|_| ())?;
            // Disabling while queued cancels initialization, so re-enabling can retry.
            if !enabled() {
                return Err(());
            }
            let style = fetch_user_style(twitch_id).await.unwrap_or_default();
            let paint = match style.paint_id {
                Some(id) => paint_def(&id).await,
                None => None,
            };
            let badge = match style.badge_id {
                Some(id) => badge_def(&id).await,
                None => None,
            };
            Ok(Cosmetics { paint, badge })
        })
        .await
        .cloned()
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct UserResponse {
    user: Option<StyledUser>,
}

#[derive(Deserialize)]
struct StyledUser {
    #[serde(default)]
    style: Style,
}

#[derive(Deserialize, Default)]
struct Style {
    #[serde(default)]
    paint_id: Option<String>,
    #[serde(default)]
    badge_id: Option<String>,
}

async fn fetch_user_style(twitch_id: &str) -> anyhow::Result<UserStyle> {
    let resp = shared_client()
        .get(format!("{USER_URL}{twitch_id}"))
        .send()
        .await?
        .error_for_status()?;
    let body: UserResponse = resp.json().await?;
    let style = body.user.map(|u| u.style).unwrap_or_default();
    Ok(UserStyle {
        // 7TV serializes "no cosmetic" as an empty string sometimes; treat it as None.
        paint_id: style.paint_id.filter(|s| !s.is_empty()),
        badge_id: style.badge_id.filter(|s| !s.is_empty()),
    })
}

/// Resolves (and caches) a paint id to a [`NamePaint`], `None` if unknown or it
/// can't be rendered as a name color.
async fn paint_def(id: &str) -> Option<NamePaint> {
    static CACHE: OnceLock<Cache<Option<NamePaint>>> = OnceLock::new();
    cached(
        CACHE.get_or_init(|| Mutex::new(TimedCache::new(1024))),
        id,
        || async { fetch_cosmetics(id).await.ok().and_then(|c| c.0) },
    )
    .await
}

async fn badge_def(id: &str) -> Option<Badge> {
    static CACHE: OnceLock<Cache<Option<Badge>>> = OnceLock::new();
    cached(
        CACHE.get_or_init(|| Mutex::new(TimedCache::new(1024))),
        id,
        || async { fetch_cosmetics(id).await.ok().and_then(|c| c.1) },
    )
    .await
}

/// The GraphQL cosmetics query: one id at a time (we cache per id, so batching
/// buys little and keeps the call simple). Returns the paint + badge definitions
/// for that id (only one of them will match a given cosmetic id).
const COSMETICS_QUERY: &str = r#"query($id:[ObjectID!]){cosmetics(list:$id){paints{id name color function angle repeat stops{at color}gradients{function angle repeat stops{at color}}}badges{id name tooltip host{url files{name format}}}}}"#;

async fn fetch_cosmetics(id: &str) -> anyhow::Result<(Option<NamePaint>, Option<Badge>)> {
    let body = serde_json::json!({
        "query": COSMETICS_QUERY,
        "variables": { "id": [id] },
    });
    let resp = shared_client()
        .post(GQL_URL)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    let parsed: GqlResponse = resp.json().await?;
    let cosmetics = parsed.data.map(|d| d.cosmetics).unwrap_or_default();
    let paint = cosmetics.paints.into_iter().find_map(parse_paint);
    let badge = cosmetics.badges.into_iter().find_map(parse_badge);
    Ok((paint, badge))
}

#[derive(Deserialize)]
struct GqlResponse {
    data: Option<GqlData>,
}

#[derive(Deserialize)]
struct GqlData {
    cosmetics: GqlCosmetics,
}

#[derive(Deserialize, Default)]
struct GqlCosmetics {
    #[serde(default)]
    paints: Vec<RawPaint>,
    #[serde(default)]
    badges: Vec<RawBadge>,
}

/// One paint from the GraphQL response. The legacy fields (`function`, `color`,
/// `stops`, `angle`) describe the paint directly; `gradients` is the newer
/// per-gradient form whose first entry, when present, overrides them.
#[derive(Deserialize)]
struct RawPaint {
    name: String,
    /// Signed 32-bit RGBA (as 7TV packs it); `None` for gradient/url paints.
    #[serde(default)]
    color: Option<i64>,
    #[serde(default)]
    function: Option<String>,
    #[serde(default)]
    angle: f64,
    #[serde(default)]
    stops: Vec<RawStop>,
    #[serde(default)]
    gradients: Vec<RawGradient>,
}

#[derive(Deserialize)]
struct RawGradient {
    #[serde(default)]
    function: Option<String>,
    #[serde(default)]
    angle: f64,
    #[serde(default)]
    stops: Vec<RawStop>,
}

#[derive(Deserialize)]
struct RawStop {
    #[serde(default)]
    at: f64,
    /// Signed 32-bit RGBA.
    #[serde(default)]
    color: i64,
}

#[derive(Deserialize)]
struct RawBadge {
    id: String,
    name: String,
    #[serde(default)]
    tooltip: Option<String>,
    host: Host,
}

/// 7TV packs colors as `0xRRGGBBAA` in a signed 32-bit int. Drop alpha (names are
/// drawn opaque) and return packed `0xRRGGBB`.
fn rgba_to_rgb(rgba: i64) -> u32 {
    let v = (rgba as i32) as u32; // reinterpret the sign bits as the raw u32
    (v >> 8) & 0x00ff_ffff
}

fn parse_stops(stops: &[RawStop]) -> Vec<PaintStop> {
    stops
        .iter()
        .map(|s| PaintStop {
            at: s.at as f32,
            color: rgba_to_rgb(s.color),
        })
        .collect()
}

/// Converts a raw paint to a [`NamePaint`]. Linear/radial gradients keep their
/// stops; a `URL`/image paint (or any paint with no usable gradient) collapses to
/// its representative color so the name is still recolored. Returns `None` only
/// when there's nothing renderable at all.
fn parse_paint(raw: RawPaint) -> Option<NamePaint> {
    // Prefer the newer `gradients[0]` form (its function/angle/stops), else the
    // legacy top-level fields.
    let (function, angle, stops) = match raw.gradients.into_iter().next() {
        Some(g) if !g.stops.is_empty() => (g.function, g.angle, g.stops),
        _ => (raw.function.clone(), raw.angle, raw.stops),
    };
    let function = function.unwrap_or_default().to_ascii_uppercase();
    let parsed_stops = parse_stops(&stops);

    let kind = match function.as_str() {
        "LINEAR_GRADIENT" if !parsed_stops.is_empty() => PaintKind::Linear {
            angle: angle as f32,
            stops: parsed_stops,
        },
        "RADIAL_GRADIENT" if !parsed_stops.is_empty() => PaintKind::Radial {
            stops: parsed_stops,
        },
        _ => {
            // URL/solid/unknown: use the flat color if present, else the first
            // gradient stop's color, else give up.
            let color = raw
                .color
                .map(rgba_to_rgb)
                .or_else(|| parsed_stops.first().map(|s| s.color))?;
            PaintKind::Solid(color)
        }
    };
    Some(NamePaint {
        name: raw.name,
        kind,
    })
}

/// Converts a raw 7TV badge to a [`Badge`], picking the largest WEBP image. A
/// badge with no renderable image is dropped.
fn parse_badge(raw: RawBadge) -> Option<Badge> {
    let url = largest_url(&raw.host, "WEBP").or_else(|| largest_url(&raw.host, "PNG"))?;
    Some(Badge {
        id: format!("7tv:{}", raw.id),
        url,
        title: Some(raw.tooltip.unwrap_or(raw.name)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_evicts_expired_entries_and_bounds_bookkeeping() {
        let mut cache = TimedCache::new(2);
        let now = Instant::now();
        cache.get_or_insert("a", now, || 1);
        cache.get_or_insert("b", now, || 2);
        cache.get_or_insert("c", now, || 3);
        assert!(!cache.values.contains_key("a"));
        assert_eq!(cache.order.len(), 2);
        assert_eq!(cache.get_or_insert("b", now, || 9), 2);
        cache.get_or_insert("d", now + COSMETICS_TTL, || 4);
        assert_eq!(cache.values.len(), 1);
        assert_eq!(cache.order.len(), 1);
    }

    #[tokio::test]
    async fn disabling_a_queued_lookup_does_not_cache_empty_cosmetics() {
        use std::{future::Future, task::Poll};

        let permits = FETCHES.acquire_many(8).await.unwrap();
        let first = resolve("disabled-while-queued");
        tokio::pin!(first);
        std::future::poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;

        set_enabled(false);
        drop(permits);
        let cancelled = first.await;
        set_enabled(true);
        assert!(cancelled.is_empty());

        // Hold every permit again: a retry must wait to fetch, not return cached empty data.
        let _permits = FETCHES.acquire_many(8).await.unwrap();
        let retry = resolve("disabled-while-queued");
        tokio::pin!(retry);
        std::future::poll_fn(|cx| {
            assert!(
                retry.as_mut().poll(cx).is_pending(),
                "disabled result was cached"
            );
            Poll::Ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn simultaneous_cosmetics_requests_share_the_fetch() {
        use std::sync::atomic::AtomicUsize;
        let cache = Mutex::new(TimedCache::new(4));
        let calls = AtomicUsize::new(0);
        let fetch = || async {
            calls.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            42
        };
        let (first, second) = tokio::join!(
            cached(&cache, "same-user", fetch),
            cached(&cache, "same-user", fetch)
        );
        assert_eq!((first, second), (42, 42));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn rgba_to_rgb_drops_alpha() {
        // 0xAABBCCDD -> 0xAABBCC. As a signed i32 this is negative.
        let packed = 0xAABBCCDDu32 as i32 as i64;
        assert_eq!(rgba_to_rgb(packed), 0x00AABBCC);
        // Fully opaque white.
        assert_eq!(rgba_to_rgb(0xFFFFFFFFu32 as i32 as i64), 0x00FFFFFF);
    }

    #[test]
    fn parses_linear_gradient_paint() {
        let raw = RawPaint {
            name: "Cool".into(),
            color: None,
            function: Some("LINEAR_GRADIENT".into()),
            angle: 90.0,
            stops: vec![
                RawStop {
                    at: 0.0,
                    color: 0xFF0000FFu32 as i32 as i64,
                },
                RawStop {
                    at: 1.0,
                    color: 0x0000FFFFu32 as i32 as i64,
                },
            ],
            gradients: vec![],
        };
        let paint = parse_paint(raw).unwrap();
        match paint.kind {
            PaintKind::Linear { angle, stops } => {
                assert_eq!(angle, 90.0);
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[0].color, 0xFF0000);
                assert_eq!(stops[1].color, 0x0000FF);
            }
            other => panic!("expected linear, got {other:?}"),
        }
    }

    #[test]
    fn url_paint_collapses_to_solid() {
        // A URL paint with shadows but a flat color falls back to solid.
        let raw = RawPaint {
            name: "Doppler".into(),
            color: Some(0x12345678u32 as i32 as i64),
            function: Some("URL".into()),
            angle: 0.0,
            stops: vec![],
            gradients: vec![],
        };
        let paint = parse_paint(raw).unwrap();
        assert_eq!(paint.kind, PaintKind::Solid(0x123456));
    }

    #[test]
    fn prefers_gradients_array_over_legacy() {
        let raw = RawPaint {
            name: "New".into(),
            color: None,
            function: Some("URL".into()), // legacy says URL …
            angle: 0.0,
            stops: vec![],
            gradients: vec![RawGradient {
                function: Some("LINEAR_GRADIENT".into()), // … but gradients[0] wins
                angle: 45.0,
                stops: vec![
                    RawStop {
                        at: 0.0,
                        color: 0x000000FFu32 as i32 as i64,
                    },
                    RawStop {
                        at: 1.0,
                        color: 0xFFFFFFFFu32 as i32 as i64,
                    },
                ],
            }],
        };
        let paint = parse_paint(raw).unwrap();
        assert!(matches!(paint.kind, PaintKind::Linear { .. }));
    }
}
