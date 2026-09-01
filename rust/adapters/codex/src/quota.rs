use serde::Serialize;
use std::collections::BTreeMap;

use crate::{CodexGroup, parse_ts_timestamp};

const RECENT_WINDOW_MILLIS: i64 = 90 * 24 * 60 * 60 * 1_000;
const PROVISIONAL_MINIMUM_AGE_MILLIS: i64 = 24 * 60 * 60 * 1_000;
const RESET_HORIZON_TOLERANCE_SECONDS: i64 = 5 * 60;
const MINIMUM_USED_PERCENT_SPAN: f64 = 20.0;

pub(super) const fn recent_window_start_millis(now_millis: i64) -> i64 {
    now_millis.saturating_sub(RECENT_WINDOW_MILLIS)
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexWeeklyRateLimitObservation {
    pub timestamp: String,
    pub used_percent: f64,
    pub resets_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexWeeklyQuotaEstimateStatus {
    Completed,
    Provisional,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexWeeklyQuotaEstimate {
    pub started_at: String,
    pub ended_at: String,
    pub used_percent_start: f64,
    pub used_percent_end: f64,
    pub observed_cost_usd: f64,
    pub estimated_weekly_cost_usd: f64,
    pub sample_count: usize,
    pub status: CodexWeeklyQuotaEstimateStatus,
}

#[derive(Debug)]
pub struct CodexDailyUsageWithQuotaEstimates {
    pub groups: BTreeMap<String, CodexGroup>,
    pub weekly_rate_limit_samples: usize,
    pub weekly_quota_estimates: Vec<CodexWeeklyQuotaEstimate>,
}

pub(super) fn estimate_weekly_quota_costs_from_timeline(
    costs: &BTreeMap<i64, f64>,
    observations: &[CodexWeeklyRateLimitObservation],
    now_millis: i64,
) -> Vec<CodexWeeklyQuotaEstimate> {
    let since_millis = recent_window_start_millis(now_millis);
    let mut observations = observations
        .iter()
        .filter_map(|observation| {
            let timestamp = parse_ts_timestamp(&observation.timestamp)?.as_millis();
            (timestamp >= since_millis
                && timestamp <= now_millis
                && (0.0..=100.0).contains(&observation.used_percent))
            .then_some((timestamp, observation))
        })
        .collect::<Vec<_>>();
    observations.sort_by_key(|(timestamp, _)| *timestamp);
    observations.dedup_by(|left, right| {
        left.0 == right.0
            && left.1.used_percent == right.1.used_percent
            && left.1.resets_at == right.1.resets_at
    });

    let mut estimates = Vec::new();
    let mut episode: Option<QuotaEpisode<'_>> = None;
    for (timestamp, observation) in observations {
        let Some(current) = episode.as_mut() else {
            episode = Some(QuotaEpisode::new(timestamp, observation));
            continue;
        };
        if observation.resets_at
            < current
                .reset_horizon
                .saturating_sub(RESET_HORIZON_TOLERANCE_SECONDS)
        {
            continue;
        }
        let horizon_advanced = observation.resets_at
            > current
                .reset_horizon
                .saturating_add(RESET_HORIZON_TOLERANCE_SECONDS);
        let percent_dropped = observation.used_percent + 0.5 < current.end.used_percent;
        if horizon_advanced && percent_dropped {
            if let Some(estimate) =
                estimate_episode(current, costs, CodexWeeklyQuotaEstimateStatus::Completed)
            {
                estimates.push(estimate);
            }
            episode = Some(QuotaEpisode::new(timestamp, observation));
            continue;
        }
        if percent_dropped {
            continue;
        }
        current.reset_horizon = current.reset_horizon.max(observation.resets_at);
        current.end_millis = timestamp;
        current.end = observation;
        current.sample_count += 1;
    }

    if let Some(episode) = episode
        && now_millis.saturating_sub(episode.start_millis) >= PROVISIONAL_MINIMUM_AGE_MILLIS
        && let Some(estimate) =
            estimate_episode(&episode, costs, CodexWeeklyQuotaEstimateStatus::Provisional)
    {
        estimates.push(estimate);
    }
    estimates
}

struct QuotaEpisode<'a> {
    start_millis: i64,
    end_millis: i64,
    start: &'a CodexWeeklyRateLimitObservation,
    end: &'a CodexWeeklyRateLimitObservation,
    reset_horizon: i64,
    sample_count: usize,
}

impl<'a> QuotaEpisode<'a> {
    fn new(timestamp: i64, observation: &'a CodexWeeklyRateLimitObservation) -> Self {
        Self {
            start_millis: timestamp,
            end_millis: timestamp,
            start: observation,
            end: observation,
            reset_horizon: observation.resets_at,
            sample_count: 1,
        }
    }
}

fn estimate_episode(
    episode: &QuotaEpisode<'_>,
    costs: &BTreeMap<i64, f64>,
    status: CodexWeeklyQuotaEstimateStatus,
) -> Option<CodexWeeklyQuotaEstimate> {
    let used_percent_span = episode.end.used_percent - episode.start.used_percent;
    if used_percent_span < MINIMUM_USED_PERCENT_SPAN {
        return None;
    }
    let observed_cost_usd = costs
        .range((episode.start_millis + 1)..=episode.end_millis)
        .map(|(_, cost)| cost)
        .sum::<f64>();
    if observed_cost_usd <= 0.0 {
        return None;
    }
    Some(CodexWeeklyQuotaEstimate {
        started_at: episode.start.timestamp.clone(),
        ended_at: episode.end.timestamp.clone(),
        used_percent_start: episode.start.used_percent,
        used_percent_end: episode.end.used_percent,
        observed_cost_usd,
        estimated_weekly_cost_usd: observed_cost_usd * 100.0 / used_percent_span,
        sample_count: episode.sample_count,
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_a_completed_week_from_priced_requests_after_the_anchor() {
        let costs = costs(&[
            ("2026-08-01T00:00:00Z", 1.0),
            ("2026-08-02T00:00:00Z", 1.0),
            ("2026-08-03T00:00:00Z", 1.0),
        ]);
        let observations = [
            observation("2026-08-01T00:00:00Z", 25.0, 1_786_051_200),
            observation("2026-08-02T00:00:00Z", 50.0, 1_786_051_200),
            observation("2026-08-03T00:00:00Z", 75.0, 1_786_051_200),
            observation("2026-08-04T00:00:00Z", 0.0, 1_786_656_000),
        ];

        let estimates =
            estimate_weekly_quota_costs_from_timeline(&costs, &observations, 1_786_406_400_000);

        assert_eq!(estimates.len(), 1);
        let estimate = &estimates[0];
        assert_eq!(estimate.started_at, "2026-08-01T00:00:00Z");
        assert_eq!(estimate.ended_at, "2026-08-03T00:00:00Z");
        assert_eq!(estimate.used_percent_start, 25.0);
        assert_eq!(estimate.used_percent_end, 75.0);
        assert!((estimate.observed_cost_usd - 2.0).abs() < f64::EPSILON);
        assert!((estimate.estimated_weekly_cost_usd - 4.0).abs() < f64::EPSILON);
        assert_eq!(estimate.sample_count, 3);
        assert_eq!(estimate.status, CodexWeeklyQuotaEstimateStatus::Completed);
    }

    #[test]
    fn ignores_same_horizon_regressions_and_stale_snapshots_after_a_reset() {
        let costs = costs(&[
            ("2026-08-01T00:00:00Z", 1.0),
            ("2026-08-02T00:00:00Z", 1.0),
            ("2026-08-03T00:00:00Z", 1.0),
        ]);
        let observations = [
            observation("2026-08-01T00:00:00Z", 20.0, 1_786_051_200),
            observation("2026-08-01T12:00:00Z", 19.0, 1_786_051_200),
            observation("2026-08-02T00:00:00Z", 50.0, 1_786_051_201),
            observation("2026-08-03T00:00:00Z", 0.0, 1_786_656_000),
            observation("2026-08-03T00:00:01Z", 51.0, 1_786_051_200),
        ];

        let estimates =
            estimate_weekly_quota_costs_from_timeline(&costs, &observations, 1_786_406_400_000);

        assert_eq!(estimates.len(), 1);
        assert_eq!(estimates[0].used_percent_start, 20.0);
        assert_eq!(estimates[0].used_percent_end, 50.0);
        assert_eq!(estimates[0].sample_count, 2);
    }

    #[test]
    fn suppresses_a_new_provisional_episode_during_its_first_day() {
        let costs = costs(&[("2026-08-03T12:00:00Z", 1.0), ("2026-08-03T18:00:00Z", 1.0)]);
        let observations = [
            observation("2026-08-03T12:00:00Z", 0.0, 1_786_656_000),
            observation("2026-08-03T18:00:00Z", 30.0, 1_786_656_000),
        ];

        let estimates =
            estimate_weekly_quota_costs_from_timeline(&costs, &observations, 1_785_783_600_000);

        assert!(estimates.is_empty());
    }

    fn costs(entries: &[(&str, f64)]) -> BTreeMap<i64, f64> {
        entries
            .iter()
            .map(|(timestamp, cost)| (parse_ts_timestamp(timestamp).unwrap().as_millis(), *cost))
            .collect()
    }

    fn observation(
        timestamp: &str,
        used_percent: f64,
        resets_at: i64,
    ) -> CodexWeeklyRateLimitObservation {
        CodexWeeklyRateLimitObservation {
            timestamp: timestamp.to_string(),
            used_percent,
            resets_at,
        }
    }
}
