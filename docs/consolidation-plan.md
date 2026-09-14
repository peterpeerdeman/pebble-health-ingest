# Consolidating Fitbit history with the Pebble pipeline

How to make six years of Fitbit data and the new Pebble pipeline read as one
continuous health history in InfluxDB + Grafana.

---

## 0. What we're actually reconciling

Both live in the same InfluxDB 1.8 instance (host `192.168.117.5:8086`), in two
databases: `fitbit` (the archive) and `pebble` (the live pipeline this repo
feeds). They were inspected directly; this plan is built on the real data, not
the code's intent.

### The single most important fact

**The two sources barely overlap.** Fitbit's last day with real steps is
**2026-07-31**; the Pebble watch started writing in **September 2026**. This is
a handoff, not a merge of concurrent measurements. There is no reconciliation of
disagreeing values to do — only a schema seam to make continuous. That makes the
whole job much smaller than it first looks.

### Fitbit is the larger, richer, frozen archive

| | |
|---|---|
| Span | 2020-02 → 2026-07 (≈6.4 years) |
| Status | **frozen** — no new writes, ever |
| Daily activity points | ~1,980 |
| Sleep summaries (with stages) | ~2,266 |
| Sleep sessions | ~2,806 |
| Intraday heart rate (1-min) | large (a count query times out; ≥ 10^5 points) |
| Retention | `autogen`, infinite |

### Pebble is the small, live, adaptable source

| | |
|---|---|
| Span | 2026-09 → ongoing |
| Status | **live** — the watch writes twice a day |
| Volume so far | ~1,200 minute points |
| Retention | `raw`, 104w **default** — see §4, this will silently delete history |

---

## 1. The three real incompatibilities

Everything else is naming. These three are the actual work, and they exist in
*either* direction.

### 1a. Units

| Quantity | Fitbit | Pebble | Factor |
|---|---|---|---|
| Distance | km (`distance_total` 4.71) | metres (`distance_m` 3253) | ×1000 |
| Sleep | minutes (`totalMinutesAsleep` 461) | seconds (`sleep_s` 24960) | ×60 |
| Calories | total `caloriesOut` (float) | split `active_kcal` + `resting_kcal` (int) | sum |

### 1b. Field types — a hard InfluxDB constraint

Every Fitbit field is a **float**; every Pebble field is an **integer**.
InfluxDB 1.x binds a field's type per shard and **rejects a write that changes
it** (`field type conflict`). So you cannot pour Pebble's `steps=3268i` into the
Fitbit `activities` measurement, which already holds `steps` as a float — that
write is refused. Whichever measurement a series lands in dictates its type, and
the writer must match it.

### 1c. Daily timestamp convention

- Fitbit daily points sit at **local midnight** — 79200 s past UTC midnight =
  22:00 UTC in summer (CEST), 23:00 UTC in winter (CET).
- Pebble daily points are **floored to 00:00 UTC** by this service
  (`t - t.rem_euclid(86_400)` in `render_batch`), regardless of the watch's
  local midnight.

So the same calendar day is stamped 1–2 hours apart by the two sources, and they
fall in different UTC day-buckets under a naïve `GROUP BY time(1d)`. This must be
normalised or the daily charts will show a one-day stagger at the seam.

### 1d. What simply doesn't map (and that's fine)

Neither source is a superset, so consolidation is lossy at the edges:

- **Fitbit-only:** sleep stages (deep/light/rem/wake), HR zones (min/max/minutes
  per zone), distance by activity type, calorie breakdown, floors, elevation,
  three tiers of active minutes. The watch cannot produce any of these.
- **Pebble-only:** per-minute `steps`, `vmc` (motion), `orientation`, `light`.
  Fitbit only ever stored per-minute *heart rate*.

The plan keeps both sets; it does not try to invent the missing halves.

---

## 2. Field-by-field crosswalk

