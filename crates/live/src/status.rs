//! `state/status.json`: the `susq.v1.StatusSnapshot` the status pusher sends to the dashboard.
//! Aggregates only (Competition Data stays on the server either way).

use std::{io::Write, path::Path};

use chrono::{DateTime, Utc};
use susq_contracts::v1::{Header, PositionSummary, Producer, StatusSnapshot};
use susq_core::{Calendar, clock::from_ns};

use crate::engine::Engine;

/// Positions listed in the snapshot (largest first).
const MAX_POSITIONS: usize = 25;

/// Contest phase shown on the dashboard.
pub fn stage(now: DateTime<Utc>, calendar: &Calendar) -> &'static str {
    let stalk: DateTime<Utc> = DateTime::from_naive_utc_and_offset(
        chrono::NaiveDate::from_ymd_opt(2026, 10, 21)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .unwrap_or_default(),
        Utc,
    );
    // Election night starts Nov 3, 19:00 ET = 00:00Z Nov 4.
    let emerge = calendar.lock_utc - chrono::Duration::hours(17);
    if now >= calendar.last_order_utc {
        "locked"
    } else if now >= emerge {
        "emerge"
    } else if now >= stalk {
        "stalk"
    } else {
        "forage"
    }
}

/// The build/deploy commit, from `SUSQ_GIT_SHA` (set by the deploy scripts).
pub fn git_sha() -> String {
    std::env::var("SUSQ_GIT_SHA").unwrap_or_else(|_| "unknown".into())
}

/// Build the snapshot from the engine's world.
pub fn snapshot(engine: &Engine, now_ns: i64, seq: u64) -> StatusSnapshot {
    let w = &engine.world;
    let cfg = engine.cfg();
    let gw = engine.gateway();
    let kill_reason = gw.kill().reason();
    let trader = gw
        .lease()
        .on_disk()
        .ok()
        .flatten()
        .filter(|l| l.expires_at_ns > now_ns)
        .map(|l| l.holder);
    let marks = w.marks();
    let mut positions: Vec<PositionSummary> = w
        .positions
        .map()
        .iter()
        .map(|(ex, q)| {
            let mark = marks.get(ex).copied().unwrap_or(0.0);
            let title = w.markets.rule(ex).map(|r| r.title.clone()).unwrap_or_default();
            PositionSummary {
                exchange_id: ex.to_string(),
                title,
                quantity: *q,
                avg_cost: 0.0,
                mark,
                unrealized: 0.0,
            }
        })
        .collect();
    positions.sort_by(|a, b| {
        b.quantity
            .abs()
            .cmp(&a.quantity.abs())
            .then_with(|| a.exchange_id.cmp(&b.exchange_id))
    });
    positions.truncate(MAX_POSITIONS);

    StatusSnapshot {
        header: Some(Header {
            schema_version: susq_contracts::SCHEMA_VERSION,
            produced_at_ns: now_ns,
            producer: Some(Producer {
                name: "rust-live".into(),
                git_sha: git_sha(),
            }),
            seq,
        }),
        mode: gw.mode().as_str().into(),
        stage: stage(from_ns(now_ns), &cfg.calendar).into(),
        trader: trader.unwrap_or_default(),
        equity: w.equity,
        cash: w.cash,
        pnl_day: 0.0,
        pnl_total: if w.initial_equity > 0.0 {
            w.equity - w.initial_equity
        } else {
            0.0
        },
        rank: w.rank.and_then(|r| i32::try_from(r).ok()).unwrap_or(0),
        field_size: w.field_size,
        killed: kill_reason.is_some(),
        kill_reason: kill_reason.unwrap_or_default(),
        health: w.health.values().cloned().collect(),
        positions,
        recent_trades: w.trades.iter().rev().take(20).cloned().collect(),
        action_needed: w.action_needed.iter().cloned().collect(),
        events: w.events.iter().rev().take(20).cloned().collect(),
    }
}

/// Atomically write the snapshot as proto3 JSON (tmp + rename).
pub fn write(path: &Path, snap: &StatusSnapshot) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = susq_contracts::to_json(snap).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal() -> Calendar {
        Calendar {
            open_utc: "2026-10-01T16:00:00Z".parse().expect("ts"),
            lock_utc: "2026-11-04T17:00:00Z".parse().expect("ts"),
            last_order_utc: "2026-11-04T16:55:00Z".parse().expect("ts"),
        }
    }

    #[test]
    fn stages_follow_the_calendar() {
        let at = |s: &str| stage(s.parse().expect("ts"), &cal());
        assert_eq!(at("2026-10-05T00:00:00Z"), "forage");
        assert_eq!(at("2026-10-25T00:00:00Z"), "stalk");
        assert_eq!(at("2026-11-04T01:00:00Z"), "emerge");
        assert_eq!(at("2026-11-04T16:55:00Z"), "locked");
    }
}
