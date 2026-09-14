# Pebble Time 2 → InfluxDB 1.8 health pipeline

Implementation plan for route 2: an Alloy watchapp reads Pebble Health, POSTs batches
to a Rust ingest service, which writes InfluxDB line protocol. Grafana on top.

---

## 0. A note on the language split

Rust cannot run on the watch. Pebble apps are either C (compiled against the Pebble SDK)
or JavaScript (Alloy, the Moddable-based runtime). There is no Rust target, and the Alloy
FFI escape hatch expects C symbols — linking a `staticlib` through it is possible in
principle but is a research project, not a foundation.

So the split is:

| Component | Language | Why |
|---|---|---|
| Watchapp (read health, batch, POST) | JavaScript (Alloy) | Only supported option; ~120 lines |
| Ingest service (auth, transform, write) | **Rust** | All the real logic lives here |
| InfluxDB 1.8 + Grafana | — | Unchanged from your existing setup |

The watchapp is deliberately dumb: it reads minute records and posts them as JSON. Every
decision about schema, tags, retention, dedup and downsampling happens in the Rust service,
where you can iterate without reflashing a watch.

---

## 1. Architecture

```
┌────────────────────┐
│  Pebble Time 2     │   Health.history.byMinute()
│  Alloy app,        │   Health.metric.query()
│  wakeup-launched   │   Health.activity.iterate()
└─────────┬──────────┘
          │ AppMessage (BLE)
┌─────────▼──────────┐
│  Phone (PKJS)      │   @moddable/pebbleproxy
│  network proxy     │   — the request actually egresses here
└─────────┬──────────┘
          │ HTTPS POST /v1/pebble/minutes   (Bearer token)
┌─────────▼──────────┐
│  pebble-ingest     │   Rust / axum
│  (Raspberry Pi)    │   validate → line protocol → write
└─────────┬──────────┘
          │ POST /write?db=pebble&precision=s
┌─────────▼──────────┐
│  InfluxDB 1.8      │ ──────► Grafana
└────────────────────┘
```

**Three consequences worth designing around:**

1. The request leaves from your *phone's* network, not your LAN. When you're away from
   home the Pi must still be reachable. Options: Tailscale on the phone (cleanest),
   or a Caddy reverse proxy with a real certificate on a public hostname.
2. The watch has no clock authority and limited heap. Keep batches small, keep the
   payload flat, and never put InfluxDB credentials on the watch.
3. **The uploader only runs when the app runs.** See the next section — this is the
   single biggest structural decision in the plan, and the answer is not "run it
   continuously".

### Execution model: why this is a plain app, not a watchface or a worker

There are three ways to get code running on a Pebble, and only one of them fits:

| | Can it HTTP? | Cost |
|---|---|---|
| Watchface | Yes — resident, syncs on a timer | Locks you into one self-built watchface |
| Background worker | **No** | Worker has HealthService and DataLogging but *no AppMessage*, and the Alloy `fetch()` proxy is built on AppMessage. Also C-only, and only one worker may exist on the whole watch |
| Ordinary app, wakeup-launched | Yes | Brief foreground takeover when the wakeup fires |

The worker is a dead end for this design: without AppMessage it cannot reach the network
at all, so it could only write to DataLogging and hand off to a phone-side companion app —
a different architecture, in C, with an Android app attached.

The thing that makes the ordinary-app route work is that **PebbleOS retains seven days of
minute-level health history**. The OS is already doing the buffering a background worker
would have done. Nothing needs to run continuously; the uploader just has to run more
often than once every six days. A wakeup every 12 hours leaves a very large margin, and
you can open the app by hand any time you want a sync now.

---

## 2. Data model

### `pebble_minute` — the core series

| | |
|---|---|
| Tags | `device` (watch serial or a name you pick), `source=alloy` |
| Fields | `steps` (int), `hr` (int), `vmc` (int), `orientation` (int), `light` (int) |
| Timestamp | minute boundary, second precision |

