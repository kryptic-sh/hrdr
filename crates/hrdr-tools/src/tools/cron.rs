//! `cron` — recurring reminders that nudge the model at a set schedule.
//!
//! The model creates a cron with a 5-field cron expression plus the reminder
//! content; a per-cron scheduler task sleeps until each next fire and delivers
//! the reminder into the conversation as a finished [`BackgroundTask`] — the
//! same spawn-return-deliver contract `watch` uses, so a fire mid-turn is
//! drained at the next request and a fire while idle wakes the model. The cron
//! persists with the session and its scheduler is re-armed on resume.
//!
//! The nudge message each fire delivers ends by telling the model it can
//! cancel the cron if the goal behind it is already achieved — the model's
//! own escape hatch, so a reminder whose purpose is done stops firing.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use chrono::{Datelike, Local, Timelike};
use serde_json::json;

use crate::{BackgroundKind, BackgroundTask, CronItem, Tool, ToolContext};

/// How far ahead `next_fire` scans before declaring a schedule unreachable. A
/// cron like `0 0 29 2 *` (Feb 29) legitimately needs four years; anything
/// rarer than that is a mistake worth refusing at create time rather than a
/// scheduler that never fires.
const NEXT_FIRE_HORIZON_DAYS: i64 = 4 * 366;

pub struct CronTool;

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &'static str {
        "cron"
    }
    fn description(&self) -> &'static str {
        "Create a recurring reminder: the harness delivers `content` to you each time the \
         schedule fires, waking you between turns. `schedule` is a 5-field cron expression \
         (`minute hour day-of-month month day-of-week`; `*`, `*/n`, `a-b`, `a,b` supported; \
         day-of-week 0-7, both 0 and 7 are Sunday). Use `cron` with `op: create` (a `schedule` \
         and `content`), `op: cancel` (the `id` of a cron that is no longer needed — its goal \
         achieved or abandoned), or `op: list`. When a reminder's goal is already met, cancel \
         the cron rather than letting it keep firing."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["create", "cancel", "list"],
                    "description": "create: set a recurring reminder with `schedule` + `content`. \
                                    cancel: stop the cron with `id`. list: show all crons."
                },
                "schedule": {
                    "type": "string",
                    "default": "",
                    "description": "5-field cron expression, e.g. `*/30 * * * *` (every 30 \
                                    minutes) or `0 9 * * 1-5` (9am weekdays)."
                },
                "content": {
                    "type": "string",
                    "default": "",
                    "description": "The reminder delivered to you at each fire, for `op: create`."
                },
                "id": {
                    "type": "integer",
                    "default": 0,
                    "description": "The cron's `#N` id, for `op: cancel`. Ids come from the \
                                    `list` output or the create result."
                }
            },
            "required": ["op"]
        })
    }
    /// `read_only` here means what it means everywhere else in the registry:
    /// *does not mutate the working tree*. `cron` mutates a `Vec<CronItem>`
    /// behind a mutex in the agent's own [`ToolContext`] and spawns a scheduler
    /// that only ever pushes a `BackgroundTask` — no file, no process, nothing
    /// outside this agent's memory.
    fn read_only(&self) -> bool {
        true
    }
    /// …but opt back out of concurrency, which `read_only` would otherwise
    /// imply. `create` and `cancel` both mutate the same shared list, so two of
    /// them in one batch are order-sensitive — sequential keeps "the last call
    /// the model made is the list it gets".
    fn concurrent(&self) -> bool {
        false
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: CronArgs = crate::tool_args("cron", args)?;
        match a.op.as_str() {
            "create" => {
                let schedule = a.schedule.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
                    anyhow!("`cron` with `op: create` needs a non-empty `schedule`")
                })?;
                let content = a.content.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
                    anyhow!("`cron` with `op: create` needs a non-empty `content`")
                })?;
                // Validate before minting: a schedule that never fires within
                // the horizon is a mistake the model should hear about now, not
                // a scheduler that silently does nothing.
                parse_schedule(&schedule)
                    .map_err(|e| anyhow!("invalid schedule `{schedule}`: {e}"))?;
                if next_fire(&schedule, Local::now()).is_none() {
                    bail!(
                        "schedule `{schedule}` never fires within the next {} days — \
                         is that what you meant?",
                        NEXT_FIRE_HORIZON_DAYS
                    );
                }
                let mut crons = ctx
                    .crons
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let id = crons.iter().map(|c| c.id).max().unwrap_or(0) + 1;
                crons.push(CronItem {
                    id,
                    schedule: schedule.trim().to_string(),
                    content: content.trim().to_string(),
                });
                // Spawn the scheduler AFTER the entry exists, so the task's
                // first lock finds itself addressable.
                arm_cron(ctx, id);
                Ok(format!(
                    "cron #{id} set: `{}` — you'll be reminded every time it fires.\n\n\
                     Reminder content: {}\n\n\
                     Cancel it with `cron cancel {id}` when its goal is achieved or no \
                     longer wanted. End your turn; the reminder arrives on schedule.",
                    crons.last().unwrap().schedule,
                    crons.last().unwrap().content
                ))
            }
            "cancel" => {
                let id =
                    a.id.ok_or_else(|| anyhow!("`cron` with `op: cancel` needs an `id`"))?;
                let mut crons = ctx
                    .crons
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(cron) = crons.iter().find(|c| c.id == id) else {
                    bail!("no cron #{id}. Ids come from `cron`'s list output or a create result.");
                };
                let schedule = cron.schedule.clone();
                crons.retain(|c| c.id != id);
                // A reminder already fired but not yet delivered must not land
                // after the cancel: mark any pending delivery for this cron.
                cancel_pending_deliveries(ctx, id);
                // The scheduler may be mid-sleep until the next fire (hours or
                // days away) — clear its armed mark now so a later recreate is
                // not blocked by a task that will only notice the cancel then.
                drop(crons);
                mark_unarmed(ctx, id);
                Ok(format!(
                    "cron #{id} cancelled (`{schedule}`). It will not fire again."
                ))
            }
            "list" => {
                let crons = ctx
                    .crons
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if crons.is_empty() {
                    return Ok("(no crons)".to_string());
                }
                let mut out = String::new();
                for c in crons.iter() {
                    out.push_str(&format!("#{} `{}` — {}\n", c.id, c.schedule, c.content));
                }
                Ok(out)
            }
            other => bail!("unknown cron op `{other}` — use `create`, `cancel`, or `list`"),
        }
    }
}

