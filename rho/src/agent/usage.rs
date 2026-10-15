//! Usage reports: `/usage` and the line after each turn.

use std::time::Duration;

use super::Session;
use crate::provider::Usage;

impl Session {
    /// Markdown summary of token, cache and cost usage, for `/usage`.
    pub fn usage_report(&self) -> String {
        let (last, total) = (&self.last_usage, &self.usage);
        let mut out = format!("**Usage** · `{}`\n\n", self.model);
        if total.prompt() + total.output == 0 {
            out += "No requests yet.";
            return out;
        }
        out += "| | Last request | Session |\n|---|---:|---:|\n";
        for (label, f) in [
            (
                "Input (uncached)",
                (|u: &Usage| u.input) as fn(&Usage) -> u64,
            ),
            ("Cache read", |u| u.cache_read),
            ("Cache write", |u| u.cache_write),
            // Every input token is one of the three above.
            ("**Input total**", |u| u.prompt()),
            ("Output", |u| u.output),
        ] {
            out += &format!("| {label} | {} | {} |\n", count(f(last)), count(f(total)));
        }
        out += &format!(
            "| Cache hit rate | {} | {} |\n\n",
            hit_rate(last),
            hit_rate(total)
        );
        let info = self.info();
        let (used, window) = (info.context_used(), info.context_window.max(1));
        out += &format!(
            "- Context: {} / {} ({:.1}%)\n",
            count(used),
            count(window),
            used as f64 * 100.0 / window as f64
        );
        if self.cost > 0.0 {
            out += &format!("- Cost: ${:.4}\n", self.cost);
        }
        let ttl = crate::compact::CACHE_TTL;
        out += &match self.last_request.map(|t| t.elapsed()) {
            Some(age) if age < ttl => {
                let left = (ttl - age).as_secs();
                format!(
                    "- Cache: warm, expires in ~{}m {:02}s",
                    left / 60,
                    left % 60
                )
            }
            _ => format!(
                "- Cache: cold (no request in the last {}m)",
                ttl.as_secs() / 60
            ),
        };
        out
    }
}

/// 1234567 -> "1,234,567".
pub(crate) fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One line: "2 requests · 3.4s · input 1,030 (98% cached, 0 written) · output 12 · $0.0123".
pub(super) fn turn_report(requests: u32, took: Duration, u: &Usage, cost: f64) -> String {
    let mut out = format!(
        "{requests} request{} · {:.1}s · input {} ({} cached, {} written) · output {}",
        if requests == 1 { "" } else { "s" },
        took.as_secs_f64(),
        count(u.prompt()),
        hit_rate(u),
        count(u.cache_write),
        count(u.output),
    );
    if cost > 0.0 {
        out += &format!(" · ${cost:.4}");
    }
    out
}

/// Share of the prompt read from cache.
fn hit_rate(u: &Usage) -> String {
    match u.prompt() {
        0 => "–".into(),
        p => format!("{:.0}%", u.cache_read as f64 * 100.0 / p as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_report_sums_up_the_run() {
        let u = Usage {
            input: 20,
            cache_read: 1010,
            cache_write: 1000,
            output: 12,
        };
        assert_eq!(
            turn_report(2, Duration::from_millis(3400), &u, 0.01234),
            "2 requests · 3.4s · input 2,030 (50% cached, 1,000 written) · output 12 · $0.0123"
        );
        assert!(!turn_report(1, Duration::ZERO, &u, 0.0).contains('$'));
    }
}