Background HR sampling defaults to every 10 minutes, so most minutes will have no `hr`.
Just omit the field — Influx handles sparse fields fine, and Grafana's `fill(null)` plus
`connectNulls` renders it as a gappy line, which is honest about what the sensor did.

**Idempotency comes free.** InfluxDB overwrites a point with the same
(measurement, tag set, timestamp). Snap every timestamp to the minute boundary and
replays become harmless — which means the watch can be sloppy about retries and you can
re-run a backfill without creating duplicates. This is the single most important property
of the design; don't break it by adding a random tag.

### `pebble_daily` — rollups the minute data can't give you

Sleep and calories aren't in the minute records, so read them separately via
`Health.metric.query()` and write one point per sync, timestamped at the start of the day.
Same overwrite trick: the day's point gets rewritten with better values as the day fills in.

Fields: `steps`, `sleep_s`, `sleep_restful_s`, `active_s`, `distance_m`,
`active_kcal`, `resting_kcal`, `hr_resting`.

### `pebble_activity` — detected walks/runs/workouts

One point per session: tag `type` (walk/run/sleep/restful_sleep/open_workout),
field `duration_s`, timestamped at session start.

---

## 3. Phases

Each phase is independently testable. Don't skip the ordering — phase 2 lets you build
the whole Rust side with `curl` before you fight Bluetooth.

| Phase | Deliverable | Done when |
|---|---|---|
| 1 | Environment | Pebble SDK builds the hello-world; Influx 1.8 + Grafana up |
| 2 | Rust ingest service | `curl` a synthetic batch → points visible in Influx |
| 3 | Alloy app reads health | `console.log` shows real minute records in the emulator |
| 4 | Networking | First real POST from a physical watch lands in Influx |
| 5 | Checkpointing + backfill | Reinstall the app, no gap and no duplicates |
| 6 | Wakeup chain | Watch syncs unattended overnight, on someone else's watchface |
| 7 | Daily + activities | Sleep chart in Grafana |
| 8 | Hardening | Runs for a week unattended |

---

## 4. Phase 1 — Environment

```bash
# InfluxDB 1.8 + Grafana (skip if you already have these)
docker run -d --name influxdb --restart always \
  -p 8086:8086 -v ~/influxdb/data:/var/lib/influxdb/data influxdb:1.8

docker exec influxdb influx -execute 'CREATE DATABASE pebble'
docker exec influxdb influx -execute \
  'CREATE RETENTION POLICY "raw" ON "pebble" DURATION 104w REPLICATION 1 DEFAULT'
```

Two years of minute data is roughly a million points per field — trivial for Influx on a Pi.
Don't bother with continuous queries until you actually feel the query latency.

For the watch side, install the Pebble SDK from `developer.repebble.com/sdk/`. The emulator
can now fake health data, which means most of phase 3 can happen without wearing anything.

---

## 5. Phase 2 — The Rust ingest service

### `Cargo.toml`

```toml
[package]
name = "pebble-ingest"
version = "0.1.0"
edition = "2021"

[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["full"] }
tower-http = { version = "0.6", features = ["trace", "limit"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
thiserror = "2"
anyhow = "1"
```

(Check current majors before you `cargo build` — these move.)

### Wire format

Keep it boring and greppable for v1:

```json
{
  "device": "pt2-peter",
  "minutes": [
    { "t": 1757846400, "steps": 12, "hr": 68, "vmc": 340, "orientation": 97, "light": 2 },
    { "t": 1757846460, "steps": 0,  "vmc": 12,  "orientation": 97, "light": 2 }
  ],
  "daily": {
    "t": 1757808000, "steps": 8231, "sleep_s": 27000, "sleep_restful_s": 9100,
    "active_s": 3300, "distance_m": 6120, "active_kcal": 410, "resting_kcal": 1680
  },
  "activities": [
    { "type": "walk", "start": 1757840000, "end": 1757842400 }
  ]
}
```