#[derive(serde::Deserialize)]
struct CronArgs {
    op: String,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    id: Option<u64>,
}

// ---- cron expression parsing + next-fire computation ----

/// A parsed 5-field cron schedule. `*` in a field is represented by a full
/// range; the Vixie day-of-week/day-of-month OR rule is derived from which of
/// the two is a wildcard (`dom_restricted`/`dow_restricted`).
#[derive(Debug, Clone)]
struct Schedule {
    minute: Vec<u32>,
    hour: Vec<u32>,
    dom: Vec<u32>,
    month: Vec<u32>,
    dow: Vec<u32>,
    dom_restricted: bool,
    dow_restricted: bool,
}

/// Parse a 5-field cron expression. Supported in each field: `*`, `*/n`,
/// `a-b`, `a-b/n`, `a,b,c`, and single values. Day-of-week is 0-7 (0 and 7 are
/// both Sunday). Month and weekday *names* (JAN, MON, …) are deliberately not
/// accepted — the model gets one consistent spelling to learn.
fn parse_schedule(expr: &str) -> Result<Schedule> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        bail!(
            "expected 5 fields (minute hour dom month dow), got {}",
            fields.len()
        );
    }
    let minute = parse_field(fields[0], 0, 59)?;
    let hour = parse_field(fields[1], 0, 23)?;
    let dom = parse_field(fields[2], 1, 31)?;
    let month = parse_field(fields[3], 1, 12)?;
    let dow = parse_field(fields[4], 0, 7)?;
    // Vixie cron: a restricted day-of-week plus a restricted day-of-month means
    // "either may match". A `*` on one side means that side always matches, so
    // the other side alone decides.
    let dom_restricted = !is_wildcard(fields[2]);
    let dow_restricted = !is_wildcard(fields[4]);
    Ok(Schedule {
        minute,
        hour,
        dom,
        month,
        dow,
        dom_restricted,
        dow_restricted,
    })
}

