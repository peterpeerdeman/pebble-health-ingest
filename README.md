# pebble-health-ingest

Rust (axum) service that receives Pebble Health batches from the
[pebble-health-watchapp](https://github.com/peterpeerdeman/pebble-health-watchapp)
and writes them to InfluxDB 1.8 as line protocol, for Grafana.

```
Pebble Time 2 (Alloy app) ──BLE──▶ phone (PKJS proxy) ──HTTPS POST──▶ pebble-ingest ──▶ InfluxDB 1.8 ──▶ Grafana
```

The watchapp is deliberately dumb; every decision about schema, tags,
retention and dedup lives here, where it can change without reflashing a
watch. Full design notes: [docs/plan.md](docs/plan.md).

## Endpoints

All except `/healthz` require `Authorization: Bearer $INGEST_TOKEN`.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/healthz` | liveness (`ok`) |
| `POST` | `/v1/pebble/minutes` | ingest a batch: minutes, optional daily rollup, optional activities |
| `GET` | `/v1/pebble/state?device=NAME` | newest minute stored for that device (unix seconds) |
| `POST` | `/v1/pebble/state` `{"device":"NAME"}` | same, for the watch (its runtime can only POST reliably) |

Batch format. Minutes are either objects or compact rows
`[t, steps, vmc, orientation, light, hr]` (the watch sends rows; its JS heap
is a few KB). `hr` of 0 means no sample and is omitted.

```json
{
  "device": "pt2-peter",
  "minutes": [
    { "t": 1757846400, "steps": 12, "hr": 68, "vmc": 340, "orientation": 97, "light": 2 },
    [ 1757846460, 0, 12, 97, 2, 0 ]
  ],
  "daily": { "t": 1757808000, "steps": 8231, "sleep_s": 27000, "sleep_restful_s": 9100,
             "active_s": 3300, "distance_m": 6120, "active_kcal": 410, "resting_kcal": 1680, "hr_resting": 52 },
  "activities": [ { "type": "walk", "start": 1757840000, "end": 1757842400 } ]
}
```

Reply: `{"accepted": 2, "rejected": 0, "highwater": 1757846460}`.

Rules applied on the way in:

- timestamps are snapped to the minute (daily: to the day), so replays overwrite
  instead of duplicating: **idempotent**, the property the whole design rests on;
- minutes older than 8 days or more than 2 minutes in the future are rejected (counted, not fatal);
- device and activity type are tag values, restricted to `[A-Za-z0-9_-]`;
- request paths like `//v1/...` are normalised: `@moddable/pebbleproxy` 0.1.8 produces them.

## Data model (InfluxDB)

| Measurement | Tags | Fields | Timestamp |
|---|---|---|---|
| `pebble_minute` | `device`, `source=alloy` | `steps`, `hr`, `vmc`, `orientation`, `light` (all int, sparse) | minute |
| `pebble_daily` | `device` | `steps`, `sleep_s`, `sleep_restful_s`, `active_s`, `distance_m`, `active_kcal`, `resting_kcal`, `hr_resting` | start of day |
| `pebble_activity` | `device`, `type` | `duration_s` | session start |

On startup the service creates the database and a default retention policy
`raw` (`INFLUX_RETENTION`, default `104w`) if they do not exist.

### Unified `health` mirror (Fitbit-schema consolidation)

To join the watch's data onto a pre-existing Fitbit history, the service also
mirrors the continuity series into a second database, `health`, in the **Fitbit
archive's schema** — float fields, kilometres, minutes, and daily points stamped
at local midnight (`HEALTH_TZ`). This is created on startup with Influx's default
(infinite) retention so it can also hold the copied Fitbit archive.

| `health` measurement | From `pebble_daily` / `pebble_minute` | Conversion |
|---|---|---|
| `activities` | steps, distance, calories, resting HR, active minutes | m→km, active+resting kcal, ints→floats, day→local midnight |
| `sleepsummaries` | `totalMinutesAsleep`, `stages.deep` | s→min; restful≈deep (approximation) |
| `heartrate` | `value` per minute with a sample | ints→floats |

The mirror never fails an ingest: if the `health` write errors, it is logged and
the native `pebble_*` write still stands. Set `HEALTH_DB=none` to turn it off.
Rationale and the full crosswalk are in
[docs/consolidation-plan.md](docs/consolidation-plan.md). Both are
idempotent and failures are logged, not fatal.

## Configuration

| Variable | Default | |
|---|---|---|
| `INGEST_TOKEN` | required | shared secret with the watchapp (`openssl rand -hex 24`) |
| `INFLUX_URL` | `http://localhost:8086` | plain HTTP; the binary has no TLS stack |
| `INFLUX_DB` | `pebble` | |
| `INFLUX_USER` / `INFLUX_PASSWORD` | unset | Influx basic auth |
| `INFLUX_RETENTION` | `104w` | default retention policy on `pebble`; `none` to skip |
| `HEALTH_DB` | `health` | unified Fitbit-schema database to mirror into; `none` to disable |
| `HEALTH_TZ` | `Europe/Amsterdam` | timezone for daily local-midnight timestamps in `health` |
| `LISTEN_ADDR` | `0.0.0.0:8088` | |
| `RUST_LOG` | `info` | |

## Run it

```sh
cp .env.example .env && $EDITOR .env      # INGEST_TOKEN
docker compose up -d                       # influxdb 1.8 + grafana + pebble-ingest
open http://localhost:3000                 # admin / $GRAFANA_ADMIN_PASSWORD, dashboard "Pebble Health"
```

Smoke test, then post the same batch twice and confirm the count doesn't move:

```sh
curl -s localhost:8088/v1/pebble/minutes \
  -H 'Authorization: Bearer dev-token' -H 'Content-Type: application/json' \
  -d '{"device":"pt2-peter","minutes":[[1757846400,12,340,97,2,68]]}'
curl -s 'localhost:8088/v1/pebble/state?device=pt2-peter' -H 'Authorization: Bearer dev-token'
docker exec influxdb influx -database pebble -execute 'SELECT count(*) FROM pebble_minute'
```

Note that the batch timestamps above are only accepted when within 8 days of now.

### Adding it to an existing stack

If InfluxDB and Grafana already run (e.g. `raspcomposer`), add only this service
to that compose file; it reaches Influx by container name on the same network:

```yaml
  pebble-ingest:
    image: ghcr.io/peterpeerdeman/pebble-ingest:1.0.0
    container_name: pebble-ingest
    restart: always
    ports: ["8088:8088"]
    environment:
      INGEST_TOKEN: ${INGEST_TOKEN}
      INFLUX_URL: http://influxdb:8086
      INFLUX_DB: pebble
    depends_on: [influxdb]
```

Then provision `grafana/provisioning/` (datasource + dashboard) or import
`grafana/provisioning/dashboards/pebble-health.json` by hand.

### Reachability

The request leaves from the phone's network. Put the service behind Tailscale
(cleanest) or a Caddy reverse proxy with a real certificate; the phone proxy
does real HTTPS, so self-signed will not work.

## Image

Published by CI as `ghcr.io/peterpeerdeman/pebble-ingest` (`latest`, `1.0.0`,
`sha-…`), a multi-arch manifest for **linux/386, linux/amd64 and linux/arm64**,
so the same tag deploys on a 32-bit x86 box, a 64-bit x86 box and a Raspberry Pi.
To also publish under `peterpeerdeman/pebble-ingest` on Docker Hub, add the
`DOCKERHUB_USERNAME`/`DOCKERHUB_TOKEN` repo secrets, or push by hand:

```sh
docker buildx build --platform linux/386,linux/amd64,linux/arm64 \
  -t peterpeerdeman/pebble-ingest:1.0.0 -t peterpeerdeman/pebble-ingest:latest --push .
```

Multi-arch Dockerfile; the build stage cross-compiles on the build host, so no
emulated rustc:

```sh
docker buildx build --platform linux/386 -t ghcr.io/peterpeerdeman/pebble-ingest:1.0.0 --load .
```

Runtime image is `debian:bookworm-slim` + one static-ish binary (~3 MB), runs
as `nobody`, has a `HEALTHCHECK` (`pebble-ingest healthcheck`, no curl needed).
CI (`.github/workflows/ci.yml`) runs fmt, clippy and tests, then builds and
pushes `linux/386,amd64,arm64` to GHCR (and to Docker Hub when
`DOCKERHUB_USERNAME`/`DOCKERHUB_TOKEN` secrets are set).

To ship an image to a host without a registry:

```sh
docker save ghcr.io/peterpeerdeman/pebble-ingest:1.0.0 | gzip > pebble-ingest-1.0.0-i386.tar.gz
# on the host:
gunzip -c pebble-ingest-1.0.0-i386.tar.gz | docker load
```

## Development

```sh
cargo test                                   # unit + handler tests against a mock Influx
cargo clippy --all-targets -- -D warnings
INGEST_TOKEN=dev-token INFLUX_URL=http://localhost:8086 cargo run
```

## Monitoring

`GET /healthz` from existing monitoring, plus the dashboard's "Minutes received
in last 30h" stat (alert when 0): sized to the 12-hour wakeup cadence with slack
for one missed link, and well inside the watch's 7-day retention window.
