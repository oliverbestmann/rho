//! The monthly report of the web client's stats page (ADR 0075).

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::{SessionId, Store};

const HOUR_MS: i64 = 3_600_000;
/// Limits at or above this count as "near the limit".
const HIGH: f64 = 0.9;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    /// Of the requested days.
    pub totals: Totals,
    /// Of the range to compare with.
    pub previous: Totals,
    pub providers: Vec<ProviderStats>,
    pub models: Vec<ModelStats>,
    /// Cost per hour from the first day on.
    pub hourly: Vec<f64>,
    /// The working directories with the highest cost, at most ten.
    pub projects: Vec<ProjectStats>,
    /// The first request ever recorded, Unix milliseconds.
    pub first: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub cost: f64,
    pub requests: i64,
    pub sessions: i64,
    /// Hours (UTC) with at least one request.
    pub active_hours: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStats {
    pub provider: String,
    pub cost: f64,
    pub requests: i64,
    /// Cost per requested day.
    pub days: Vec<f64>,
    pub five_hour: Option<WindowStats>,
    pub seven_day: Option<WindowStats>,
}

/// One limit window over the requested days.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowStats {
    pub peak: f64,
    /// Milliseconds at 90% or more: from each such report to the next one
    /// or the window's reset.
    pub high_ms: i64,
    /// The highest report per hour from the first day on, `None` without one.
    pub hourly: Vec<Option<f64>>,
    pub latest: Sample,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub at: i64,
    pub utilization: f64,
    pub resets_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStats {
    pub provider: String,
    pub model: String,
    pub requests: i64,
    pub cost: f64,
    pub output_tokens: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStats {
    /// `None` for API proxy requests and deleted sessions.
    pub path: Option<String>,
    pub requests: i64,
    pub cost: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Per token type; requests of models without a known price add nothing.
    pub input_cost: f64,
    pub output_cost: f64,
    pub cache_read_cost: f64,
    pub cache_write_cost: f64,
}

/// All recorded requests of one session, over all epochs.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsage {
    #[serde(flatten)]
    pub totals: ProjectStats,
    /// First and last request, Unix milliseconds.
    pub first: Option<i64>,
    pub last: Option<i64>,
    /// Hours (UTC) with at least one request.
    pub active_hours: i64,
    /// Some rows are estimated from turn notes (ADR 0075).
    pub estimated: bool,
    /// Compactions and clears.
    pub epochs: i64,
    pub models: Vec<ModelStats>,
}

impl Store {
    pub async fn session_usage(&self, id: SessionId) -> Result<SessionUsage> {
        self.call(move |c| session_usage(c, &id)).await
    }

    /// The report for `days` (the bounds of each day, ascending, Unix
    /// milliseconds), compared with `[compare.0, compare.1)`.
    pub async fn stats(&self, days: Vec<i64>, compare: (i64, i64)) -> Result<Stats> {
        if days.len() < 2 || days.len() > 64 || !days.is_sorted() {
            bail!("days must be 2 to 64 ascending bounds");
        }
        self.call(move |c| stats(c, &days, compare)).await
    }
}