If payload size bites later (it will, past ~150 minutes per batch), switch `minutes` to a
columnar array-of-arrays with a header — it roughly halves the bytes. Do that as an
optimisation, not as v1.

### `src/main.rs`

```rust
use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

mod influx;
mod line;

#[derive(Clone)]
struct Config {
    token: String,
    influx_url: String,
    influx_db: String,
}

struct AppState {
    cfg: Config,
    http: reqwest::Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let cfg = Config {
        token: std::env::var("INGEST_TOKEN")?,
        influx_url: std::env::var("INFLUX_URL")
            .unwrap_or_else(|_| "http://localhost:8086".into()),
        influx_db: std::env::var("INFLUX_DB").unwrap_or_else(|_| "pebble".into()),
    };

    let state = Arc::new(AppState {
        cfg,
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
    });

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/pebble/minutes", post(ingest))
        .route("/v1/pebble/state", get(state_handler))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(256 * 1024))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 8088));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------- payload ----------

#[derive(Deserialize)]
struct Batch {
    device: String,
    #[serde(default)]
    minutes: Vec<Minute>,
    #[serde(default)]
    daily: Option<Daily>,
    #[serde(default)]
    activities: Vec<Activity>,
}

#[derive(Deserialize)]
struct Minute {
    t: i64,
    #[serde(default)] steps: Option<i64>,
    #[serde(default)] hr: Option<i64>,
    #[serde(default)] vmc: Option<i64>,
    #[serde(default)] orientation: Option<i64>,
    #[serde(default)] light: Option<i64>,
}

#[derive(Deserialize)]
struct Daily {
    t: i64,
    #[serde(default)] steps: Option<i64>,
    #[serde(default)] sleep_s: Option<i64>,
    #[serde(default)] sleep_restful_s: Option<i64>,
    #[serde(default)] active_s: Option<i64>,
    #[serde(default)] distance_m: Option<i64>,
    #[serde(default)] active_kcal: Option<i64>,
    #[serde(default)] resting_kcal: Option<i64>,
    #[serde(default)] hr_resting: Option<i64>,
}

#[derive(Deserialize)]
struct Activity {
    #[serde(rename = "type")]
    kind: String,
    start: i64,
    end: i64,
}

#[derive(Serialize)]
struct IngestReply {
    accepted: usize,
    highwater: Option<i64>,
}

// ---------- handlers ----------

async fn ingest(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(batch): Json<Batch>,
) -> Result<Json<IngestReply>, (StatusCode, String)> {
    authorize(&headers, &st.cfg.token)?;

    let device = sanitize_device(&batch.device)
        .ok_or((StatusCode::BAD_REQUEST, "bad device".into()))?;

    let now = chrono_now();
    let mut body = String::with_capacity(batch.minutes.len() * 80);
    let mut accepted = 0usize;
    let mut highwater: Option<i64> = None;

    for m in &batch.minutes {
        // Reject nonsense timestamps: nothing older than 8 days, nothing in the future.
        if m.t > now + 120 || m.t < now - 8 * 86_400 {
            continue;
        }
        let t = m.t - (m.t % 60); // snap to the minute → idempotent writes

        let mut p = line::Point::new("pebble_minute", t);
        p.tag("device", &device).tag("source", "alloy");
        p.ifield("steps", m.steps)
            .ifield("hr", m.hr)
            .ifield("vmc", m.vmc)
            .ifield("orientation", m.orientation)
            .ifield("light", m.light);

        if let Some(l) = p.finish() {
            body.push_str(&l);
            body.push('\n');
            accepted += 1;
            highwater = Some(highwater.map_or(t, |h: i64| h.max(t)));
        }
    }

    if let Some(d) = &batch.daily {
        let t = d.t - (d.t % 86_400);
        let mut p = line::Point::new("pebble_daily", t);
        p.tag("device", &device);
        p.ifield("steps", d.steps)
            .ifield("sleep_s", d.sleep_s)
            .ifield("sleep_restful_s", d.sleep_restful_s)
            .ifield("active_s", d.active_s)
            .ifield("distance_m", d.distance_m)
            .ifield("active_kcal", d.active_kcal)
            .ifield("resting_kcal", d.resting_kcal)
            .ifield("hr_resting", d.hr_resting);
        if let Some(l) = p.finish() {
            body.push_str(&l);
            body.push('\n');
        }
    }

    for a in &batch.activities {
        if a.end <= a.start {
            continue;
        }
        let mut p = line::Point::new("pebble_activity", a.start);
        p.tag("device", &device).tag("type", &a.kind);
        p.ifield("duration_s", Some(a.end - a.start));
        if let Some(l) = p.finish() {
            body.push_str(&l);
            body.push('\n');
        }
    }

    if !body.is_empty() {
        influx::write(&st, &body)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    }

    tracing::info!(device = %device, accepted, "batch written");
    Ok(Json(IngestReply { accepted, highwater }))
}

#[derive(Deserialize)]
struct StateQuery {
    device: String,
}

/// Lets the watch resume after a reinstall: returns the newest minute we already hold.
async fn state_handler(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<StateQuery>,
) -> Result<Json<IngestReply>, (StatusCode, String)> {
    authorize(&headers, &st.cfg.token)?;
    let device = sanitize_device(&q.device)
        .ok_or((StatusCode::BAD_REQUEST, "bad device".into()))?;

    let hw = influx::last_minute(&st, &device)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    Ok(Json(IngestReply { accepted: 0, highwater: hw }))
}

// ---------- helpers ----------

fn authorize(headers: &HeaderMap, expected: &str) -> Result<(), (StatusCode, String)> {
    let got = headers
        .get(axum::http::header::AUTHORIZATION)
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

/// Device names become tag values; keep them to a safe alphabet rather than escaping.
fn sanitize_device(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s.len() > 48 {
        return None;
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        Some(s.to_string())
    } else {
        None
    }
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
```

