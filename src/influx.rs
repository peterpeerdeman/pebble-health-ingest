//! Thin InfluxDB 1.8 HTTP client: `/write` and `/query` only.

use std::sync::Arc;

use crate::resting_hr::MinuteSample;
use crate::AppState;

fn base_params(st: &AppState, db: &str) -> Vec<(&'static str, String)> {
    let mut q = vec![("db", db.to_string())];
    if let Some(u) = &st.cfg.influx_user {
        q.push(("u", u.clone()));
        q.push(("p", st.cfg.influx_password.clone().unwrap_or_default()));
    }
    q
}

/// Write a body of line-protocol points to `db` with second precision.
pub async fn write(st: &Arc<AppState>, db: &str, body: &str) -> anyhow::Result<()> {
    let url = format!("{}/write", st.cfg.influx_url);
    let mut params = base_params(st, db);
    params.push(("precision", "s".into()));

    let resp = st
        .http
        .post(&url)
        .query(&params)
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
///
/// Queries `vmc` rather than `steps`: `vmc` is present in essentially every
/// valid minute record, whereas `steps` can legitimately be absent.
pub async fn last_minute(st: &Arc<AppState>, device: &str) -> anyhow::Result<Option<i64>> {
    // `device` has already passed `sanitize_device` (alphanumerics, '-', '_'),
    // so it cannot break out of the quoted literal.
    let q = format!("SELECT last(\"vmc\") FROM \"pebble_minute\" WHERE \"device\" = '{device}'");
    let v = query(st, &st.cfg.influx_db, &q).await?;
    Ok(v["results"][0]["series"][0]["values"][0][0].as_i64())
}

/// `hr`, `vmc` and `steps` for every `pebble_minute` point at or after
/// `since` (unix seconds), for the resting heart rate estimate in
/// `resting_hr::compute`. Missing fields on a given minute come back as
/// `None` from Influx and are kept as such, not coerced to zero.
pub async fn recent_minutes(
    st: &Arc<AppState>,
    device: &str,
    since: i64,
) -> anyhow::Result<Vec<MinuteSample>> {
    let q = format!(
        "SELECT hr, vmc, steps FROM \"pebble_minute\" WHERE \"device\" = '{device}' AND time >= {since}s"
    );
    let v = query(st, &st.cfg.influx_db, &q).await?;

    let empty = Vec::new();
    let rows = v["results"][0]["series"][0]["values"]
        .as_array()
        .unwrap_or(&empty);

    let as_int = |v: &serde_json::Value| -> Option<i64> {
        v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
    };

    Ok(rows
        .iter()
        .map(|row| MinuteSample {
            hr: row.get(1).and_then(as_int),
            vmc: row.get(2).and_then(as_int),
            steps: row.get(3).and_then(as_int),
        })
        .collect())
}

/// Best-effort, idempotent schema setup at startup. Never fatal: the service
/// still starts if Influx is down, and the first write will surface the error.
///
/// Creates the native `pebble` database (with the configured retention) and,
/// when the health mirror is enabled, the unified `health` database. The latter
/// gets Influx's default `autogen` (infinite) retention so it can hold the
/// copied Fitbit archive; we deliberately do not put a bounded policy on it.
pub async fn ensure_database(st: &Arc<AppState>) {
    let mut stmts = vec![format!("CREATE DATABASE \"{}\"", st.cfg.influx_db)];
    if let Some(rp) = &st.cfg.influx_retention {
        stmts.push(format!(
            "CREATE RETENTION POLICY \"raw\" ON \"{}\" DURATION {rp} REPLICATION 1 DEFAULT",
            st.cfg.influx_db
        ));
    }
    if let Some(health) = &st.cfg.health_db {
        stmts.push(format!("CREATE DATABASE \"{health}\""));
    }

    for attempt in 1..=10u32 {
        let mut ok = true;
        for stmt in &stmts {
            match exec(st, stmt).await {
                Ok(()) => {}
                Err(e) if e.to_string().contains("already exists") => {}
                Err(e) => {
                    ok = false;
                    tracing::warn!(attempt, error = %e, stmt, "influx schema setup failed");
                    break;
                }
            }
        }
        if ok {
            tracing::info!(
                db = %st.cfg.influx_db,
                health = ?st.cfg.health_db,
                "influx schema ready"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    tracing::warn!("giving up on influx schema setup; will rely on writes surfacing errors");
}

async fn exec(st: &Arc<AppState>, stmt: &str) -> anyhow::Result<()> {
    let url = format!("{}/query", st.cfg.influx_url);
    let mut params = base_params(st, &st.cfg.influx_db);
    params.push(("q", stmt.to_string()));
    let resp = st.http.post(&url).query(&params).send().await?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        anyhow::bail!("influx query failed: {status} {v}");
    }
    if let Some(err) = v["results"][0]["error"]
        .as_str()
        .or_else(|| v["error"].as_str())
    {
        anyhow::bail!("{err}");
    }
    Ok(())
}

async fn query(st: &Arc<AppState>, db: &str, q: &str) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}/query", st.cfg.influx_url);
    let mut params = base_params(st, db);
    params.push(("q", q.to_string()));
    params.push(("epoch", "s".into()));
    let resp = st.http.get(&url).query(&params).send().await?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        anyhow::bail!("influx query failed: {status} {v}");
    }
    if let Some(err) = v["results"][0]["error"]
        .as_str()
        .or_else(|| v["error"].as_str())
    {
        anyhow::bail!("influx query error: {err}");
    }
    Ok(v)
}
