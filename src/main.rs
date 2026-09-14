//! pebble-ingest: accepts Pebble Health batches from the Alloy watchapp and
//! writes them to InfluxDB 1.8 as line protocol.
//!
//! Endpoints (all under a shared bearer token):
//!   GET  /healthz                      liveness
//!   POST /v1/pebble/minutes            ingest a batch (minutes, daily, activities)
//!   GET  /v1/pebble/state?device=NAME  newest minute stored for that device
//!   POST /v1/pebble/state {"device"}   same, for clients that can only POST

mod influx;
mod line;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::ServiceExt as _;
use axum::{
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{header, uri::PathAndQuery, HeaderMap, StatusCode, Uri},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tower::util::MapRequest;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

/// Largest request body we accept. ~150 minutes of JSON is ~12 KiB; leave headroom.
const MAX_BODY_BYTES: usize = 256 * 1024;
/// Records older than this are dropped: the watch only retains 7 days.
const MAX_AGE_SECS: i64 = 8 * 86_400;
/// Records further in the future than this are dropped (clock skew allowance).
const MAX_FUTURE_SECS: i64 = 120;

type ApiError = (StatusCode, String);

#[derive(Clone, Debug)]
pub struct Config {
    pub token: String,
    pub influx_url: String,
    pub influx_db: String,
    pub influx_user: Option<String>,
    pub influx_password: Option<String>,
    /// Retention for the default policy, e.g. `104w`. `None` leaves Influx defaults alone.
    pub influx_retention: Option<String>,
    /// Unified `health` database written in the Fitbit schema (Option A).
    /// `None` disables the mirror entirely.
    pub health_db: Option<String>,
    /// Timezone whose local midnight the daily `health` points are stamped at,
    /// so they line up with the Fitbit archive's local-midnight timestamps.
    pub health_tz: chrono_tz::Tz,
    pub listen_addr: SocketAddr,
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let token = std::env::var("INGEST_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("INGEST_TOKEN must be set to a non-empty value"))?;
        let listen_addr: SocketAddr = env_or("LISTEN_ADDR", "0.0.0.0:8088").parse()?;
        Ok(Self {
            token,
            influx_url: env_or("INFLUX_URL", "http://localhost:8086")
                .trim_end_matches('/')
                .to_string(),
            influx_db: env_or("INFLUX_DB", "pebble"),
            influx_user: std::env::var("INFLUX_USER").ok().filter(|s| !s.is_empty()),
            influx_password: std::env::var("INFLUX_PASSWORD").ok(),
            influx_retention: Some(env_or("INFLUX_RETENTION", "104w")).filter(|s| s != "none"),
            health_db: Some(env_or("HEALTH_DB", "health")).filter(|s| !s.is_empty() && s != "none"),
            health_tz: env_or("HEALTH_TZ", "Europe/Amsterdam")
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid HEALTH_TZ: {e}"))?,
            listen_addr,
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

pub struct AppState {
    pub cfg: Config,
    pub http: reqwest::Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // `pebble-ingest healthcheck` is what the Docker HEALTHCHECK runs: the
    // runtime image has no curl, and this keeps it that way.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck().await;
    }

    let cfg = Config::from_env()?;
    let state = Arc::new(AppState {
        cfg: cfg.clone(),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?,
    });

    influx::ensure_database(&state).await;

    let listener = tokio::net::TcpListener::bind(cfg.listen_addr).await?;
    tracing::info!(
        addr = %cfg.listen_addr,
        influx = %cfg.influx_url,
        db = %cfg.influx_db,
        "listening"
    );
    axum::serve(listener, app(state).into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

pub type App = MapRequest<Router, fn(Request) -> Request>;

/// The router wrapped so that request paths are normalised *before* routing.
pub fn app(state: Arc<AppState>) -> App {
    MapRequest::new(
        router(state),
        collapse_leading_slashes as fn(Request) -> Request,
    )
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/pebble/minutes", post(ingest))
        .route(
            "/v1/pebble/state",
            get(state_handler).post(state_post_handler),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// `@moddable/pebbleproxy` (the phone-side proxy the watch's fetch() goes
/// through) joins "host:port/" and "/path" into "host:port//path". Collapse
/// leading slashes so those requests route normally.
fn collapse_leading_slashes(mut req: Request) -> Request {
    let path = req.uri().path();
    if !path.starts_with("//") {
        return req;
    }
    let mut pq = format!("/{}", path.trim_start_matches('/'));
    if let Some(q) = req.uri().query() {
        pq.push('?');
        pq.push_str(q);
    }
    if let Ok(pq) = pq.parse::<PathAndQuery>() {
        let mut parts = req.uri().clone().into_parts();
        parts.path_and_query = Some(pq);
        if let Ok(uri) = Uri::from_parts(parts) {
            *req.uri_mut() = uri;
        }
    }
    req
}

async fn healthcheck() -> anyhow::Result<()> {
    let addr: SocketAddr = env_or("LISTEN_ADDR", "0.0.0.0:8088").parse()?;
    let url = format!("http://127.0.0.1:{}/healthz", addr.port());
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?
        .get(&url)
        .send()
        .await?;
    anyhow::ensure!(
        resp.status().is_success(),
        "healthz returned {}",
        resp.status()
    );
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}

// ---------- payload ----------

#[derive(Deserialize)]
pub struct Batch {
    device: String,
    #[serde(default)]
    minutes: Vec<MinuteRecord>,
    #[serde(default)]
    daily: Option<Daily>,
    #[serde(default)]
    activities: Vec<Activity>,
}

/// A minute either as an object (`{"t":..,"steps":..}`, the documented form)
/// or as a compact row `[t, steps, vmc, orientation, light, hr]` (what the
/// watchapp sends: the watch JS heap is a few KB, so bytes and objects count).
/// Missing trailing row fields are treated as absent; `hr` of 0 is absent.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum MinuteRecord {
    Row(Vec<i64>),
    Object(Minute),
}

impl MinuteRecord {
    fn minute(&self) -> Option<Minute> {
        match self {
            MinuteRecord::Object(m) => Some(m.clone()),
            MinuteRecord::Row(r) => {
                let at = |i: usize| r.get(i).copied();
                Some(Minute {
                    t: at(0)?,
                    steps: at(1),
                    vmc: at(2),
                    orientation: at(3),
                    light: at(4),
                    hr: at(5),
                })
            }
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct Minute {
    t: i64,
    #[serde(default)]
    steps: Option<i64>,
    #[serde(default)]
    hr: Option<i64>,
    #[serde(default)]
    vmc: Option<i64>,
    #[serde(default)]
    orientation: Option<i64>,
    #[serde(default)]
    light: Option<i64>,
}

#[derive(Deserialize)]
pub struct Daily {
    t: i64,
    #[serde(default)]
    steps: Option<i64>,
    #[serde(default)]
    sleep_s: Option<i64>,
    #[serde(default)]
    sleep_restful_s: Option<i64>,
    #[serde(default)]
    active_s: Option<i64>,
    #[serde(default)]
    distance_m: Option<i64>,
    #[serde(default)]
    active_kcal: Option<i64>,
    #[serde(default)]
    resting_kcal: Option<i64>,
    #[serde(default)]
    hr_resting: Option<i64>,
}

#[derive(Deserialize)]
pub struct Activity {
    #[serde(rename = "type")]
    kind: String,
    start: i64,
    end: i64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct IngestReply {
    /// Minute records written.
    accepted: usize,
    /// Minute records dropped (timestamp out of range).
    rejected: usize,
    /// Newest minute timestamp (unix seconds) now stored for this device.
    highwater: Option<i64>,
}

// ---------- rendering ----------

struct Rendered {
    body: String,
    accepted: usize,
    rejected: usize,
    highwater: Option<i64>,
}

/// Turn a validated batch into line protocol. Pure, so it is unit-testable
/// without Influx; `now` is injected for the same reason.
fn render_batch(batch: &Batch, device: &str, now: i64) -> Rendered {
    let mut body = String::with_capacity(batch.minutes.len() * 96);
    let mut accepted = 0usize;
    let mut rejected = 0usize;
    let mut highwater: Option<i64> = None;

    let in_range = |t: i64| t <= now + MAX_FUTURE_SECS && t >= now - MAX_AGE_SECS;

    for rec in &batch.minutes {
        let Some(m) = rec.minute() else {
            rejected += 1;
            continue;
        };
        if !in_range(m.t) {
            rejected += 1;
            continue;
        }
        let t = m.t - m.t.rem_euclid(60); // snap to the minute -> idempotent writes

        let mut p = line::Point::new("pebble_minute", t);
        p.tag("device", device).tag("source", "alloy");
        p.ifield("steps", m.steps)
            .ifield("hr", m.hr.filter(|&hr| hr > 0)) // 0 bpm means "no sample"
            .ifield("vmc", m.vmc)
            .ifield("orientation", m.orientation)
            .ifield("light", m.light);

        match p.finish() {
            Some(l) => {
                body.push_str(&l);
                body.push('\n');
                accepted += 1;
                highwater = Some(highwater.map_or(t, |h| h.max(t)));
            }
            None => rejected += 1,
        }
    }

    if let Some(d) = &batch.daily {
        if in_range(d.t) {
            let t = d.t - d.t.rem_euclid(86_400);
            let mut p = line::Point::new("pebble_daily", t);
            p.tag("device", device);
            p.ifield("steps", d.steps)
                .ifield("sleep_s", d.sleep_s)
                .ifield("sleep_restful_s", d.sleep_restful_s)
                .ifield("active_s", d.active_s)
                .ifield("distance_m", d.distance_m)
                .ifield("active_kcal", d.active_kcal)
                .ifield("resting_kcal", d.resting_kcal)
                .ifield("hr_resting", d.hr_resting.filter(|&hr| hr > 0));
            if let Some(l) = p.finish() {
                body.push_str(&l);
                body.push('\n');
            }
        }
    }

    for a in &batch.activities {
        if a.end <= a.start || !in_range(a.start) {
            continue;
        }
        let Some(kind) = sanitize_tag(&a.kind, 32) else {
            continue;
        };
        let mut p = line::Point::new("pebble_activity", a.start);
        p.tag("device", device).tag("type", &kind);
        p.ifield("duration_s", Some(a.end - a.start));
        if let Some(l) = p.finish() {
            body.push_str(&l);
            body.push('\n');
        }
    }

    Rendered {
        body,
        accepted,
        rejected,
        highwater,
    }
}

// ---------- health mirror (Option A) ----------
//
// Writes the continuity series into the unified `health` database using the
// Fitbit archive's schema, units and types (see docs/consolidation-plan.md):
// float fields, kilometres, minutes, and daily points stamped at local
// midnight so they line up with the copied Fitbit data.

/// Unix second of local midnight, in `tz`, of the day containing `t`.
fn local_midnight(t: i64, tz: chrono_tz::Tz) -> Option<i64> {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::DateTime::from_timestamp(t, 0)?.with_timezone(&tz);
    let d = dt.date_naive();
    tz.with_ymd_and_hms(d.year(), d.month(), d.day(), 0, 0, 0)
        .single()
        .map(|m| m.timestamp())
}

/// Sum two optional integers as an f64 when at least one is present.
fn opt_sum(a: Option<i64>, b: Option<i64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        _ => Some((a.unwrap_or(0) + b.unwrap_or(0)) as f64),
    }
}

fn render_health(batch: &Batch, device: &str, now: i64, tz: chrono_tz::Tz) -> String {
    let mut body = String::new();
    let in_range = |t: i64| t <= now + MAX_FUTURE_SECS && t >= now - MAX_AGE_SECS;
    let f = |v: Option<i64>| v.map(|x| x as f64);

    if let Some(d) = &batch.daily {
        if in_range(d.t) {
            if let Some(day) = local_midnight(d.t, tz) {
                // activities: the daily summary Grafana plots for steps,
                // distance, calories and resting HR.
                let mut p = line::Point::new("activities", day);
                p.tag("device", device);
                p.ffield("steps", f(d.steps))
                    .ffield("distance_total", d.distance_m.map(|m| m as f64 / 1000.0))
                    .ffield("caloriesOut", opt_sum(d.active_kcal, d.resting_kcal))
                    .ffield("caloriesBMR", f(d.resting_kcal))
                    .ffield("activityCalories", f(d.active_kcal))
                    .ffield("activeMinutes", d.active_s.map(|s| s as f64 / 60.0))
                    .ffield("restingHeartRate", f(d.hr_resting.filter(|&hr| hr > 0)));
                if let Some(l) = p.finish() {
                    body.push_str(&l);
                    body.push('\n');
                }

                // sleepsummaries: total sleep, plus restful sleep mapped to
                // stages.deep as a documented approximation (the watch has no
                // true sleep-stage breakdown).
                let mut s = line::Point::new("sleepsummaries", day);
                s.tag("device", device);
                s.ffield("totalMinutesAsleep", d.sleep_s.map(|x| x as f64 / 60.0))
                    .ffield("stages.deep", d.sleep_restful_s.map(|x| x as f64 / 60.0));
                if let Some(l) = s.finish() {
                    body.push_str(&l);
                    body.push('\n');
                }
            }
        }
    }

    // heartrate: one intraday point per minute that has a sample, so the HR
    // chart is continuous across the Fitbit/Pebble seam.
    for rec in &batch.minutes {
        let Some(m) = rec.minute() else { continue };
        if !in_range(m.t) {
            continue;
        }
        let Some(hr) = m.hr.filter(|&hr| hr > 0) else {
            continue;
        };
        let t = m.t - m.t.rem_euclid(60);
        let mut p = line::Point::new("heartrate", t);
        p.tag("device", device).ffield("value", Some(hr as f64));
        if let Some(l) = p.finish() {
            body.push_str(&l);
            body.push('\n');
        }
    }

    body
}

// ---------- handlers ----------

async fn ingest(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(batch): Json<Batch>,
) -> Result<Json<IngestReply>, ApiError> {
    authorize(&headers, &st.cfg.token)?;

    let device =
        sanitize_device(&batch.device).ok_or((StatusCode::BAD_REQUEST, "bad device".into()))?;

    let now = unix_now();
    let r = render_batch(&batch, &device, now);

    if !r.body.is_empty() {
        influx::write(&st, &st.cfg.influx_db, &r.body)
            .await
            .map_err(|e| {
                tracing::error!(device = %device, error = %e, "influx write failed");
                (StatusCode::BAD_GATEWAY, e.to_string())
            })?;
    }

    // Option A: mirror the continuity series into the unified `health` database
    // in the Fitbit schema. A mirror failure must not fail the ingest — the
    // native pebble write already succeeded — so it is logged, not returned.
    if let Some(health_db) = &st.cfg.health_db {
        let health = render_health(&batch, &device, now, st.cfg.health_tz);
        if !health.is_empty() {
            if let Err(e) = influx::write(&st, health_db, &health).await {
                tracing::warn!(device = %device, error = %e, "health mirror write failed");
            }
        }
    }

    tracing::info!(
        device = %device,
        accepted = r.accepted,
        rejected = r.rejected,
        daily = batch.daily.is_some(),
        activities = batch.activities.len(),
        highwater = ?r.highwater,
        "batch written"
    );
    Ok(Json(IngestReply {
        accepted: r.accepted,
        rejected: r.rejected,
        highwater: r.highwater,
    }))
}

#[derive(Deserialize)]
struct StateQuery {
    device: String,
}

/// Lets the watch resume after a reinstall: returns the newest minute we hold.
async fn state_handler(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<StateQuery>,
) -> Result<Json<IngestReply>, ApiError> {
    state_for(&st, &headers, &q.device).await
}

/// POST variant of the same lookup: the watch runtime's fetch() is only
/// reliable for POST with a body, so the watchapp uses this one.
async fn state_post_handler(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(q): Json<StateQuery>,
) -> Result<Json<IngestReply>, ApiError> {
    state_for(&st, &headers, &q.device).await
}

async fn state_for(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    device: &str,
) -> Result<Json<IngestReply>, ApiError> {
    authorize(headers, &st.cfg.token)?;
    let device = sanitize_device(device).ok_or((StatusCode::BAD_REQUEST, "bad device".into()))?;

    let hw = influx::last_minute(st, &device)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    Ok(Json(IngestReply {
        accepted: 0,
        rejected: 0,
        highwater: hw,
    }))
}

// ---------- helpers ----------

fn authorize(headers: &HeaderMap, expected: &str) -> Result<(), ApiError> {
    let got = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if ct_eq(got.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "unauthorized".into()))
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Tag values are kept to a safe alphabet rather than escaped, which also
/// caps series cardinality from a misbehaving client.
fn sanitize_tag(s: &str, max_len: usize) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s.len() > max_len {
        return None;
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some(s.to_string())
    } else {
        None
    }
}

fn sanitize_device(s: &str) -> Option<String> {
    sanitize_tag(s, 48)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::{collections::HashMap, sync::Mutex};
    use tower::ServiceExt;

    const NOW: i64 = 1_757_846_400; // 2025-09-14T10:40:00Z

    fn batch(json: serde_json::Value) -> Batch {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn sanitizes_device_names() {
        assert_eq!(sanitize_device(" pt2-peter "), Some("pt2-peter".into()));
        assert_eq!(sanitize_device("a_b-1"), Some("a_b-1".into()));
        assert_eq!(sanitize_device(""), None);
        assert_eq!(sanitize_device("has space"), None);
        assert_eq!(sanitize_device("it's"), None);
        assert_eq!(sanitize_device(&"x".repeat(49)), None);
    }

    #[test]
    fn snaps_to_minute_and_tracks_highwater() {
        let b = batch(json!({
            "device": "pt2",
            "minutes": [
                {"t": NOW + 17, "steps": 12, "hr": 68, "vmc": 340, "orientation": 97, "light": 2},
                {"t": NOW - 60, "steps": 0, "hr": 0, "vmc": 12, "orientation": 97, "light": 2}
            ]
        }));
        let r = render_batch(&b, "pt2", NOW);
        assert_eq!(r.accepted, 2);
        assert_eq!(r.rejected, 0);
        assert_eq!(r.highwater, Some(NOW));
        let lines: Vec<&str> = r.body.lines().collect();
        assert_eq!(
            lines[0],
            format!("pebble_minute,device=pt2,source=alloy steps=12i,hr=68i,vmc=340i,orientation=97i,light=2i {NOW}")
        );
        // hr=0 means "no sample": omitted, not written as zero
        assert_eq!(
            lines[1],
            format!("pebble_minute,device=pt2,source=alloy steps=0i,vmc=12i,orientation=97i,light=2i {}", NOW - 60)
        );
    }

    #[test]
    fn accepts_compact_rows() {
        let b = batch(json!({
            "device": "pt2",
            "minutes": [
                [NOW, 12, 340, 97, 2, 68],
                [NOW - 60, 0, 12, 97, 2, 0],
                [NOW - 120, 5],
                [],
                {"t": NOW - 180, "vmc": 7}
            ]
        }));
        let r = render_batch(&b, "pt2", NOW);
        assert_eq!(r.accepted, 4);
        assert_eq!(r.rejected, 1);
        let lines: Vec<&str> = r.body.lines().collect();
        assert_eq!(
            lines[0],
            format!("pebble_minute,device=pt2,source=alloy steps=12i,hr=68i,vmc=340i,orientation=97i,light=2i {NOW}")
        );
        assert_eq!(
            lines[1],
            format!("pebble_minute,device=pt2,source=alloy steps=0i,vmc=12i,orientation=97i,light=2i {}", NOW - 60)
        );
        assert_eq!(
            lines[2],
            format!(
                "pebble_minute,device=pt2,source=alloy steps=5i {}",
                NOW - 120
            )
        );
        assert_eq!(
            lines[3],
            format!("pebble_minute,device=pt2,source=alloy vmc=7i {}", NOW - 180)
        );
    }

    #[test]
    fn rejects_out_of_range_and_empty_records() {
        let b = batch(json!({
            "device": "pt2",
            "minutes": [
                {"t": NOW + 3600, "steps": 1},          // future
                {"t": NOW - 9 * 86_400, "steps": 1},    // too old
                {"t": NOW},                              // no fields
                {"t": NOW - 120, "vmc": 5}
            ]
        }));
        let r = render_batch(&b, "pt2", NOW);
        assert_eq!(r.accepted, 1);
        assert_eq!(r.rejected, 3);
        assert_eq!(r.highwater, Some(NOW - 120));
        assert_eq!(r.body.lines().count(), 1);
    }

    #[test]
    fn renders_daily_and_activities() {
        let b = batch(json!({
            "device": "pt2",
            "daily": {"t": NOW, "steps": 8231, "sleep_s": 27000, "hr_resting": 0},
            "activities": [
                {"type": "walk", "start": NOW - 2400, "end": NOW},
                {"type": "bad type!", "start": NOW - 100, "end": NOW},
                {"type": "run", "start": NOW, "end": NOW}
            ]
        }));
        let r = render_batch(&b, "pt2", NOW);
        assert_eq!(r.accepted, 0);
        assert_eq!(r.highwater, None);
        let day = NOW - NOW.rem_euclid(86_400);
        let lines: Vec<&str> = r.body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            format!("pebble_daily,device=pt2 steps=8231i,sleep_s=27000i {day}")
        );
        assert_eq!(
            lines[1],
            format!(
                "pebble_activity,device=pt2,type=walk duration_s=2400i {}",
                NOW - 2400
            )
        );
    }

    #[test]
    fn health_mirror_converts_units_types_and_timestamp() {
        // A winter day (CET = UTC+1): 2025-01-15. Pick a daily timestamp in the
        // middle of that local day and confirm it snaps to local midnight.
        let noon_cet = 1_736_940_000; // 2025-01-15 11:00:00 UTC = 12:00 CET
        let b = batch(json!({
            "device": "pt2",
            "daily": {
                "t": noon_cet, "steps": 6349, "distance_m": 4709,
                "active_kcal": 940, "resting_kcal": 1680, "hr_resting": 52,
                "sleep_s": 27660, "sleep_restful_s": 3360, "active_s": 3300
            },
            "minutes": [
                [noon_cet, 5, 30, 97, 2, 61],
                [noon_cet + 60, 0, 12, 97, 1, 0]
            ]
        }));
        let body = render_health(&b, "pt2", noon_cet, chrono_tz::Europe::Amsterdam);
        let lines: Vec<&str> = body.lines().collect();

        // local midnight of 2025-01-15 in Amsterdam = 2025-01-14 23:00:00 UTC
        let midnight = 1_736_895_600;
        assert_eq!(
            lines[0],
            format!(
                "activities,device=pt2 steps=6349.0,distance_total=4.709,caloriesOut=2620.0,\
caloriesBMR=1680.0,activityCalories=940.0,activeMinutes=55.0,restingHeartRate=52.0 {midnight}"
            )
        );
        assert_eq!(
            lines[1],
            format!(
                "sleepsummaries,device=pt2 totalMinutesAsleep=461.0,stages.deep=56.0 {midnight}"
            )
        );
        // only the minute with hr>0 becomes a heartrate point
        assert_eq!(
            lines[2],
            format!("heartrate,device=pt2 value=61.0 {noon_cet}")
        );
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn health_mirror_is_empty_without_daily_or_hr() {
        let b = batch(json!({"device": "pt2", "minutes": [[NOW, 5, 30, 97, 2, 0]]}));
        assert!(render_health(&b, "pt2", NOW, chrono_tz::Europe::Amsterdam).is_empty());
    }

    #[test]
    fn replaying_a_batch_renders_identically() {
        let b = batch(json!({"device": "pt2", "minutes": [{"t": NOW + 5, "vmc": 1}]}));
        let a = render_batch(&b, "pt2", NOW).body;
        let c = render_batch(&b, "pt2", NOW + 30).body;
        assert_eq!(a, c);
    }

    // --- HTTP-level tests against a mock Influx ---

    async fn mock_influx() -> (String, Arc<Mutex<Vec<String>>>) {
        let writes: Arc<Mutex<Vec<String>>> = Arc::default();
        let w = writes.clone();
        let app = Router::new()
            .route(
                "/write",
                post(move |body: String| {
                    let w = w.clone();
                    async move {
                        w.lock().unwrap().push(body);
                        StatusCode::NO_CONTENT
                    }
                }),
            )
            .route(
                "/query",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    if q["q"].starts_with("SELECT") && q["q"].contains("'pt2'") {
                        Json(json!({"results":[{"series":[{"values":[[NOW, 340]]}]}]}))
                    } else {
                        Json(json!({"results":[{}]}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), writes)
    }

    fn app(influx_url: &str) -> App {
        super::app(Arc::new(AppState {
            cfg: Config {
                token: "dev-token".into(),
                influx_url: influx_url.into(),
                influx_db: "pebble".into(),
                influx_user: None,
                influx_password: None,
                influx_retention: None,
                health_db: None,
                health_tz: chrono_tz::Europe::Amsterdam,
                listen_addr: "127.0.0.1:0".parse().unwrap(),
            },
            http: reqwest::Client::new(),
        }))
    }

    async fn body_json<T: serde::de::DeserializeOwned>(resp: axum::response::Response) -> T {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn rejects_missing_or_wrong_token() {
        let app = app("http://127.0.0.1:1");
        for auth in [None, Some("Bearer nope"), Some("dev-token")] {
            let mut req =
                Request::post("/v1/pebble/minutes").header("content-type", "application/json");
            if let Some(a) = auth {
                req = req.header("authorization", a);
            }
            let resp = app
                .clone()
                .oneshot(req.body(Body::from(r#"{"device":"pt2"}"#)).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "auth={auth:?}");
        }
    }

    #[tokio::test]
    async fn ingests_and_writes_to_influx() {
        let (url, writes) = mock_influx().await;
        let app = app(&url);
        let now = unix_now();
        let payload = json!({
            "device": "pt2",
            "minutes": [{"t": now - 300, "steps": 12, "hr": 68, "vmc": 340, "orientation": 97, "light": 2}]
        });
        let req = Request::post("/v1/pebble/minutes")
            .header("content-type", "application/json")
            .header("authorization", "Bearer dev-token")
            .body(Body::from(payload.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let reply: IngestReply = body_json(resp).await;
        let expected_t = (now - 300) - (now - 300).rem_euclid(60);
        assert_eq!(
            reply,
            IngestReply {
                accepted: 1,
                rejected: 0,
                highwater: Some(expected_t)
            }
        );
        let w = writes.lock().unwrap();
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("pebble_minute,device=pt2,source=alloy steps=12i,hr=68i"));
    }

    #[tokio::test]
    async fn bad_device_is_400_and_nothing_is_written() {
        let (url, writes) = mock_influx().await;
        let req = Request::post("/v1/pebble/minutes")
            .header("content-type", "application/json")
            .header("authorization", "Bearer dev-token")
            .body(Body::from(r#"{"device":"no good","minutes":[]}"#))
            .unwrap();
        let resp = app(&url).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn state_returns_highwater_from_influx() {
        let (url, _) = mock_influx().await;
        let app = app(&url);
        let req = Request::get("/v1/pebble/state?device=pt2")
            .header("authorization", "Bearer dev-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let reply: IngestReply = body_json(resp).await;
        assert_eq!(reply.highwater, Some(NOW));

        let req = Request::get("/v1/pebble/state?device=unknown")
            .header("authorization", "Bearer dev-token")
            .body(Body::empty())
            .unwrap();
        let reply: IngestReply = body_json(app.oneshot(req).await.unwrap()).await;
        assert_eq!(reply.highwater, None);
    }

    #[tokio::test]
    async fn state_accepts_post_and_doubled_slashes() {
        let (url, _) = mock_influx().await;
        let app = app(&url);

        // POST body variant, as the watchapp sends it
        let req = Request::post("/v1/pebble/state")
            .header("content-type", "application/json")
            .header("authorization", "Bearer dev-token")
            .body(Body::from(r#"{"device":"pt2"}"#))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let reply: IngestReply = body_json(resp).await;
        assert_eq!(reply.highwater, Some(NOW));

        // "//path" as produced by @moddable/pebbleproxy
        let req = Request::get("//v1/pebble/state?device=pt2")
            .header("authorization", "Bearer dev-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let reply: IngestReply = body_json(resp).await;
        assert_eq!(reply.highwater, Some(NOW));

        let req = Request::get("///healthz").body(Body::empty()).unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn influx_failure_is_502() {
        let app = app("http://127.0.0.1:1");
        let req = Request::post("/v1/pebble/minutes")
            .header("content-type", "application/json")
            .header("authorization", "Bearer dev-token")
            .body(Body::from(
                json!({"device":"pt2","minutes":[{"t": unix_now(), "vmc": 1}]}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }
}