fn session_usage(c: &Connection, id: &SessionId) -> Result<SessionUsage> {
    let mut usage = c.query_row(
        "SELECT COALESCE(SUM(requests), 0), COALESCE(SUM(cost), 0), \
         COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0), \
         COALESCE(SUM(cache_read_tokens), 0), COALESCE(SUM(cache_write_tokens), 0), \
         TOTAL(input_cost), TOTAL(output_cost), TOTAL(cache_read_cost), \
         TOTAL(cache_write_cost), MIN(created_at), MAX(created_at), \
         COUNT(DISTINCT created_at / 3600000), COALESCE(MAX(estimated), 0) \
         FROM requests WHERE session_id = ?1",
        [id],
        |r| {
            Ok(SessionUsage {
                totals: ProjectStats {
                    path: None,
                    requests: r.get(0)?,
                    cost: r.get(1)?,
                    input_tokens: r.get(2)?,
                    output_tokens: r.get(3)?,
                    cache_read_tokens: r.get(4)?,
                    cache_write_tokens: r.get(5)?,
                    input_cost: r.get(6)?,
                    output_cost: r.get(7)?,
                    cache_read_cost: r.get(8)?,
                    cache_write_cost: r.get(9)?,
                },
                first: r.get(10)?,
                last: r.get(11)?,
                active_hours: r.get(12)?,
                estimated: r.get::<_, i64>(13)? == 1,
                ..Default::default()
            })
        },
    )?;
    usage.epochs = c
        .query_row("SELECT epoch FROM sessions WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .unwrap_or(0);
    let mut select = c.prepare(
        "SELECT provider, model, SUM(requests), SUM(cost), SUM(output_tokens) FROM requests \
         WHERE session_id = ?1 GROUP BY provider, model \
         ORDER BY SUM(cost) DESC, SUM(requests) DESC",
    )?;
    usage.models = select
        .query_map([id], |r| {
            Ok(ModelStats {
                provider: r.get(0)?,
                model: r.get(1)?,
                requests: r.get(2)?,
                cost: r.get(3)?,
                output_tokens: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(usage)
}

fn totals(c: &Connection, from: i64, to: i64) -> Result<Totals> {
    Ok(c.query_row(
        "SELECT COALESCE(SUM(cost), 0), COALESCE(SUM(requests), 0), COUNT(DISTINCT session_id), \
         COUNT(DISTINCT created_at / 3600000), COALESCE(SUM(input_tokens), 0), \
         COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cache_read_tokens), 0), \
         COALESCE(SUM(cache_write_tokens), 0) \
         FROM requests WHERE created_at >= ?1 AND created_at < ?2",
        [from, to],
        |r| {
            Ok(Totals {
                cost: r.get(0)?,
                requests: r.get(1)?,
                sessions: r.get(2)?,
                active_hours: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                cache_read_tokens: r.get(6)?,
                cache_write_tokens: r.get(7)?,
            })
        },
    )?)
}

/// Limit reports of one window, in time order.
#[derive(Default)]
struct Window(Vec<Sample>);

impl Window {
    fn stats(self, from: i64, to: i64) -> Option<WindowStats> {
        let latest = *self.0.last()?;
        let hours = usize::try_from((to - from + HOUR_MS - 1) / HOUR_MS).unwrap_or(0);
        let mut out = WindowStats {
            hourly: vec![None; hours],
            latest,
            ..Default::default()
        };
        for (i, s) in self.0.iter().enumerate() {
            out.peak = out.peak.max(s.utilization);
            if let Some(h) = out.hourly.get_mut(((s.at - from) / HOUR_MS) as usize) {
                *h = Some(h.map_or(s.utilization, |v: f64| v.max(s.utilization)));
            }
            if s.utilization >= HIGH {
                let next = self.0.get(i + 1).map_or(to, |n| n.at);
                out.high_ms += (next.min(s.resets_at).min(to) - s.at).max(0);
            }
        }
        Some(out)
    }
}

fn stats(c: &Connection, days: &[i64], compare: (i64, i64)) -> Result<Stats> {
    let (from, to) = (days[0], days[days.len() - 1]);
    // Running months end now, not at the end of the last day.
    let now = super::now_ms();
    let mut providers: BTreeMap<String, (ProviderStats, Window, Window)> = BTreeMap::new();
    let mut hourly = vec![0.0; usize::try_from((to - from + HOUR_MS - 1) / HOUR_MS).unwrap_or(0)];
    let mut select = c.prepare(
        "SELECT created_at, provider, requests, cost, limit_5h, limit_5h_reset, limit_7d, \
         limit_7d_reset FROM requests WHERE created_at >= ?1 AND created_at < ?2 \
         ORDER BY created_at",
    )?;
    let mut rows = select.query(params![from, to])?;
    while let Some(r) = rows.next()? {
        let at: i64 = r.get(0)?;
        let provider: String = r.get(1)?;
        let (p, five, seven) = providers.entry(provider.clone()).or_insert_with(|| {
            let stats = ProviderStats {
                provider,
                days: vec![0.0; days.len() - 1],
                ..Default::default()
            };
            (stats, Window::default(), Window::default())
        });
        let cost: f64 = r.get(3)?;
        p.requests += r.get::<_, i64>(2)?;
        p.cost += cost;
        let day = days.partition_point(|&d| d <= at) - 1;
        p.days[day] += cost;
        hourly[((at - from) / HOUR_MS) as usize] += cost;
        for (window, col) in [(five, 4), (seven, 6)] {
            if let (Some(utilization), Some(resets_at)) = (r.get(col)?, r.get(col + 1)?) {
                window.0.push(Sample {
                    at,
                    utilization,
                    resets_at,
                });
            }
        }
    }
    let end = to.min(now.max(from));
    let providers = providers
        .into_values()
        .map(|(mut p, five, seven)| {
            p.five_hour = five.stats(from, end);
            p.seven_day = seven.stats(from, end);
            p
        })
        .collect();

    let mut select = c.prepare(
        "SELECT provider, model, SUM(requests), SUM(cost), SUM(output_tokens) FROM requests \
         WHERE created_at >= ?1 AND created_at < ?2 GROUP BY provider, model \
         ORDER BY SUM(cost) DESC, SUM(requests) DESC",
    )?;
    let models = select
        .query_map([from, to], |r| {
            Ok(ModelStats {
                provider: r.get(0)?,
                model: r.get(1)?,
                requests: r.get(2)?,
                cost: r.get(3)?,
                output_tokens: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut select = c.prepare(
        "SELECT s.cwd, SUM(r.requests), SUM(r.cost), SUM(r.input_tokens), \
         SUM(r.output_tokens), SUM(r.cache_read_tokens), SUM(r.cache_write_tokens), \
         TOTAL(r.input_cost), TOTAL(r.output_cost), TOTAL(r.cache_read_cost), \
         TOTAL(r.cache_write_cost) FROM requests r \
         LEFT JOIN sessions s ON s.id = r.session_id \
         WHERE r.created_at >= ?1 AND r.created_at < ?2 GROUP BY s.cwd \
         ORDER BY SUM(r.cost) DESC, SUM(r.requests) DESC LIMIT 10",
    )?;
    let projects = select
        .query_map([from, to], |r| {
            Ok(ProjectStats {
                path: r.get(0)?,
                requests: r.get(1)?,
                cost: r.get(2)?,
                input_tokens: r.get(3)?,
                output_tokens: r.get(4)?,
                cache_read_tokens: r.get(5)?,
                cache_write_tokens: r.get(6)?,
                input_cost: r.get(7)?,
                output_cost: r.get(8)?,
                cache_read_cost: r.get(9)?,
                cache_write_cost: r.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let first = c.query_row("SELECT MIN(created_at) FROM requests", [], |r| r.get(0))?;
    Ok(Stats {
        totals: totals(c, from, to)?,
        previous: totals(c, compare.0, compare.1)?,
        providers,
        models,
        hourly,
        projects,
        first,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{LimitWindow, Limits, ModelSpec, Usage};
    use crate::store::RequestRecord;

    fn record(at: i64, model: &str, cost: f64, seven_day: Option<(f64, i64)>) -> RequestRecord {
        let (provider, id) = model.split_once('/').unwrap();
        RequestRecord {
            created_at: at,
            session_id: None,
            model: ModelSpec::new(provider, id),
            usage: Usage {
                input: 1,
                output: 10,
                cache_read: 3,
                cache_write: 0,
            },
            cost,
            costs: None,
            limits: Limits {
                five_hour: None,
                seven_day: seven_day.map(|(utilization, resets_at)| LimitWindow {
                    utilization,
                    resets_at,
                }),
            },
        }
    }

    #[tokio::test]
    async fn sums_days_providers_models_and_limits() {
        let store = Store::memory().unwrap();
        store
            .call(|c| {
                c.execute(
                    "INSERT INTO sessions (id, cwd, model, thinking, created_at, updated_at) \
                     VALUES ('s', '/w/rho', 'a/x', 'off', 0, 0)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let h = HOUR_MS;
        let in_session = RequestRecord {
            session_id: Some(crate::store::SessionId::from_static("s")),
            costs: Some([0.1, 0.6, 0.2, 0.1]),
            ..record(h, "a/x", 1.0, Some((0.5, 100 * h)))
        };
        for r in [
            record(-h, "a/x", 9.0, None), // the day before, compared
            in_session,
            record(2 * h, "a/x", 2.0, Some((0.95, 100 * h))),
            record(2 * h + 1, "a/y", 0.5, Some((0.92, 100 * h))),
            record(26 * h, "b/z", 4.0, None),
        ] {
            store.record_request(r).await.unwrap();
        }
        let stats = store
            .stats(vec![0, 24 * h, 48 * h], (-24 * h, 0))
            .await
            .unwrap();
        assert_eq!(stats.totals.requests, 4);
        assert_eq!(stats.totals.cost, 7.5);
        assert_eq!(stats.totals.active_hours, 3);
        assert_eq!(stats.totals.output_tokens, 40);
        assert_eq!(stats.previous.cost, 9.0);
        assert_eq!(stats.first, Some(-h));

        let a = &stats.providers[0];
        assert_eq!((a.provider.as_str(), a.requests, a.cost), ("a", 3, 3.5));
        assert_eq!(a.days, [3.5, 0.0]);
        assert_eq!(stats.providers[1].days, [0.0, 4.0]);
        assert_eq!(stats.providers[1].seven_day, None);

        let seven = a.seven_day.as_ref().unwrap();
        assert_eq!(seven.peak, 0.95);
        assert_eq!(seven.hourly.len(), 48);
        assert_eq!(seven.hourly[..3], [None, Some(0.5), Some(0.95)]);
        // From the first high report to the end of the range, the reset is later.
        assert_eq!(seven.high_ms, 46 * h);
        assert_eq!(seven.latest.utilization, 0.92);

        let models: Vec<_> = stats.models.iter().map(|m| m.model.as_str()).collect();
        assert_eq!(models, ["z", "x", "y"]);
        let projects: Vec<_> = stats
            .projects
            .iter()
            .map(|p| (p.path.as_deref(), p.cost))
            .collect();
        assert_eq!(projects, [(None, 6.5), (Some("/w/rho"), 1.0)]);
        let rho = &stats.projects[1];
        assert_eq!((rho.output_tokens, rho.output_cost), (10, 0.6));
        assert_eq!(stats.projects[0].input_cost, 0.0);
        assert_eq!(stats.hourly.len(), 48);
        assert_eq!(stats.hourly[..3], [0.0, 1.0, 2.5]);
        assert!(store.stats(vec![1, 0], (0, 0)).await.is_err());

        let s = store
            .session_usage(crate::store::SessionId::from_static("s"))
            .await
            .unwrap();
        assert_eq!((s.totals.requests, s.totals.cost), (1, 1.0));
        assert_eq!(
            (s.totals.output_cost, s.first, s.last),
            (0.6, Some(h), Some(h))
        );
        assert_eq!((s.active_hours, s.estimated, s.epochs), (1, false, 0));
        assert_eq!(s.models.len(), 1);
        let none = store
            .session_usage(crate::store::SessionId::from_static("t"))
            .await
            .unwrap();
        assert_eq!(
            (none.totals.requests, none.first, none.models.len()),
            (0, None, 0)
        );
    }
}