fn is_wildcard(field: &str) -> bool {
    field == "*" || field.starts_with("*/")
}

/// Parse one field into the set of values it matches. `*` expands to the whole
/// range; `*/n` to every n-th value; `a-b` (optionally `/n`) to the range (or
/// every n-th of it); `a,b,c` to the union. The Vixie convention that `7`
/// means Sunday is normalized to 0.
fn parse_field(field: &str, lo: u32, hi: u32) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| anyhow!("bad step `{s}`"))?),
            None => (part, 1),
        };
        if step == 0 {
            bail!("step cannot be 0");
        }
        let (start, end) = match range {
            "*" => (lo, hi),
            r if r.contains('-') => {
                let (a, b) = r
                    .split_once('-')
                    .ok_or_else(|| anyhow!("bad range `{r}`"))?;
                (a.parse::<u32>()?, b.parse::<u32>()?)
            }
            single => {
                let v = single.parse::<u32>()?;
                (v, v)
            }
        };
        if start < lo || end > hi || start > end {
            bail!("values in `{part}` must lie in {lo}..={hi}");
        }
        for v in (start..=end).step_by(step as usize) {
            // Normalize 7 → 0 (Sunday) for day-of-week only.
            let v = if hi == 7 && v == 7 { 0 } else { v };
            if !out.contains(&v) {
                out.push(v);
            }
        }
    }
    if out.is_empty() {
        bail!("field `{field}` matches nothing");
    }
    out.sort_unstable();
    Ok(out)
}

/// Whether `t`'s day satisfies the Vixie day rule: with both day-of-month and
/// day-of-week restricted, either may match; with either side a wildcard, only
/// the restricted side decides.
fn day_hit(sched: &Schedule, t: &chrono::DateTime<Local>) -> bool {
    let dom_hit = sched.dom.contains(&t.day());
    let dow_hit = sched.dow.contains(&t.weekday().num_days_from_sunday());
    if sched.dom_restricted && sched.dow_restricted {
        dom_hit || dow_hit
    } else {
        dom_hit && dow_hit
    }
}

fn matches(sched: &Schedule, t: &chrono::DateTime<Local>) -> bool {
    sched.month.contains(&t.month())
        && day_hit(sched, t)
        && sched.hour.contains(&t.hour())
        && sched.minute.contains(&t.minute())
}

/// The next fire strictly after `from`, or `None` when the schedule never
/// fires within [`NEXT_FIRE_HORIZON_DAYS`]. Fast-forwards by the coarsest
/// failing unit instead of stepping minute by minute — a failing month rules
/// out the whole month, a failing day the whole day — so a yearly schedule
/// costs ~526 K iterations of the old walk and a Feb-29 one ~2.1 M. Every jump
/// lands on an instant that could still match (a skipped hour, say a DST
/// spring-forward gap, is stepped over, not leapt near), so the earliest match
/// is identical to the minute walk; the horizon still caps the never-fires
/// case.
fn next_fire(expr: &str, from: chrono::DateTime<Local>) -> Option<chrono::DateTime<Local>> {
    let sched = parse_schedule(expr).ok()?;
    let horizon = from + chrono::Duration::days(NEXT_FIRE_HORIZON_DAYS);
    let mut cand = from + chrono::Duration::minutes(1);
    loop {
        if cand > horizon {
            return None;
        }
        if matches(&sched, &cand) {
            return Some(cand);
        }
        if !sched.month.contains(&cand.month()) {
            cand = next_month_start(&sched, cand);
        } else if !day_hit(&sched, &cand) {
            cand = next_local_midnight(cand);
        } else if !sched.hour.contains(&cand.hour()) {
            match next_hour_start(&sched, cand) {
                Some(t) => cand = t,
                None => cand = next_local_midnight(cand),
            }
        } else {
            // The hour matches but the minute does not: step to the next
            // matching minute, or escalate past the hour when exhausted.
            let Some(&m) = sched.minute.iter().find(|&&m| m > cand.minute()) else {
                match next_hour_start(&sched, cand) {
                    Some(t) => cand = t,
                    None => cand = next_local_midnight(cand),
                }
                continue;
            };
            // m comes from a 0..=59 field, so with_minute cannot fail; the
            // fallback only exists to keep the loop total.
            cand = cand
                .with_minute(m)
                .unwrap_or(cand + chrono::Duration::minutes(1));
        }
    }
}