### `src/line.rs` — line protocol builder

```rust
pub struct Point {
    measurement: String,
    tags: String,
    fields: String,
    ts: i64,
}

impl Point {
    pub fn new(measurement: &str, ts: i64) -> Self {
        Self {
            measurement: measurement.to_string(),
            tags: String::new(),
            fields: String::new(),
            ts,
        }
    }

    pub fn tag(&mut self, k: &str, v: &str) -> &mut Self {
        self.tags.push(',');
        self.tags.push_str(k);
        self.tags.push('=');
        self.tags.push_str(&escape_tag(v));
        self
    }

    /// Integer field; `None` is skipped entirely (sparse series are fine).
    pub fn ifield(&mut self, k: &str, v: Option<i64>) -> &mut Self {
        if let Some(v) = v {
            if !self.fields.is_empty() {
                self.fields.push(',');
            }
            self.fields.push_str(k);
            self.fields.push('=');
            self.fields.push_str(&v.to_string());
            self.fields.push('i'); // 1.8 integer literal
        }
        self
    }

    /// `None` when the point has no fields — Influx rejects those.
    pub fn finish(&self) -> Option<String> {
        if self.fields.is_empty() {
            return None;
        }
        Some(format!(
            "{}{} {} {}",
            self.measurement, self.tags, self.fields, self.ts
        ))
    }
}

fn escape_tag(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace('=', "\\=")
        .replace(' ', "\\ ")
}
```

### `src/influx.rs`

```rust
use crate::AppState;
use std::sync::Arc;

pub async fn write(st: &Arc<AppState>, body: &str) -> anyhow::Result<()> {
    let url = format!("{}/write", st.cfg.influx_url);
    let resp = st
        .http
        .post(&url)
        .query(&[("db", st.cfg.influx_db.as_str()), ("precision", "s")])
        .body(body.to_string())
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("influx write failed: {status} {text}");
    }
    Ok(())
}

/// Newest minute already stored for this device, as a unix second.
pub async fn last_minute(st: &Arc<AppState>, device: &str) -> anyhow::Result<Option<i64>> {
    let q = format!(
        "SELECT last(\"vmc\") FROM \"pebble_minute\" WHERE \"device\" = '{}'",
        device.replace('\'', "")
    );
    let url = format!("{}/query", st.cfg.influx_url);
    let v: serde_json::Value = st
        .http
        .get(&url)
        .query(&[
            ("db", st.cfg.influx_db.as_str()),
            ("q", q.as_str()),
            ("epoch", "s"),
        ])
        .send()
        .await?
        .json()
        .await?;

    let ts = v["results"][0]["series"][0]["values"][0][0].as_i64();
    Ok(ts)
}
```