The continuity series that a health dashboard actually plots, and how they line
up. `activities`/`sleepsummaries`/`heartrate` are Fitbit measurements;
`pebble_daily`/`pebble_minute` are ours.

| Concept | Fitbit | Pebble | Note |
|---|---|---|---|
| Daily steps | `activities.steps` (float) | `pebble_daily.steps` (int) | type + tz |
| Resting HR | `activities.restingHeartRate` | `pebble_daily.hr_resting` | sparse in both |
| Distance | `activities.distance_total` km | `pebble_daily.distance_m` m | ×1000 |
| Calories out | `activities.caloriesOut` | `active_kcal + resting_kcal` | sum |
| Active minutes | `veryActiveMinutes` etc. | `pebble_daily.active_s` /60 | tiers vs one number |
| Sleep total | `sleepsummaries.totalMinutesAsleep` | `pebble_daily.sleep_s` /60 | ×60 |
| Deep sleep | `sleepsummaries."stages.deep"` | `pebble_daily.sleep_restful_s` /60 | restful ≈ deep, not equal |
| Intraday HR | `heartrate.value` (1-min) | `pebble_minute.hr` (1-min) | same shape |
| Per-minute motion | — | `pebble_minute.{steps,vmc,orientation,light}` | Pebble-only |

---

## 3. The decision: which way to converge

### Option A — Pebble writes into the Fitbit schema (recommended)

Make this ingest service *also* write the continuity series into the existing
Fitbit measurements, converted to Fitbit units/types/timestamps. Keep Pebble's
minute-level extras in `pebble_minute`. Never touch the archive.

- **Pro:** zero rewrite of six irreplaceable years; every existing Grafana panel
  built on `activities`/`sleep`/`heartrate` keeps working and simply extends into
  the present. All change is in the small, tested, version-controlled service.
- **Pro:** Fitbit's schema is the superset for daily/sleep; Pebble maps into it
  cleanly. The reverse loses stages, zones, floors.
- **Con:** you inherit a legacy shape (float, km, minutes, local-midnight, no
  `device` tag). Pebble concepts map imperfectly (restful-sleep → deep-sleep is
  an approximation; you'd leave stages/zones null going forward).

### Option B — Migrate Fitbit into the Pebble schema

Rewrite the archive into `pebble_*` (int, metres, seconds), extending
`pebble_daily` with new fields for the Fitbit-only riches.

- **Pro:** one modern, minute-first, integer schema owned entirely by this repo;
  no legacy conventions.
- **Con:** you rewrite six years of irreplaceable data (a risky one-shot),
  **break every existing Fitbit dashboard**, and still have to widen
  `pebble_daily` with ~10 stage/zone/tier fields it was never designed for. More
  work, more risk, for a cleaner-schema benefit you mostly don't feel because the
  archive is frozen.

### Recommendation: **Option A.**

The archive is frozen, larger, richer, and already has dashboards. The live
source is small and adaptable and *this repo owns it*. Converge the moving part
onto the stationary one. Reserve Option B for the day you decide the integer
minute-first schema is worth a migration project on its own — it isn't forced by
consolidation.

The rest of this plan implements Option A.

---

## 4. Implementation (Option A)

### Step 0 — Fix the retention trap first (independent, do it now)

The `pebble` database's **default** retention is `raw` at 104 weeks, but daily
points written into the Fitbit `activities` measurement will live in the
`fitbit` database under `autogen`/infinite — good. However any series still
written to `pebble` inherits 104w and will **silently drop** after two years.
Decide retention explicitly:

```sql
-- keep Pebble's own minute data as long as you want it (or make it infinite)
ALTER RETENTION POLICY "raw" ON "pebble" DURATION 0s   -- infinite
```

Set `INFLUX_RETENTION` in this service accordingly (or `none`).

### Step 1 — Add a "legacy mirror" writer to the ingest service

In `render_batch` (see `src/main.rs`), when a `daily` block is present, emit a
*second* point into the Fitbit `activities` measurement alongside the native
`pebble_daily`. Requirements, driven by §1:

- **Floats, not ints** (§1b). Add a float field helper to `src/line.rs`
  (`ffield`) beside the existing `ifield`; the Fitbit fields are all floats.
- **Convert units** (§1a): `distance_total = distance_m / 1000.0`,
  `caloriesOut = active_kcal + resting_kcal`,
  and write sleep via `sleepsummaries.totalMinutesAsleep = sleep_s / 60.0`,
  `"stages.deep" = sleep_restful_s / 60.0` (documented approximation).
- **Local-midnight timestamp** (§1c): stop flooring daily to UTC midnight. Take
  the day boundary in a configured zone. Simplest: add `TZ_OFFSET_SECONDS` (e.g.
  3600 CET / 7200 CEST) or a `chrono-tz` zone, and floor to local midnight so the
  point lands where Fitbit's did. (A fixed offset misses DST; `chrono-tz` with
  `Europe/Amsterdam` is correct — small dependency, worth it.)