/// The first day of the next month whose number is in `sched.month` (wrapping
/// into the next year), at local midnight — or, when that midnight does not
/// exist (a whole-day zone skip, practically never), the instant 24 h after
/// `from`: the loop then walks days inside the target month rather than
/// leaping past its matching days.
fn next_month_start(sched: &Schedule, from: chrono::DateTime<Local>) -> chrono::DateTime<Local> {
    let (mut year, mut month) = (from.year(), from.month());
    loop {
        month += 1;
        if month > 12 {
            month = 1;
            year += 1;
        }
        if sched.month.contains(&month) {
            break;
        }
    }
    to_local_datetime(year, month, 1, 0, 0, 0).unwrap_or(from + chrono::Duration::days(1))
}

/// The next matching hour after `from`'s hour on the same day, at minute 0 —
/// `None` when the hour list is exhausted (the caller moves to the next day).
/// An hour swallowed by a DST gap is stepped over, not returned as `None`.
fn next_hour_start(
    sched: &Schedule,
    from: chrono::DateTime<Local>,
) -> Option<chrono::DateTime<Local>> {
    sched
        .hour
        .iter()
        .filter(|&&h| h > from.hour())
        .find_map(|&h| to_local_datetime(from.year(), from.month(), from.day(), h, 0, 0))
}

/// The instant of the next day's local midnight. Falls back to `from + 24 h`
/// when midnight does not exist as a local time — the loop re-evaluates and
/// jumps again.
fn next_local_midnight(from: chrono::DateTime<Local>) -> chrono::DateTime<Local> {
    let next = from + chrono::Duration::days(1);
    to_local_datetime(next.year(), next.month(), next.day(), 0, 0, 0).unwrap_or(next)
}