Query on `vmc` rather than `steps`: `vmc` is present in essentially every valid minute
record, whereas a `steps` field can legitimately be absent.

### Test it before touching the watch

```bash
INGEST_TOKEN=dev-token cargo run &

curl -s localhost:8088/v1/pebble/minutes \
  -H 'Authorization: Bearer dev-token' \
  -H 'Content-Type: application/json' \
  -d '{"device":"pt2-peter","minutes":[
        {"t":1757846400,"steps":12,"hr":68,"vmc":340,"orientation":97,"light":2}]}'
# → {"accepted":1,"highwater":1757846400}

curl -s 'localhost:8088/v1/pebble/state?device=pt2-peter' \
  -H 'Authorization: Bearer dev-token'
```

Post the same batch twice and confirm `SELECT count(*) FROM pebble_minute` doesn't move.
That's the idempotency property; verify it now, not in month three.

---

## 6. Phase 3–5 — The Alloy watchapp

### Project setup

```bash
pebble new-project --alloy pebble-influx
cd pebble-influx
pebble package install @moddable/pebbleproxy
```

`package.json` needs the health capability and Emery (Pebble Time 2) as a target:

```json
{
  "pebble": {
    "targetPlatforms": ["emery"],
    "capabilities": ["health"]
  }
}
```

### `src/pkjs/index.js` — phone side, just the proxy

```js
const moddableProxy = require("@moddable/pebbleproxy");
Pebble.addEventListener("ready", moddableProxy.readyReceived);
Pebble.addEventListener("appmessage", moddableProxy.appMessageReceived);
```

### `src/embeddedjs/main.js` — watch side

This is an ordinary **app**, launched either by hand from the launcher or by its own
wakeup. Each launch drains everything accumulated since the highwater mark, schedules the
next wakeup, and exits. Your watchface stays whatever you want it to be.

