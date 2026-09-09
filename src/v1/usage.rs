//! Sampling what a run actually cost.
//!
//! For subscription providers the meaningful price is not tokens or dollars but **share of
//! a rolling window**: a planning call that consumes 4% of a five-hour limit has cost
//! something real, and no invoice will tell you so. Each provider reports this differently
//! — Grok a weekly percentage, Qwen and Muse cumulative tokens, Codex per-window
//! percentages — so samples are stored raw and compared later.
//!
//! **Not every reading is usable.** The muse and qwen probes start a fresh CLI session and
//! ask it for usage, so they report that session's tokens rather than the account's — always
//! near zero, and no use as a cost signal. Only a percentage of a stated window measures an
//! account. Where no usable reading exists, record the figure by hand:
//! `firm usage --provider codex --percent 4`.
//!
//! Sampled once before and once after, not per call. Reading usage costs seconds per
//! provider, and a run that measured itself between every attempt would spend more time
//! measuring than working.

use crate::{config::Config, usage as v0};
use serde::Serialize;

/// One reading of one provider's consumption, at a moment.
#[derive(Clone, Debug, Serialize)]
pub struct Sample {
    pub provider: String,
    /// `percent` of a stated window, or `tokens` counted cumulatively.
    pub metric: String,
    pub label: String,
    pub value: f64,
}

/// Read every enabled provider's usage. Failures are silent by omission: a provider whose
/// CLI cannot be probed simply has no sample, which is honest — an absent reading must
/// never be recorded as zero consumption.
pub async fn sample(config: &Config) -> Vec<Sample> {
    let mut samples = Vec::new();
    for provider in config.providers.iter().filter(|p| p.enabled) {
        let reading = match provider.id.as_str() {
            "grok" => v0::probe_grok_usage(&provider.command, &config.workspace).await.ok(),
            "qwen" => v0::probe_qwen_usage(&provider.command, &config.workspace).await.ok(),
            "muse" => v0::probe_muse_usage(&provider.command, &config.workspace).await.ok(),
            // Codex reports through app-server, which a board run does not hold open.
            _ => None,
        };
        let Some(reading) = reading else { continue };
        if let Some(percent) = reading.get("used_percent").and_then(serde_json::Value::as_f64) {
            samples.push(Sample {
                provider: provider.id.clone(),
                metric: "percent".into(),
                label: reading
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("limit")
                    .to_string(),
                value: percent,
            });
        }
        if let Some(tokens) = reading.get("total_tokens").and_then(serde_json::Value::as_f64) {
            samples.push(Sample {
                provider: provider.id.clone(),
                metric: "tokens".into(),
                label: "total tokens".into(),
                value: tokens,
            });
        }
    }
    samples
}

/// What was consumed between two readings, keyed by provider and metric.
///
/// A metric present in only one of the two samples is dropped rather than treated as a
/// change from zero: providers report cumulative totals, and a missing reading means the
/// probe failed, not that nothing was used.
pub fn consumed(before: &[Sample], after: &[Sample]) -> Vec<Sample> {
    let mut deltas = Vec::new();
    for later in after {
        let Some(earlier) = before
            .iter()
            .find(|s| s.provider == later.provider && s.metric == later.metric)
        else {
            continue;
        };
        let change = later.value - earlier.value;
        // A window that reset between readings goes backwards; report nothing rather than
        // a negative cost.
        if change > 0.0 {
            deltas.push(Sample {
                value: change,
                ..later.clone()
            });
        }
    }
    deltas
}

/// How a consumption figure reads to a person.
pub fn describe(sample: &Sample) -> String {
    if sample.metric == "percent" {
        format!(
            "{} +{:.1}% of {}",
            sample.provider, sample.value, sample.label
        )
    } else {
        format!(
            "{} +{} tokens",
            sample.provider,
            sample.value as u64
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(provider: &str, metric: &str, value: f64) -> Sample {
        Sample {
            provider: provider.into(),
            metric: metric.into(),
            label: "Weekly limit".into(),
            value,
        }
    }

    #[test]
    fn consumption_is_the_difference_and_never_invented() {
        let before = vec![sample("grok", "percent", 12.0), sample("muse", "tokens", 1000.0)];
        let after = vec![
            sample("grok", "percent", 14.5),
            sample("muse", "tokens", 1340.0),
            sample("qwen", "tokens", 500.0), // no earlier reading
        ];
        let used = consumed(&before, &after);
        assert_eq!(used.len(), 2, "a metric with no baseline is dropped, not counted from zero");
        let grok = used.iter().find(|s| s.provider == "grok").unwrap();
        assert!((grok.value - 2.5).abs() < f64::EPSILON);
        assert_eq!(describe(grok), "grok +2.5% of Weekly limit");
        let muse = used.iter().find(|s| s.provider == "muse").unwrap();
        assert_eq!(describe(muse), "muse +340 tokens");
    }

    #[test]
    fn a_window_that_reset_is_not_reported_as_negative_cost() {
        let before = vec![sample("grok", "percent", 96.0)];
        let after = vec![sample("grok", "percent", 3.0)];
        assert!(consumed(&before, &after).is_empty());
    }
}