/// `with_ymd_and_hms` against the local zone, choosing the earliest instant
/// when a fall-back makes the wall time ambiguous; `None` when it does not
/// exist (a spring-forward gap).
fn to_local_datetime(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<chrono::DateTime<Local>> {
    chrono::TimeZone::with_ymd_and_hms(&Local, year, month, day, hour, minute, second).earliest()
}

// ---- the per-cron scheduler task ----

/// Spawn a scheduler task for every cron in `ctx.crons` that does not already
/// have one — idempotent, so it is safe to call on every resume and after each
/// `cron create` (the create path arms just its own cron via [`arm_cron`]).
pub fn arm_crons(ctx: &ToolContext) {
    let ids: Vec<u64> = {
        let crons = ctx
            .crons
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crons.iter().map(|c| c.id).collect()
    };
    for id in ids {
        arm_cron(ctx, id);
    }
}

/// Spawn the scheduler task for cron `id` if it is not already armed. The
/// armed-set makes create + resume + `/clear`-then-recreate all safe: a cron
/// that already has a live task is not double-spawned (two tasks would race to
/// deliver the same fire).
fn arm_cron(ctx: &ToolContext, id: u64) {
    {
        let mut armed = ctx
            .cron_armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !armed.insert(id) {
            return; // already armed
        }
    }
    let mut poller_ctx = ctx.clone();
    poller_ctx.stream = None;
    tokio::spawn(async move {
        run_scheduler(poller_ctx, id).await;
    });
}

/// The scheduler loop: sleep until the next fire, deliver the reminder, repeat.
/// Exits when the cron is cancelled (its entry vanishes from the shared list),
/// clearing its armed mark so a later recreate re-arms cleanly.
async fn run_scheduler(ctx: ToolContext, id: u64) {
    loop {
        let schedule = {
            let crons = ctx
                .crons
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match crons.iter().find(|c| c.id == id) {
                Some(c) => c.schedule.clone(),
                None => {
                    drop(crons);
                    mark_unarmed(&ctx, id);
                    return; // cancelled (or /clear, or teardown)
                }
            }
        };
        let Some(next) = next_fire(&schedule, Local::now()) else {
            // Unreachable after create-time validation, but a schedule edited on
            // disk between resume and arm must not spin: stop quietly.
            return;
        };
        let wait = (next - Local::now()).to_std().unwrap_or(Duration::ZERO);
        tokio::time::sleep(wait).await;
        // Deliver the fire atomically with respect to `cron cancel`: the
        // existence check and the push share `deliver`'s crons →
        // background_tasks lock order, so a cancel that landed during the sleep
        // either left no cron to find (deliver returns false) or marks the
        // just-pushed delivery cancelled before it can be delivered.
        if !deliver(&ctx, id) {
            mark_unarmed(&ctx, id);
            return;
        }
    }
}

/// Clear a cron's armed mark — the scheduler calls this on every exit path so
/// a later recreate (same id, after `/clear` + fresh crons, or a resume racing
/// a dying task) re-arms instead of silently skipping.
fn mark_unarmed(ctx: &ToolContext, id: u64) {
    let mut armed = ctx
        .cron_armed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    armed.remove(&id);
}

/// Deliver one fire as a finished `BackgroundTask` — `drain_background`
/// (mid-turn) or the frontend's idle wake delivers it into the conversation
/// like a finished watch. The message ends with the cancel hint — the model's
/// reminder to stop the cron once its goal is achieved.
///
/// Returns false when the cron no longer exists (cancelled or cleared while
/// the scheduler slept): nothing is pushed, and the caller clears the armed
/// mark and exits. The existence check and the push run under the crons lock
/// (then the background_tasks lock — the same order `cancel` holds them), so
/// they are atomic with respect to a `cron cancel`: one that runs before
/// leaves no cron to find, one that runs after marks this delivery cancelled.
fn deliver(ctx: &ToolContext, cron_id: u64) -> bool {
    let crons = ctx
        .crons
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(content) = crons.iter().find(|c| c.id == cron_id) else {
        return false;
    };
    let id = BackgroundTask::next_id();
    let label = format!(
        "cron #{cron_id}: {}",
        crate::truncate(&content.content, 60).replace(['\n', '\r'], " ")
    );
    let result = format!(
        "[Cron reminder #{cron_id}] {}\n\n\
         If the goal behind this reminder is already achieved, cancel this cron with \
         `cron cancel {cron_id}` — say plainly why.",
        content.content
    );
    let mut v = ctx
        .background_tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    v.push(BackgroundTask {
        id,
        kind: BackgroundKind::Cron,
        tool_id: None,
        label,
        log: String::new(),
        done: true,
        result: Some(result),
        delivered: false,
        cancelled: false,
    });
    true
}

/// Mark any not-yet-delivered `BackgroundKind::Cron` delivery for this cron as
/// cancelled, so a fire that landed just before `cron cancel` is not delivered
/// after it.
fn cancel_pending_deliveries(ctx: &ToolContext, cron_id: u64) {
    let label_prefix = format!("cron #{cron_id}:");
    let mut v = ctx
        .background_tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for t in v.iter_mut() {
        if t.kind == BackgroundKind::Cron && t.label.starts_with(&label_prefix) {
            t.cancelled = true;
            t.done = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Arm every cron in the shared list — the same call the agent makes on
    /// resume. Exposed for tests via the tool's own create path; here we test
    /// the pieces directly.
    #[test]
    fn parse_field_expands_star_step_range_and_list() {
        assert_eq!(
            parse_field("*", 0, 59).unwrap(),
            (0..=59).collect::<Vec<_>>()
        );
        assert_eq!(parse_field("*/15", 0, 59).unwrap(), vec![0, 15, 30, 45]);
        assert_eq!(parse_field("1-5", 0, 23).unwrap(), vec![1, 2, 3, 4, 5]);
        assert_eq!(parse_field("0-30/10", 0, 59).unwrap(), vec![0, 10, 20, 30]);
        assert_eq!(parse_field("1,2,5", 0, 59).unwrap(), vec![1, 2, 5]);
        assert_eq!(
            parse_field("7", 0, 7).unwrap(),
            vec![0],
            "7 normalizes to Sunday"
        );
    }

    #[test]
    fn parse_field_rejects_out_of_range_and_zero_step() {
        assert!(parse_field("60", 0, 59).is_err());
        assert!(parse_field("*/0", 0, 59).is_err());
        assert!(parse_field("5-2", 0, 59).is_err());
    }

    #[test]
    fn next_fire_honors_minute_hour_and_month() {
        // Every 30 minutes, starting just after :25 → the :30 mark of the same hour.
        let from = Local.with_ymd_and_hms(2026, 8, 30, 10, 25, 0).unwrap();
        let next = next_fire("*/30 * * * *", from).unwrap();
        assert_eq!((next.hour(), next.minute()), (10, 30), "{next}");

        // Daily 9am; from 10:25 today → tomorrow 09:00.
        let next = next_fire("0 9 * * *", from).unwrap();
        assert_eq!(
            (next.month(), next.day(), next.hour(), next.minute()),
            (8, 31, 9, 0),
            "{next}"
        );

        // Weekday 9am; from Sat 2026-08-29 10:25 → Mon 2026-08-31 09:00.
        let from_sat = Local.with_ymd_and_hms(2026, 8, 29, 10, 25, 0).unwrap();
        assert_eq!(from_sat.weekday().num_days_from_sunday(), 6, "Saturday");
        let next = next_fire("0 9 * * 1-5", from_sat).unwrap();
        assert_eq!(
            (next.weekday().num_days_from_sunday(), next.hour()),
            (1, 9),
            "{next}"
        );
        assert_eq!(next.day(), 31, "{next}");
    }

    #[test]
    fn next_fire_applies_the_vixie_dom_dow_or_rule() {
        // dom=13 AND dow=Mon both restricted → either matches (OR): the next
        // Monday OR the 13th, whichever comes first. From 2026-08-10 (a
        // Monday) → that same Monday 10:30, because Monday matches.
        let from = Local.with_ymd_and_hms(2026, 8, 10, 10, 0, 0).unwrap();
        assert_eq!(from.weekday().num_days_from_sunday(), 1, "Monday");
        let next = next_fire("30 10 13 * 1", from).unwrap();
        assert_eq!(next.day(), 10, "Monday wins over the 13th: {next}");

        // dom=1 with dow=* → the AND side is a wildcard, so only the 1st matters.
        let from = Local.with_ymd_and_hms(2026, 8, 10, 10, 0, 0).unwrap();
        let next = next_fire("30 10 1 * *", from).unwrap();
        assert_eq!((next.month(), next.day()), (9, 1), "{next}");
    }

    #[test]
    fn next_fire_finds_feb_29_within_the_horizon() {
        let from = Local.with_ymd_and_hms(2026, 8, 30, 0, 0, 0).unwrap();
        let next = next_fire("0 0 29 2 *", from).unwrap();
        assert_eq!((next.month(), next.day()), (2, 29), "{next}");
    }

    #[test]
    fn next_fire_returns_none_for_a_schedule_that_never_fires() {
        let from = Local.with_ymd_and_hms(2026, 8, 30, 0, 0, 0).unwrap();
        // Feb 30 does not exist — the horizon is 4 years, and no Feb has 30 days.
        assert!(next_fire("0 0 30 2 *", from).is_none());
    }

    /// The month-jump + day-scan fast-forward must reproduce the minute walk's
    /// earliest-match result for a far-away yearly schedule, including the
    /// year wrap.
    #[test]
    fn next_fire_fast_forwards_across_months_to_the_yearly_fire() {
        let from = Local.with_ymd_and_hms(2026, 1, 3, 14, 12, 0).unwrap();
        let next = next_fire("30 9 13 6 *", from).unwrap();
        assert_eq!(
            (
                next.year(),
                next.month(),
                next.day(),
                next.hour(),
                next.minute()
            ),
            (2026, 6, 13, 9, 30),
            "{next}"
        );
        // The only fire within ~a year is after the year boundary: the month
        // list wraps to January of the next year.
        let from = Local.with_ymd_and_hms(2026, 11, 20, 23, 59, 0).unwrap();
        let next = next_fire("0 0 1 1 *", from).unwrap();
        assert_eq!(
            (next.year(), next.month(), next.day()),
            (2027, 1, 1),
            "{next}"
        );
    }

    /// A minutes list exhausted within the current hour escalates to the next
    /// matching hour, not to the next day.
    #[test]
    fn next_fire_escalates_minutes_to_the_next_hour() {
        let from = Local.with_ymd_and_hms(2026, 8, 30, 10, 50, 0).unwrap();
        let next = next_fire("45 * * * *", from).unwrap();
        assert_eq!((next.hour(), next.minute()), (11, 45), "{next}");
    }

    /// Differential oracle: the fast-forward must land on the same matching
    /// minute as the minute walk across a spread of schedules and start times,
    /// and never later than it. (The walk preserves `from`'s seconds — from
    /// 10:50:01 it returns 11:00:01 — while the fast-forward returns the
    /// match's `:00` boundary, so the two are compared per-minute; the exact
    /// second is immaterial to the scheduler, which just sleeps until the
    /// returned instant and the schedule is minute-granular.) The schedules
    /// chosen all fire within a year, so the brute-force walk stays bounded.
    #[test]
    fn next_fire_agrees_with_a_minute_walk() {
        let schedules = [
            "* * * * *",
            "*/30 * * * *",
            "0 9 * * *",
            "45 * * * *",
            "30 10 13 * 1",
            "0 9 * * 1-5",
            "5 0 1,15 * *",
            "0 12 * 6,12 *",
        ];
        let starts = [
            (2026, 1, 3, 14, 12, 0),
            (2026, 6, 13, 9, 29, 30),
            (2026, 8, 29, 10, 50, 1),
            (2026, 11, 20, 23, 59, 0),
        ];
        let mut checked = 0;
        for expr in schedules {
            let sched = parse_schedule(expr).unwrap();
            for (y, mo, d, h, mi, se) in starts {
                let from = Local.with_ymd_and_hms(y, mo, d, h, mi, se).unwrap();
                let horizon = from + chrono::Duration::days(366);
                // The reference: one minute at a time, exactly the old walk.
                let mut walk = from + chrono::Duration::minutes(1);
                let expected = loop {
                    if walk > horizon {
                        break None;
                    }
                    if matches(&sched, &walk) {
                        break Some(walk);
                    }
                    walk += chrono::Duration::minutes(1);
                };
                let got = next_fire(expr, from);
                match (got, expected) {
                    (None, None) => {}
                    (Some(g), Some(e)) => {
                        assert_eq!(
                            (g.year(), g.month(), g.day(), g.hour(), g.minute()),
                            (e.year(), e.month(), e.day(), e.hour(), e.minute()),
                            "fast-forward landed on a different minute than the walk \
                             for `{expr}` from {from}"
                        );
                        assert!(
                            g <= e,
                            "fast-forward must not return a later instant than the walk \
                             for `{expr}` from {from}: {g} vs {e}"
                        );
                    }
                    _ => panic!(
                        "fast-forward diverged from the minute walk for `{expr}` from {from}: \
                         {got:?} vs {expected:?}"
                    ),
                }
                checked += 1;
            }
        }
        assert_eq!(checked, schedules.len() * starts.len(), "all combos ran");
    }

    #[tokio::test]
    async fn create_validates_schedule_and_mints_ids() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let out = CronTool
            .execute(
                json!({"op": "create", "schedule": "*/30 * * * *", "content": "check CI"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("cron #1 set"), "{out}");
        assert!(out.contains("cron cancel 1"), "{out}");
        let crons = ctx.crons.lock().unwrap();
        assert_eq!(crons.len(), 1);
        assert_eq!(crons[0].id, 1);
        assert_eq!(crons[0].schedule, "*/30 * * * *");
    }

    #[tokio::test]
    async fn create_refuses_a_schedule_that_never_fires() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let err = CronTool
            .execute(
                json!({"op": "create", "schedule": "0 0 30 2 *", "content": "never"}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("never fires"), "{err}");
        assert!(ctx.crons.lock().unwrap().is_empty(), "nothing was created");
    }

    #[tokio::test]
    async fn create_refuses_a_malformed_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let err = CronTool
            .execute(
                json!({"op": "create", "schedule": "not a cron", "content": "x"}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid schedule"), "{err}");
    }

    #[tokio::test]
    async fn cancel_removes_the_cron_and_blocks_pending_deliveries() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        CronTool
            .execute(
                json!({"op": "create", "schedule": "*/30 * * * *", "content": "a"}),
                &ctx,
            )
            .await
            .unwrap();
        // Simulate a fire that landed but has not been delivered yet.
        assert!(deliver(&ctx, 1), "cron #1 exists, so the fire delivers");
        {
            let v = ctx.background_tasks.lock().unwrap();
            assert_eq!(v.len(), 1);
            assert!(!v[0].cancelled, "the pending delivery starts live");
        }
        let out = CronTool
            .execute(json!({"op": "cancel", "id": 1}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("cron #1 cancelled"), "{out}");
        assert!(ctx.crons.lock().unwrap().is_empty());
        let v = ctx.background_tasks.lock().unwrap();
        assert!(
            v[0].cancelled,
            "the pending delivery was cancelled with the cron"
        );
    }

    #[tokio::test]
    async fn cancel_of_unknown_cron_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let err = CronTool
            .execute(json!({"op": "cancel", "id": 9}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no cron #9"), "{err}");
    }

    #[tokio::test]
    async fn deliver_carries_the_reminder_and_the_cancel_hint() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        *ctx.crons.lock().unwrap() = vec![CronItem {
            id: 3,
            schedule: "0 9 * * *".to_string(),
            content: "review the release notes".to_string(),
        }];
        assert!(deliver(&ctx, 3), "cron #3 exists, so the fire delivers");
        let v = ctx.background_tasks.lock().unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, BackgroundKind::Cron);
        assert!(v[0].done);
        let result = v[0].result.as_deref().unwrap();
        assert!(result.contains("review the release notes"), "{result}");
        assert!(result.contains("cron cancel 3"), "{result}");
        assert!(
            result.contains("already achieved"),
            "the cancel-if-done hint rides the reminder: {result}"
        );
    }

    /// A fire whose cron was cancelled (or cleared) while the scheduler slept
    /// must not land after the cancel: `deliver` re-checks existence under the
    /// crons lock and pushes nothing, returning false — the half of the
    /// cancel-vs-fire race that used to slip a `cancelled: false` task in.
    #[tokio::test]
    async fn deliver_skips_when_the_cron_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        assert!(!deliver(&ctx, 1), "no cron #1 to deliver");
        assert!(
            ctx.background_tasks.lock().unwrap().is_empty(),
            "no stray reminder is pushed"
        );
    }

    /// `arm_crons` spawns a scheduler per cron exactly once — a second call
    /// (a resume re-arming crons whose tasks already run) must not double-spawn
    /// and double-deliver a fire.
    #[tokio::test]
    async fn arm_crons_is_idempotent_per_cron() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        // A schedule far in the future: the task arms and sleeps; a double-arm
        // would show as two armed marks.
        *ctx.crons.lock().unwrap() = vec![CronItem {
            id: 1,
            schedule: "0 0 1 1 *".to_string(),
            content: "new year".to_string(),
        }];
        arm_crons(&ctx);
        arm_crons(&ctx);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let armed = ctx.cron_armed.lock().unwrap();
        assert_eq!(armed.len(), 1, "one scheduler task per cron: {armed:?}");
    }

    /// Cancel removes the cron from the list and clears the armed mark
    /// synchronously (the scheduler may be mid-sleep until the next fire), so a
    /// later recreate with the same id re-arms.
    #[tokio::test]
    async fn cancel_clears_the_armed_mark() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        CronTool
            .execute(
                json!({"op": "create", "schedule": "0 0 1 1 *", "content": "a"}),
                &ctx,
            )
            .await
            .unwrap();
        {
            let armed = ctx.cron_armed.lock().unwrap();
            assert!(armed.contains(&1), "the create arms its cron: {armed:?}");
        }
        CronTool
            .execute(json!({"op": "cancel", "id": 1}), &ctx)
            .await
            .unwrap();
        let armed = ctx.cron_armed.lock().unwrap();
        assert!(
            !armed.contains(&1),
            "cancel clears the armed mark: {armed:?}"
        );
    }
}