```js
import Health from "pebble/health";
import WakeUp from "pebble/wakeup";

const INGEST  = "https://pi.example.net/v1/pebble";
const TOKEN   = "REPLACE_ME";          // move to app config before you publish anything
const DEVICE  = "pt2-peter";
const MINUTE  = 60 * 1000;
const BATCH   = 90;                    // minutes per POST
const SYNC_INTERVAL = 12 * 60 * MINUTE;     // wakeup cadence: twice a day
const MAX_BACKFILL  = 6 * 24 * 60 * MINUTE; // API holds 7 days; stay inside it
const MAX_BATCHES   = 20;              // per launch; ~30 hours of minutes
const SYNC_COOKIE   = 1;

let syncing = false;

async function highwater() {
    const cached = Number(localStorage.getItem("hw") || 0);
    if (cached) return cached;

    // Fresh install: ask the server what it already has.
    const r = await fetch(`${INGEST}/state?device=${DEVICE}`, {
        headers: { Authorization: `Bearer ${TOKEN}` }
    });
    if (r.ok) {
        const { highwater } = await r.json();
        if (highwater) return highwater * 1000;
    }
    return Date.now() - MAX_BACKFILL;
}

function readMinutes(start, end) {
    const access = Health.metric.accessible({ metric: "step count", start, end });
    if (!(access & Health.access.available)) return null;

    const records = Health.history.byMinute({ length: BATCH, start, end });
    const base = records.start;
    const out = [];

    records.forEach((rec, i) => {
        if (!rec) return;                       // gaps come back undefined
        const t = Math.floor((base + i * MINUTE) / 1000);
        const m = { t, steps: rec.steps, vmc: rec.vmc, orientation: rec.orientation };
        if (rec.heartRate) m.hr = rec.heartRate;
        if (rec.light !== undefined) m.light = rec.light;
        out.push(m);
    });

    return { minutes: out, covered: records.end };
}

function readDaily() {
    const startOfDay = new Date();
    startOfDay.setHours(0, 0, 0, 0);
    const t = Math.floor(startOfDay.getTime() / 1000);
    const q = (metric) => Health.metric.query({ metric });

    return {
        t,
        steps:           q("step count"),
        sleep_s:         q("sleep seconds"),
        sleep_restful_s: q("sleep restful seconds"),
        active_s:        q("active seconds"),
        distance_m:      q("walked distance"),
        active_kcal:     q("active calories"),
        resting_kcal:    q("resting calories")
    };
}

/** One batch. Returns true if there may be more to send. */
async function sendOnce() {
    const start = await highwater();
    // Leave the last 15 minutes alone: recent records are still settling.
    const end = Date.now() - 15 * MINUTE;
    if (end - start < MINUTE) return false;

    const page = readMinutes(start, end);
    if (!page || page.minutes.length === 0) return false;

    const body = {
        device: DEVICE,
        minutes: page.minutes,
        daily: readDaily()
    };

    const r = await fetch(`${INGEST}/minutes`, {
        method: "POST",
        headers: {
            Authorization: `Bearer ${TOKEN}`,
            "Content-Type": "application/json"
        },
        body: JSON.stringify(body)
    });

    if (!r.ok) {
        trace(`ingest rejected: ${r.status}\n`);
        return false;                     // keep the old highwater; retry next launch
    }

    const { highwater: hw } = await r.json();
    if (!hw) return false;

    localStorage.setItem("hw", String(hw * 1000 + MINUTE));
    return page.minutes.length >= BATCH;   // a full page means there is probably more
}

/** Drain as much of the backlog as one launch reasonably can. */
async function syncAll() {
    if (syncing || !watch.connected.pebblekit) return;
    syncing = true;
    try {
        for (let i = 0; i < MAX_BATCHES; i++) {
            if (!await sendOnce()) break;
            onProgress(i + 1);
        }
        localStorage.setItem("lastSync", String(Date.now()));
    } catch (e) {
        trace(`sync failed: ${e}\n`);      // offline is the normal case, not an error
    } finally {
        syncing = false;
    }
}

/** Keep the chain alive. Called on every launch, however the app was started. */
function scheduleNext() {
    const old = Number(localStorage.getItem("wakeup") || -1);
    if (old >= 0) {
        try { WakeUp.cancel(old); } catch (e) { /* already fired or gone */ }
    }
    // notifyIfMissed = false: a missed sync is not worth a notification.
    const id = WakeUp.schedule(Date.now() + SYNC_INTERVAL, SYNC_COOKIE, false);
    if (id >= 0) localStorage.setItem("wakeup", String(id));
    else trace(`wakeup schedule failed: ${id}\n`);
}

async function main() {
    scheduleNext();                        // do this first — it must survive a failed sync
    await syncAll();
    if (launchedByWakeup()) exitApp();     // silent runs get out of the way immediately
}

main();
```

### Launch context and exiting

Two pieces of glue you'll need to look up in the *Device Info & App Events* and
*Wakeups* guides rather than take from me, because I'm not certain of the exact Alloy
spelling:

- **`launchedByWakeup()`** — the wakeup guide describes reading back the wakeup details
  (including your `cookie`) when the app is started by one. Use that to distinguish a
  silent 04:00 run from you opening the app by hand.
- **`exitApp()`** — whatever Alloy exposes for terminating the app and returning to the
  watchface. In C this is simply returning from the event loop. If Alloy has no explicit
  exit, the fallback is to show a one-line "synced" screen and let the app's own
  inactivity timeout return to the watchface — slightly uglier, functionally identical.

