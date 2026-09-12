use serde::Serialize;
use std::collections::BTreeMap;

use crate::{CodexGroup, parse_ts_timestamp};

const RECENT_WINDOW_MILLIS: i64 = 90 * 24 * 60 * 60 * 1_000;
const RESET_HORIZON_TOLERANCE_SECONDS: i64 = 5 * 60;
const RESET_HANDOFF_TOLERANCE_MILLIS: i64 = 5 * 60 * 1_000;
const MINIMUM_USED_PERCENT_SPAN: f64 = 5.0;

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
    let mut unique_observations = Vec::with_capacity(observations.len());
    let mut index = 0;
    while index < observations.len() {
        let timestamp = observations[index].0;
        let mut end = index + 1;
        while end < observations.len() && observations[end].0 == timestamp {
            end += 1;
        }
        if end == index + 1 {
            unique_observations.push(observations[index]);
        }
        index = end;
    }

    let mut estimates = Vec::new();
    let mut episode: Option<QuotaEpisode<'_>> = None;
    for (timestamp, observation) in unique_observations {
        let Some(current) = episode.as_mut() else {
            episode = Some(QuotaEpisode::new(timestamp, observation));
            continue;
        };
        let horizon_advanced = observation.resets_at
            > current
                .reset_horizon
                .saturating_add(RESET_HORIZON_TOLERANCE_SECONDS);
        let percent_dropped = observation.used_percent + 0.5 < current.end.used_percent;
        if horizon_advanced
            && (percent_dropped || current.continuity == QuotaEpisodeContinuity::Frozen)
        {
            if let Some(estimate) =
                estimate_episode(current, costs, CodexWeeklyQuotaEstimateStatus::Completed)
            {
                estimates.push(estimate);
            }
            episode = Some(QuotaEpisode::new(timestamp, observation));
            continue;
        }
        if current.continuity == QuotaEpisodeContinuity::Frozen {
            continue;
        }
        if observation.resets_at
            < current
                .reset_horizon
                .saturating_sub(RESET_HORIZON_TOLERANCE_SECONDS)
        {
            let is_reset_handoff = timestamp.saturating_sub(current.start_millis)
                <= RESET_HANDOFF_TOLERANCE_MILLIS
                && current.end.used_percent <= current.start.used_percent + 0.5;
            if is_reset_handoff {
                continue;
            }
            current.continuity = QuotaEpisodeContinuity::Frozen;
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
        && let Some(estimate) =
            estimate_episode(&episode, costs, CodexWeeklyQuotaEstimateStatus::Provisional)
    {
        estimates.push(estimate);
    }
    estimates
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuotaEpisodeContinuity {
    Tracking,
    Frozen,
}

struct QuotaEpisode<'a> {
    start_millis: i64,
    end_millis: i64,
    start: &'a CodexWeeklyRateLimitObservation,
    end: &'a CodexWeeklyRateLimitObservation,
    reset_horizon: i64,
    sample_count: usize,
    continuity: QuotaEpisodeContinuity,
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
            continuity: QuotaEpisodeContinuity::Tracking,
        }
    }
}