- **Keep a `device` tag** on the mirrored point so watch-sourced days are
  distinguishable from Fitbit-sourced days in the same measurement.
- Map intraday HR: for each `pebble_minute` with `hr > 0`, also write
  `heartrate.value = hr` (float) at the same minute, so the HR chart is one
  continuous series across the seam.

Gate the whole mirror behind a config flag (`LEGACY_MIRROR=1`,
`LEGACY_DB=fitbit`) so it is opt-in and testable. The native `pebble_*` writes
stay exactly as they are — nothing is lost, and you can migrate dashboards at
your own pace.

### Step 2 — Backfill the seam (one-off)

For the ~six-week gap where the watch already wrote `pebble_daily` before this
change (Sept → now), run a one-shot: read `pebble_daily`/`pebble_minute` from the
`pebble` DB, apply the same conversion, and write the `activities`/`heartrate`
points into `fitbit`. A ~30-line script (Rust bin or Python against the two HTTP
APIs). Idempotent, because points are overwritten by (measurement, tags, time).

### Step 3 — Point Grafana at the unified series

Existing Fitbit panels already query `activities`/`sleep`/`heartrate`; after
Step 1 they extend into the present automatically. For any panel that used
`pebble_*`, switch it to the Fitbit measurement. Use tz-aware daily grouping so
the local-midnight points bucket correctly:

```sql
SELECT last("steps") FROM "activities"
WHERE $timeFilter GROUP BY time(1d, '-2h') fill(null)   -- or tz('Europe/Amsterdam')
```

---

## 5. Verification

1. **Seam continuity:** plot daily steps and resting HR across July→October 2026.
   No gap, no one-day stagger (confirms §1c is handled).
2. **Units:** a known Pebble day's `distance_total` reads in km, `caloriesOut`
   is active+resting, sleep reads in minutes — same magnitude as neighbouring
   Fitbit days.
3. **Types:** the mirrored write succeeds (no `field type conflict` in the
   service logs) — proves floats matched the existing `activities` schema.
4. **Idempotency:** re-run the Step-2 backfill; `SELECT count(*)` on `activities`
   is unchanged.
5. **HR:** one `heartrate.value` line spans Fitbit and Pebble eras continuously.
6. **Archive untouched:** `fitbit` row counts for pre-2026-08 dates are identical
   before and after (Option A never rewrites history).

---

## 6. Scope estimate

| | |
|---|---|
| §0 retention fix | 5 minutes |
| §1 `ffield` + unit/tz conversion in ingest | half a day; add `chrono-tz` |
| §1 intraday HR mirror | an hour |
| §2 seam backfill script | an hour, one-off |
| §3 Grafana re-point + tz grouping | an hour |
| Verification | an hour |

No rewrite of the historical archive, no dashboard rebuild, and the change is
contained to the service this repo already owns and tests.