If the app was opened by hand, *don't* exit. Show the result (see below) so you can tell
at a glance whether the pipeline is alive.

### Minimal UI

The app needs a screen, but it needs almost nothing on it. One text layer is enough:

```
  pebble-influx
  ─────────────
  last sync   04:02
  sent        1,247 min
  highwater   03:47
```

`onProgress(n)` in the code above is where you'd update a batch counter while draining.
Also worth setting the **App Glance** on each successful sync (`"synced 04:02"`), so the
launcher tells you the pipeline is healthy without opening anything.

### The one thing to verify on real hardware

The code above maps array index → timestamp as `records.start + i * 60000`. The underlying
C API (`health_service_get_minute_history`) *collapses* missing minutes rather than leaving
holes, and the Alloy wrapper documents undefined entries for invalid data — these two
behaviours are not obviously the same thing. Before you trust a month of data, take a
walk, sync, and check that the step spikes in Grafana land at the times you actually walked.

If the index mapping turns out to be wrong, the fix is to request one minute at a time
(`length: 1`) and read `records.start` per call — slower, but unambiguous. Decide this
empirically in phase 4; it's the highest-risk assumption in the whole plan.

### Backfill and timing

The minute API holds seven days; on first run the code walks back six. A 12-hour gap is
720 minutes, so roughly 8 batches of 90 — call it 30–60 seconds of BLE round trips. A full
six-day backfill is ~96 batches, which is why `MAX_BATCHES` caps a single launch at 20:
the rest gets picked up by the next wakeup, and the seven-day window gives you enough room
to catch up over two or three runs. Do the initial backfill by hand, plugged in.

If a run takes uncomfortably long, the fix is payload size, not batch count — switch
`minutes` to the columnar array-of-arrays format mentioned in section 5 and raise `BATCH`
to 150–200.

---

## 7. Phase 6 — Making the wakeup chain reliable

This is the phase that replaces "a watchface that never stops running", so it deserves
more care than its line count suggests.

### How the chain works, and how it breaks

Each launch schedules exactly one future wakeup. That's a chain, and chains break:

| Failure | Effect | Mitigation |
|---|---|---|
| Watch off, reset, or battery dead when the wakeup was due | Link lost, no further syncs ever | `scheduleNext()` runs on *every* launch, including manual ones. Open the app once after any reset |
| Sync throws (offline, Pi down, token wrong) | Nothing sent, but chain survives | `scheduleNext()` is called **before** `syncAll()`, deliberately |
| Wakeup rejected by the system | Silent death | Check the return of `WakeUp.schedule()`; anything negative is an error code |
| App uninstalled/reinstalled | Highwater lost | Server-side `/state` endpoint restores it |

The seven-day retention window is the real safety net: any single break costs you nothing
as long as you notice within six days. Which is what the Grafana alert in section 9 is for.

### Wakeup limitations to check before you pick a cadence

The Wakeup guide has a *Limitations* section — read it. The constraints that matter are
how many wakeups a single app may have scheduled at once, and the minimum spacing the
system enforces between wakeups (including across apps, which can cause a schedule to be
rejected because something else got there first). Don't design a 15-minute cadence and
discover the floor is higher; verify the accepted interval empirically, then set
`SYNC_INTERVAL`.

If multiple concurrent wakeups are permitted, scheduling two or three ahead instead of one
makes the chain much more robust to a single missed launch. Do that if the API allows it.

### Choosing when it fires

A wakeup launches the app into the foreground: the screen leaves your watchface for the
duration of the sync. Pick times you won't witness:

- **04:00** — asleep, and a 12-hour partner slot lands around 16:00. Acceptable.
- **Better:** if you charge on a predictable schedule, aim for that window. The watch is
  off your wrist, the screen doesn't matter, and BLE throughput is free.

Set `notifyIfMissed` to `false`. A missed health sync is not an event worth a buzz.

### Verifying it

1. Set `SYNC_INTERVAL` to 3 minutes temporarily. Install, leave the watch on your normal
   watchface, and confirm the app launches, syncs and disappears on its own.