fn estimate_episode(
    episode: &QuotaEpisode<'_>,
    costs: &BTreeMap<i64, f64>,
    status: CodexWeeklyQuotaEstimateStatus,
) -> Option<CodexWeeklyQuotaEstimate> {
    let used_percent_span = episode.end.used_percent - episode.start.used_percent;
    if used_percent_span <= MINIMUM_USED_PERCENT_SPAN {
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
    fn ignores_same_horizon_percentage_regressions() {
        let costs = costs(&[
            ("2026-08-01T00:00:00Z", 1.0),
            ("2026-08-01T12:00:00Z", 1.0),
            ("2026-08-02T00:00:00Z", 1.0),
        ]);
        let observations = [
            observation("2026-08-01T00:00:00Z", 20.0, 1_786_051_200),
            observation("2026-08-01T12:00:00Z", 19.0, 1_786_051_200),
            observation("2026-08-02T00:00:00Z", 50.0, 1_786_051_201),
        ];

        let estimates = estimate_weekly_quota_costs_from_timeline(
            &costs,
            &observations,
            parse_ts_timestamp("2026-08-03T00:00:00Z")
                .unwrap()
                .as_millis(),
        );

        assert_eq!(estimates.len(), 1);
        assert_eq!(estimates[0].used_percent_start, 20.0);
        assert_eq!(estimates[0].used_percent_end, 50.0);
        assert!((estimates[0].observed_cost_usd - 2.0).abs() < f64::EPSILON);
        assert_eq!(estimates[0].sample_count, 2);
        assert_eq!(
            estimates[0].status,
            CodexWeeklyQuotaEstimateStatus::Provisional
        );
    }

    #[test]
    fn freezes_an_episode_when_a_stale_rate_limit_snapshot_recovers() {
        let costs = costs(&[
            ("2026-09-08T01:31:42.408Z", 1.0),
            ("2026-09-09T16:41:17.572Z", 2.0),
            ("2026-09-09T16:41:39.755Z", 3.0),
            ("2026-09-09T17:41:32.956Z", 5.0),
            ("2026-09-09T17:49:50.490Z", 7.0),
        ]);
        let observations = [
            observation("2026-09-08T01:29:22.343Z", 71.0, 1_789_272_004),
            observation("2026-09-08T01:31:42.408Z", 0.0, 1_789_435_768),
            observation("2026-09-09T16:41:17.572Z", 63.0, 1_789_435_784),
            observation("2026-09-09T16:41:39.755Z", 71.0, 1_789_272_004),
            observation("2026-09-09T17:41:32.956Z", 63.0, 1_789_435_784),
            observation("2026-09-09T17:49:50.490Z", 63.0, 1_789_435_784),
        ];

        let estimates = estimate_weekly_quota_costs_from_timeline(
            &costs,
            &observations,
            parse_ts_timestamp("2026-09-09T18:00:00Z")
                .unwrap()
                .as_millis(),
        );

        assert_eq!(estimates.len(), 1);
        let estimate = &estimates[0];
        assert_eq!(estimate.started_at, "2026-09-08T01:31:42.408Z");
        assert_eq!(estimate.ended_at, "2026-09-09T16:41:17.572Z");
        assert_eq!(estimate.used_percent_start, 0.0);
        assert_eq!(estimate.used_percent_end, 63.0);
        assert!((estimate.observed_cost_usd - 2.0).abs() < f64::EPSILON);
        assert_eq!(estimate.sample_count, 2);
        assert_eq!(estimate.status, CodexWeeklyQuotaEstimateStatus::Provisional);
    }

    #[test]
    fn starts_a_new_episode_after_a_frozen_episode_reaches_a_real_reset() {
        let costs = costs(&[
            ("2026-09-08T01:31:42.408Z", 1.0),
            ("2026-09-09T16:41:17.572Z", 2.0),
            ("2026-09-09T16:41:39.755Z", 3.0),
            ("2026-09-09T17:41:32.956Z", 5.0),
            ("2026-09-15T01:30:00Z", 7.0),
            ("2026-09-16T01:30:00Z", 11.0),
        ]);
        let observations = [
            observation("2026-09-08T01:29:22.343Z", 71.0, 1_789_272_004),
            observation("2026-09-08T01:31:42.408Z", 0.0, 1_789_435_768),
            observation("2026-09-09T16:41:17.572Z", 63.0, 1_789_435_784),
            observation("2026-09-09T16:41:39.755Z", 71.0, 1_789_272_004),
            observation("2026-09-09T17:41:32.956Z", 63.0, 1_789_435_784),
            observation("2026-09-15T01:30:00Z", 0.0, 1_790_040_584),
            observation("2026-09-16T01:30:00Z", 25.0, 1_790_040_584),
        ];

        let estimates = estimate_weekly_quota_costs_from_timeline(
            &costs,
            &observations,
            parse_ts_timestamp("2026-09-17T02:00:00Z")
                .unwrap()
                .as_millis(),
        );

        assert_eq!(estimates.len(), 2);
        assert_eq!(estimates[0].ended_at, "2026-09-09T16:41:17.572Z");
        assert!((estimates[0].observed_cost_usd - 2.0).abs() < f64::EPSILON);
        assert_eq!(
            estimates[0].status,
            CodexWeeklyQuotaEstimateStatus::Completed
        );
        assert_eq!(estimates[1].started_at, "2026-09-15T01:30:00Z");
        assert_eq!(estimates[1].ended_at, "2026-09-16T01:30:00Z");
        assert!((estimates[1].observed_cost_usd - 11.0).abs() < f64::EPSILON);
        assert_eq!(
            estimates[1].status,
            CodexWeeklyQuotaEstimateStatus::Provisional
        );
    }

    #[test]
    fn starts_a_new_episode_when_a_zero_percent_episode_was_frozen() {
        let costs = costs(&[
            ("2026-09-08T01:31:42.408Z", 1.0),
            ("2026-09-09T16:41:39.755Z", 3.0),
            ("2026-09-15T01:30:00Z", 7.0),
            ("2026-09-16T01:30:00Z", 11.0),
        ]);
        let observations = [
            observation("2026-09-08T01:31:42.408Z", 0.0, 1_789_435_768),
            observation("2026-09-09T16:41:39.755Z", 71.0, 1_789_272_004),
            observation("2026-09-09T17:41:32.956Z", 0.0, 1_789_435_768),
            observation("2026-09-15T01:30:00Z", 0.0, 1_790_040_584),
            observation("2026-09-16T01:30:00Z", 25.0, 1_790_040_584),
        ];

        let estimates = estimate_weekly_quota_costs_from_timeline(
            &costs,
            &observations,
            parse_ts_timestamp("2026-09-17T02:00:00Z")
                .unwrap()
                .as_millis(),
        );

        assert_eq!(estimates.len(), 1);
        assert_eq!(estimates[0].started_at, "2026-09-15T01:30:00Z");
        assert_eq!(estimates[0].ended_at, "2026-09-16T01:30:00Z");
        assert!((estimates[0].observed_cost_usd - 11.0).abs() < f64::EPSILON);
        assert_eq!(
            estimates[0].status,
            CodexWeeklyQuotaEstimateStatus::Provisional
        );
    }

    #[test]
    fn ignores_a_late_previous_episode_sample_during_reset_handoff() {
        let costs = costs(&[
            ("2026-09-12T08:09:51.891Z", 1.0),
            ("2026-09-12T08:10:15.345Z", 2.0),
            ("2026-09-12T17:33:48.720Z", 3.0),
        ]);
        let observations = [
            observation("2026-09-11T08:00:21.647Z", 0.0, 1_789_718_422),
            observation("2026-09-12T08:09:51.891Z", 52.0, 1_789_718_422),
            observation("2026-09-12T08:09:53.151Z", 0.0, 1_789_805_387),
            observation("2026-09-12T08:10:15.345Z", 52.0, 1_789_718_422),
            observation("2026-09-12T17:33:48.720Z", 15.0, 1_789_805_392),
        ];

        let estimates = estimate_weekly_quota_costs_from_timeline(
            &costs,
            &observations,
            parse_ts_timestamp("2026-09-12T17:34:00Z")
                .unwrap()
                .as_millis(),
        );

        assert_eq!(estimates.len(), 2);
        assert_eq!(estimates[1].started_at, "2026-09-12T08:09:53.151Z");
        assert_eq!(estimates[1].ended_at, "2026-09-12T17:33:48.720Z");
        assert_eq!(estimates[1].used_percent_end, 15.0);
        assert_eq!(
            estimates[1].status,
            CodexWeeklyQuotaEstimateStatus::Provisional
        );
    }

    #[test]
    fn publishes_a_new_provisional_episode_once_usage_exceeds_five_percent() {
        let costs = costs(&[("2026-08-03T12:00:00Z", 1.0), ("2026-08-03T18:00:00Z", 1.0)]);
        let observations = [
            observation("2026-08-03T12:00:00Z", 0.0, 1_786_656_000),
            observation("2026-08-03T18:00:00Z", 30.0, 1_786_656_000),
        ];

        let estimates =
            estimate_weekly_quota_costs_from_timeline(&costs, &observations, 1_785_783_600_000);

        assert_eq!(estimates.len(), 1);
        assert!((estimates[0].observed_cost_usd - 1.0).abs() < f64::EPSILON);
        assert_eq!(
            estimates[0].status,
            CodexWeeklyQuotaEstimateStatus::Provisional
        );
    }

    #[test]
    fn requires_more_than_five_observed_percentage_points() {
        let costs = costs(&[("2026-08-03T00:00:00Z", 1.0), ("2026-08-04T00:00:00Z", 1.0)]);
        let five_percent = [
            observation("2026-08-03T00:00:00Z", 0.0, 1_786_656_000),
            observation("2026-08-04T00:00:00Z", 5.0, 1_786_656_000),
        ];
        let six_percent = [
            observation("2026-08-03T00:00:00Z", 0.0, 1_786_656_000),
            observation("2026-08-04T00:00:00Z", 6.0, 1_786_656_000),
        ];
        let now = parse_ts_timestamp("2026-08-05T01:00:00Z")
            .unwrap()
            .as_millis();

        assert!(estimate_weekly_quota_costs_from_timeline(&costs, &five_percent, now).is_empty());
        assert_eq!(
            estimate_weekly_quota_costs_from_timeline(&costs, &six_percent, now).len(),
            1
        );
    }

    #[test]
    fn ignores_conflicting_observations_at_the_same_timestamp() {
        let costs = costs(&[("2026-08-03T00:00:00Z", 1.0)]);
        let observations = [
            observation("2026-08-03T00:00:00Z", 0.0, 1_786_656_000),
            observation("2026-08-03T00:00:00Z", 11.0, 1_786_656_000),
        ];
        let now = parse_ts_timestamp("2026-08-05T01:00:00Z")
            .unwrap()
            .as_millis();

        assert!(estimate_weekly_quota_costs_from_timeline(&costs, &observations, now).is_empty());
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
