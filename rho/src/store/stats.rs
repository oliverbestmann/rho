//! The monthly report of the web client's stats page (ADR 0075).

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;

use super::Store;

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
    /// The first request ever recorded, Unix milliseconds.
    pub first: Option<i64>,
    /// The last row estimated from turn notes.
    pub estimated_until: Option<i64>,
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

impl Store {
    /// The report for `days` (the bounds of each day, ascending, Unix
    /// milliseconds), compared with `[compare.0, compare.1)`.
    pub async fn stats(&self, days: Vec<i64>, compare: (i64, i64)) -> Result<Stats> {
        if days.len() < 2 || days.len() > 64 || !days.is_sorted() {
            bail!("days must be 2 to 64 ascending bounds");
        }
        self.call(move |c| stats(c, &days, compare)).await
    }
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
    let (first, estimated_until) = c.query_row(
        "SELECT MIN(created_at), (SELECT MAX(created_at) FROM requests WHERE estimated) \
         FROM requests",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(Stats {
        totals: totals(c, from, to)?,
        previous: totals(c, compare.0, compare.1)?,
        providers,
        models,
        first,
        estimated_until,
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
        let h = HOUR_MS;
        for r in [
            record(-h, "a/x", 9.0, None), // the day before, compared
            record(h, "a/x", 1.0, Some((0.5, 100 * h))),
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
        assert_eq!(stats.estimated_until, None);

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
        assert!(store.stats(vec![1, 0], (0, 0)).await.is_err());
    }
}