2. Confirm the launcher's App Glance updates each cycle.
3. Set it back to 12 hours. Leave it overnight. In the morning you should have an
   unbroken minute series and the app should have reappeared in the launcher's glance
   with a fresh timestamp.
4. Force a break: turn the watch off for a couple of hours, turn it back on, open the app
   by hand. Confirm the backlog drains and the chain restarts.

---

## 8. Phase 7 — Grafana

Four panels get you most of the value:

```sql
-- Heart rate, raw minute resolution
SELECT mean("hr") FROM "pebble_minute"
WHERE $timeFilter GROUP BY time(5m) fill(null)

-- Resting HR trend (the number that actually means something)
SELECT min("hr") FROM "pebble_minute"
WHERE $timeFilter GROUP BY time(1d) fill(none)

-- Steps per day
SELECT last("steps") FROM "pebble_daily"
WHERE $timeFilter GROUP BY time(1d) fill(0)

-- Sleep, total vs deep
SELECT last("sleep_s")/3600, last("sleep_restful_s")/3600 FROM "pebble_daily"
WHERE $timeFilter GROUP BY time(1d) fill(0)
```

Set **Connect null values → never** on the HR panel. The gaps are real information:
they show you when the sensor wasn't sampling, which is most of the time at the default
10-minute background rate.

---

## 9. Phase 8 — Hardening

**Deploy.** Same shape as the rest of rasplogger — multi-arch image, push, run:

```dockerfile
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/pebble-ingest /usr/local/bin/
ENV INFLUX_URL=http://influxdb:8086 INFLUX_DB=pebble
EXPOSE 8088
CMD ["pebble-ingest"]
```

```bash
docker buildx build -t peterpeerdeman/pebble-ingest:1.0.0-arm64 \
  --platform linux/arm64/v8 --push .
```

**Reachability.** Tailscale on the phone plus a tailnet hostname is the least-effort
answer and keeps the endpoint off the public internet. Failing that, Caddy in front with
automatic TLS — the Moddable proxy does real HTTPS, so a self-signed cert will not work.

**Token.** Hardcoding it in `main.js` is fine for a personal build. If you ever publish
the app, move it into the app config page (`Pebble.addEventListener("showConfiguration")`)
and pass it through to the watch via AppMessage.

**Battery.** Don't touch `Health.heartRate.samplePeriod` — it's a system-wide request,
and an app that leaves it at 1 second will flatten the battery even after it exits. Set
the background sampling rate in the Pebble app settings instead (10 / 30 / 60 minutes).
If you want denser HR data, that dial is the honest place to pay for it. Two BLE sync
bursts a day are negligible by comparison — and this is the quiet advantage of the
wakeup model over a resident watchface, which would have kept the radio and the JS
runtime busy around the clock.

**Watchdog.** One `curl` on `/healthz` from your existing monitoring, plus a Grafana alert
on **"no `pebble_minute` points in the last 30 hours"** — sized to the 12-hour wakeup
cadence with slack for one missed link, and well inside the seven-day retention window
where a break is still recoverable. That alert is what makes the chain safe to rely on.

---

## 10. Scope estimate

| | |
|---|---|
| Rust ingest service | ~350 lines, one evening |
| Alloy app (sync + drain loop) | ~150 lines, one evening |
| Alloy app (wakeup chain + status screen) | ~40 lines, plus a night of soak testing |
| Grafana dashboard | an hour |
| Verifying the minute-index mapping | do not skip; budget a day of wearing it |

No watchface to write, and no obligation to wear one you built. That was the point.

Three risks, in order:

1. **The index→timestamp assumption** in the minute reader (section 6). Highest impact,
   cheapest to test — take a walk and look at where the spikes land.
2. **Wakeup limitations** you haven't read yet (section 7). Could force a different
   cadence than 12 hours.
3. **Reachability** of the Pi from your phone's network.

None of them are in the Rust. Test all three in the first week.
