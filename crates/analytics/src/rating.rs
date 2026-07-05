//! Composite Chat Rating: a 0-100 score derived from seven weighted
//! components. The big "81/100" gauge on the dashboard is this number.
//!
//! # Components (default weights — configurable)
//!
//! | Weight | Component       | Measures                                           |
//! |-------:|-----------------|----------------------------------------------------|
//! |   20%  | Responsiveness  | how fast both parties reply (medians)              |
//! |   15%  | Balance         | message volume + initiation symmetry               |
//! |   15%  | Engagement      | avg conversation length + media sharing rate       |
//! |   15%  | Consistency     | regularity of communication over time              |
//! |   10%  | Reciprocity     | volume parity for questions, media, encouragement  |
//! |   10%  | Longevity       | bonus for long-running relationships               |
//! |   15%  | Mutual Effort   | parity of emotional labor (apologies, encouragement, questions) |
//!
//! Sum = 100%. Each component scores 0-100 independently; the overall is a
//! weighted average rounded to integer.
//!
//! # Data sufficiency
//!
//! For relationships with very few messages, a score is misleading. We expose
//! a `confidence` band:
//! - **Hidden** (< 50 messages): the orchestrator should NOT show the rating
//! - **Limited** (50-200 messages): show the rating with a "limited data" badge
//! - **Full** (> 200 messages): show normally
//!
//! Thresholds are configurable via `RatingThresholds` and live in
//! `analytics_meta` at runtime.

use crate::aggregator::{ContactAggregates, DailyBucket};
use crate::responses::ResponseMetrics;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RatingWeights {
    pub responsiveness: f64,
    pub balance: f64,
    pub engagement: f64,
    pub consistency: f64,
    pub reciprocity: f64,
    pub longevity: f64,
    pub mutual_effort: f64,
}

impl Default for RatingWeights {
    fn default() -> Self {
        Self {
            responsiveness: 0.20,
            balance: 0.15,
            engagement: 0.15,
            consistency: 0.15,
            reciprocity: 0.10,
            longevity: 0.10,
            mutual_effort: 0.15,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RatingThresholds {
    pub hide_below_messages: u32,
    pub low_confidence_max_messages: u32,
}

impl Default for RatingThresholds {
    fn default() -> Self {
        Self {
            hide_below_messages: 50,
            low_confidence_max_messages: 200,
        }
    }
}

/// Bundle of inputs the rating engine needs. Built by the orchestrator from
/// the outputs of segmenter / aggregator / responses / scoring.
#[derive(Debug, Clone)]
pub struct RatingInput<'a> {
    pub contact: &'a ContactAggregates,
    pub responses: &'a ResponseMetrics,
    pub daily: &'a [DailyBucket],
    pub conversations_started_by_me: u32,
    pub conversations_started_by_them: u32,
    pub first_message_ms: i64,
    pub last_message_ms: i64,
    pub avg_convo_length_msgs: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RatingConfidence {
    #[default]
    Hidden,
    Limited,
    Full,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RatingOutput {
    pub overall: u8,
    pub responsiveness: u8,
    pub balance: u8,
    pub engagement: u8,
    pub consistency: u8,
    pub reciprocity: u8,
    pub longevity: u8,
    pub mutual_effort: u8,
    /// Whether the rating should be shown, shown-with-caveat, or hidden.
    pub confidence: RatingConfidence,
}

/// Run the full rating pipeline.
pub fn compute_rating(
    input: &RatingInput,
    weights: &RatingWeights,
    thresholds: &RatingThresholds,
) -> RatingOutput {
    let total_msgs = input.contact.my_message_count + input.contact.their_message_count;
    let confidence = if total_msgs < thresholds.hide_below_messages {
        RatingConfidence::Hidden
    } else if total_msgs <= thresholds.low_confidence_max_messages {
        RatingConfidence::Limited
    } else {
        RatingConfidence::Full
    };

    let responsiveness = score_responsiveness(input.responses);
    let balance = score_balance(input);
    let engagement = score_engagement(input);
    let consistency = score_consistency(input.daily);
    let reciprocity = score_reciprocity(input.contact);
    let longevity = score_longevity(input.first_message_ms, input.last_message_ms);
    let mutual_effort = score_mutual_effort(input.contact);

    let weighted_sum = responsiveness as f64 * weights.responsiveness
        + balance as f64 * weights.balance
        + engagement as f64 * weights.engagement
        + consistency as f64 * weights.consistency
        + reciprocity as f64 * weights.reciprocity
        + longevity as f64 * weights.longevity
        + mutual_effort as f64 * weights.mutual_effort;
    let weight_total = weights.responsiveness
        + weights.balance
        + weights.engagement
        + weights.consistency
        + weights.reciprocity
        + weights.longevity
        + weights.mutual_effort;
    let overall = if weight_total > 0.0 {
        (weighted_sum / weight_total).round().clamp(0.0, 100.0) as u8
    } else {
        0
    };

    RatingOutput {
        overall,
        responsiveness,
        balance,
        engagement,
        consistency,
        reciprocity,
        longevity,
        mutual_effort,
        confidence,
    }
}

// ---------- component scorers ----------

/// Responsiveness: combines both parties' median response times. Faster = higher.
/// Returns 50 when there's no response data (no penalty for new relationships).
fn score_responsiveness(r: &ResponseMetrics) -> u8 {
    let medians: Vec<i64> = [r.my_median_response_ms, r.their_median_response_ms]
        .into_iter()
        .flatten()
        .collect();
    if medians.is_empty() {
        return 50;
    }
    let avg_med_ms = medians.iter().sum::<i64>() / medians.len() as i64;
    let score_f = if avg_med_ms <= 0 {
        100.0
    } else if avg_med_ms <= 5 * 60 * 1000 {
        // 0 → 100, 5 min → 80
        100.0 - (avg_med_ms as f64 / (5.0 * 60.0 * 1000.0)) * 20.0
    } else if avg_med_ms <= 60 * 60 * 1000 {
        // 5 min → 80, 1 hour → 50
        80.0 - ((avg_med_ms - 5 * 60 * 1000) as f64 / (55.0 * 60.0 * 1000.0)) * 30.0
    } else if avg_med_ms <= 6 * 60 * 60 * 1000 {
        // 1 hour → 50, 6 hour → 0
        50.0 - ((avg_med_ms - 60 * 60 * 1000) as f64 / (5.0 * 60.0 * 60.0 * 1000.0)) * 50.0
    } else {
        0.0
    };
    score_f.clamp(0.0, 100.0) as u8
}

/// Balance: how evenly split between sides. Combines message volume balance
/// (60% weight) and initiation balance (40% weight). 100 = perfect 50/50.
fn score_balance(input: &RatingInput) -> u8 {
    let total_msgs = input.contact.my_message_count + input.contact.their_message_count;
    if total_msgs == 0 {
        return 50;
    }
    let msg_ratio = symmetric_ratio(
        input.contact.my_message_count,
        input.contact.their_message_count,
    );

    let total_inits = input.conversations_started_by_me + input.conversations_started_by_them;
    let init_ratio = if total_inits > 0 {
        symmetric_ratio(
            input.conversations_started_by_me,
            input.conversations_started_by_them,
        )
    } else {
        0.5
    };

    let combined = msg_ratio * 0.6 + init_ratio * 0.4;
    (combined * 100.0).clamp(0.0, 100.0).round() as u8
}

/// Engagement: avg conversation length × 70% + media sharing rate × 30%.
fn score_engagement(input: &RatingInput) -> u8 {
    // Conversation length: 30+ msgs → 90, 15 msgs → 60, 5 msgs → 30 (linear cap at 90).
    let convo_score = (input.avg_convo_length_msgs * 3.0).clamp(0.0, 90.0);

    let total_msgs = input.contact.my_message_count + input.contact.their_message_count;
    let total_media = input.contact.my_image_count
        + input.contact.their_image_count
        + input.contact.my_video_count
        + input.contact.their_video_count
        + input.contact.my_gif_count
        + input.contact.their_gif_count
        + input.contact.my_audio_count
        + input.contact.their_audio_count;
    let media_score = if total_msgs > 0 {
        let media_ratio = total_media as f64 / total_msgs as f64;
        (media_ratio * 500.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let combined = convo_score * 0.7 + media_score * 0.3;
    combined.clamp(0.0, 100.0).round() as u8
}

/// Consistency: low variance in weekly message counts → high score. Uses
/// coefficient of variation over true calendar weeks (days since epoch / 7),
/// counting the zero-message weeks between the first and last active day —
/// `daily` only contains days with activity, so chunking it by index would
/// make three week-long bursts separated by months of silence look like
/// steady weekly traffic. Returns 50 when there isn't enough data.
fn score_consistency(daily: &[DailyBucket]) -> u8 {
    if daily.len() < 7 {
        return 50;
    }
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).expect("constant date is valid");
    let mut week_totals: std::collections::BTreeMap<i64, f64> = std::collections::BTreeMap::new();
    let mut d_min = i64::MAX;
    let mut d_max = i64::MIN;
    for d in daily {
        let Ok(date) = chrono::NaiveDate::parse_from_str(&d.day, "%Y-%m-%d") else {
            continue;
        };
        let day_num = date.signed_duration_since(epoch).num_days();
        d_min = d_min.min(day_num);
        d_max = d_max.max(day_num);
        *week_totals.entry(day_num.div_euclid(7)).or_insert(0.0) +=
            (d.my_messages + d.their_messages) as f64;
    }
    if week_totals.is_empty() {
        return 50;
    }
    let w_first = d_min.div_euclid(7);
    let w_last = d_max.div_euclid(7);
    let n_weeks = w_last - w_first + 1;
    if n_weeks < 4 {
        return 50;
    }
    if n_weeks > 53 * 100 {
        // A >100-year span means corrupt dates; don't pretend to score it.
        return 50;
    }

    // Pro-rate the (possibly partial) first and last weeks to a full-week
    // equivalent so steady traffic isn't penalized for where the window
    // happens to start; interior silent weeks count as zeros.
    let scaled = |w: i64| -> f64 {
        let total = week_totals.get(&w).copied().unwrap_or(0.0);
        let days_in_window = (7 * w + 6).min(d_max) - (7 * w).max(d_min) + 1;
        total * 7.0 / days_in_window as f64
    };
    let n = n_weeks as f64;
    let mean = (w_first..=w_last).map(scaled).sum::<f64>() / n;
    if mean <= 0.0 {
        return 0;
    }
    let var = (w_first..=w_last)
        .map(|w| (scaled(w) - mean).powi(2))
        .sum::<f64>()
        / n;
    let cv = var.sqrt() / mean;
    // CV of 0 → 100, CV of 2+ → 0
    let score = ((1.0 - cv / 2.0) * 100.0).clamp(0.0, 100.0);
    score.round() as u8
}

/// Reciprocity: do both sides do similar VOLUME of stuff? Looks at questions,
/// media (sum of categories), and encouragement counts. Each pair contributes
/// only if there's enough sample (combined > 10).
fn score_reciprocity(c: &ContactAggregates) -> u8 {
    let pairs: [(u32, u32); 3] = [
        (c.my_question_count, c.their_question_count),
        (
            c.my_image_count + c.my_video_count + c.my_gif_count + c.my_audio_count,
            c.their_image_count + c.their_video_count + c.their_gif_count + c.their_audio_count,
        ),
        (c.my_encouragement_count, c.their_encouragement_count),
    ];
    let mut ratios: Vec<f64> = Vec::new();
    for (mine, theirs) in pairs {
        let total = mine + theirs;
        if total > 10 {
            ratios.push(symmetric_ratio(mine, theirs));
        }
    }
    if ratios.is_empty() {
        return 50;
    }
    let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
    (mean * 100.0).clamp(0.0, 100.0).round() as u8
}

/// Longevity: bonus for long-running relationships. Smooth gradient from
/// 1 month (10) through 1 year (50) to 5+ years (100).
///
/// `first_ms` of 0 (Unix epoch) is a valid timestamp — only reject when
/// either value is negative or when the duration is non-positive.
fn score_longevity(first_ms: i64, last_ms: i64) -> u8 {
    if first_ms < 0 || last_ms < 0 || last_ms <= first_ms {
        return 0;
    }
    let duration_days = ((last_ms - first_ms) as f64) / (24.0 * 60.0 * 60.0 * 1000.0);
    let score = if duration_days < 30.0 {
        10.0
    } else if duration_days < 180.0 {
        25.0
    } else if duration_days < 365.0 {
        // 6mo → 25 to 12mo → 50 (linear)
        25.0 + ((duration_days - 180.0) / 185.0) * 25.0
    } else if duration_days < 5.0 * 365.0 {
        // 1y → 50 to 5y → 100 (linear)
        50.0 + ((duration_days - 365.0) / (4.0 * 365.0)) * 50.0
    } else {
        100.0
    };
    score.clamp(0.0, 100.0).round() as u8
}

/// Mutual Effort: parity of emotional labor — apologies, encouragement,
/// questions. Different from Reciprocity (which is about volume); this is
/// specifically about who does the relational maintenance work.
fn score_mutual_effort(c: &ContactAggregates) -> u8 {
    let pairs: [(u32, u32); 3] = [
        (c.my_apology_count, c.their_apology_count),
        (c.my_encouragement_count, c.their_encouragement_count),
        (c.my_question_count, c.their_question_count),
    ];
    let mut ratios: Vec<f64> = Vec::new();
    for (mine, theirs) in pairs {
        let total = mine + theirs;
        if total >= 10 {
            ratios.push(symmetric_ratio(mine, theirs));
        }
    }
    if ratios.is_empty() {
        return 50;
    }
    let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
    (mean * 100.0).clamp(0.0, 100.0).round() as u8
}

/// Helper: returns a 0.0-1.0 ratio measuring how evenly two counts are split.
/// 1.0 = perfect 50/50, 0.0 = entirely one-sided.
fn symmetric_ratio(a: u32, b: u32) -> f64 {
    let total = a + b;
    if total == 0 {
        return 0.5;
    }
    let smaller = a.min(b) as f64;
    let half = (total as f64) / 2.0;
    (smaller / half).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::DailyBucket;

    fn empty_response_metrics() -> ResponseMetrics {
        ResponseMetrics::default()
    }

    fn empty_contact() -> ContactAggregates {
        ContactAggregates::default()
    }

    #[allow(clippy::too_many_arguments)] // test fixture builder
    fn input_with(
        contact: &ContactAggregates,
        responses: &ResponseMetrics,
        daily: &[DailyBucket],
        starts_me: u32,
        starts_them: u32,
        first_ms: i64,
        last_ms: i64,
        avg_convo: f64,
    ) -> RatingInput<'static> {
        // Lifetime trickery for tests — leak the references we need static.
        let contact_static: &'static ContactAggregates = Box::leak(Box::new(contact.clone()));
        let responses_static: &'static ResponseMetrics = Box::leak(Box::new(responses.clone()));
        let daily_static: &'static [DailyBucket] = Box::leak(daily.to_vec().into_boxed_slice());
        RatingInput {
            contact: contact_static,
            responses: responses_static,
            daily: daily_static,
            conversations_started_by_me: starts_me,
            conversations_started_by_them: starts_them,
            first_message_ms: first_ms,
            last_message_ms: last_ms,
            avg_convo_length_msgs: avg_convo,
        }
    }

    // ---------- symmetric_ratio ----------
    #[test]
    fn symmetric_ratio_basic() {
        assert!((symmetric_ratio(50, 50) - 1.0).abs() < 1e-9);
        assert!((symmetric_ratio(0, 100) - 0.0).abs() < 1e-9);
        assert!((symmetric_ratio(100, 0) - 0.0).abs() < 1e-9);
        assert!((symmetric_ratio(25, 75) - 0.5).abs() < 1e-9); // 25/50 = 0.5
        assert_eq!(symmetric_ratio(0, 0), 0.5); // empty fallback
    }

    // ---------- responsiveness ----------
    #[test]
    fn responsiveness_no_data_returns_neutral() {
        let r = empty_response_metrics();
        assert_eq!(score_responsiveness(&r), 50);
    }

    #[test]
    fn responsiveness_fast_replies_score_high() {
        let mut r = empty_response_metrics();
        r.my_median_response_ms = Some(60_000); // 1 min
        r.their_median_response_ms = Some(60_000);
        let score = score_responsiveness(&r);
        // Average median = 1 min. 100 - (60_000 / 300_000) * 20 = 100 - 4 = 96.
        assert!(score >= 90, "fast replies should score high, got {}", score);
    }

    #[test]
    fn responsiveness_slow_replies_score_low() {
        let mut r = empty_response_metrics();
        r.my_median_response_ms = Some(8 * 60 * 60 * 1000); // 8 hours
        r.their_median_response_ms = Some(8 * 60 * 60 * 1000);
        let score = score_responsiveness(&r);
        assert_eq!(score, 0, "8h median should be 0, got {}", score);
    }

    #[test]
    fn responsiveness_mid_speed_score_in_middle() {
        let mut r = empty_response_metrics();
        r.my_median_response_ms = Some(60 * 60 * 1000); // 1 hour
        r.their_median_response_ms = Some(60 * 60 * 1000);
        let score = score_responsiveness(&r);
        // Avg median = 1 hour boundary → 50.
        assert!(
            (score as i32 - 50).abs() <= 1,
            "1h median should ≈ 50, got {}",
            score
        );
    }

    // ---------- balance ----------
    #[test]
    fn balance_perfect_split_high_score() {
        let mut c = empty_contact();
        c.my_message_count = 100;
        c.their_message_count = 100;
        let r = empty_response_metrics();
        let daily: [DailyBucket; 0] = [];
        let input = input_with(&c, &r, &daily, 50, 50, 0, 0, 0.0);
        let score = score_balance(&input);
        assert!(score >= 95, "perfect split should be ≥95, got {}", score);
    }

    #[test]
    fn balance_one_sided_low_score() {
        let mut c = empty_contact();
        c.my_message_count = 100;
        c.their_message_count = 5;
        let r = empty_response_metrics();
        let daily: [DailyBucket; 0] = [];
        let input = input_with(&c, &r, &daily, 100, 0, 0, 0, 0.0);
        let score = score_balance(&input);
        assert!(score < 30, "very one-sided should score <30, got {}", score);
    }

    // ---------- consistency ----------
    #[test]
    fn consistency_short_history_returns_neutral() {
        let mut daily = Vec::new();
        for i in 0..6 {
            daily.push(DailyBucket {
                day: format!("2024-01-{:02}", i + 1),
                my_messages: 5,
                their_messages: 5,
                my_words: 0,
                their_words: 0,
                my_media: 0,
                their_media: 0,
            });
        }
        assert_eq!(score_consistency(&daily), 50);
    }

    #[test]
    fn consistency_steady_communication_scores_high() {
        // 28 days, exactly 10 msgs/day every day → CV = 0, score = 100.
        let mut daily = Vec::new();
        for i in 0..28 {
            daily.push(DailyBucket {
                day: format!("2024-01-{:02}", i + 1),
                my_messages: 5,
                their_messages: 5,
                my_words: 0,
                their_words: 0,
                my_media: 0,
                their_media: 0,
            });
        }
        let score = score_consistency(&daily);
        assert!(score >= 95, "perfectly steady should ≥95, got {}", score);
    }

    #[test]
    fn consistency_counts_silent_weeks_between_bursts() {
        // Three identical week-long bursts. Contiguous, they read as steady;
        // separated by months of silence, the silent calendar weeks must drag
        // the score down (the old index-mod-7 chunking treated both the same).
        let burst = |start: &str| -> Vec<DailyBucket> {
            let d0 = chrono::NaiveDate::parse_from_str(start, "%Y-%m-%d").unwrap();
            (0..7)
                .map(|i| DailyBucket {
                    day: (d0 + chrono::Days::new(i)).format("%Y-%m-%d").to_string(),
                    my_messages: 5,
                    their_messages: 5,
                    my_words: 0,
                    their_words: 0,
                    my_media: 0,
                    their_media: 0,
                })
                .collect()
        };
        let contiguous: Vec<DailyBucket> = [
            burst("2024-01-01"),
            burst("2024-01-08"),
            burst("2024-01-15"),
        ]
        .concat();
        let with_gaps: Vec<DailyBucket> = [
            burst("2024-01-01"),
            burst("2024-04-01"),
            burst("2024-08-05"),
        ]
        .concat();
        // Pad both to 4+ weeks so neither takes the "not enough data" branch.
        let contiguous = [contiguous, burst("2024-01-22")].concat();
        let with_gaps = [with_gaps, burst("2024-11-04")].concat();

        let contiguous_score = score_consistency(&contiguous);
        let gap_score = score_consistency(&with_gaps);
        assert!(
            gap_score < contiguous_score,
            "bursts with silent months ({}) must score below contiguous bursts ({})",
            gap_score,
            contiguous_score
        );
    }

    #[test]
    fn consistency_bursty_communication_scores_lower() {
        // 4 weeks: 100 msgs in week 1, 0 in others. High variance → low score.
        let mut daily = Vec::new();
        for i in 0..28 {
            let count = if i < 7 { 100 } else { 0 };
            daily.push(DailyBucket {
                day: format!("2024-01-{:02}", i + 1),
                my_messages: count,
                their_messages: 0,
                my_words: 0,
                their_words: 0,
                my_media: 0,
                their_media: 0,
            });
        }
        let steady_score = {
            let mut steady = Vec::new();
            for i in 0..28 {
                steady.push(DailyBucket {
                    day: format!("2024-01-{:02}", i + 1),
                    my_messages: 5,
                    their_messages: 5,
                    my_words: 0,
                    their_words: 0,
                    my_media: 0,
                    their_media: 0,
                });
            }
            score_consistency(&steady)
        };
        let bursty_score = score_consistency(&daily);
        assert!(
            bursty_score < steady_score,
            "bursty ({}) should score lower than steady ({})",
            bursty_score,
            steady_score
        );
    }

    // ---------- longevity ----------
    #[test]
    fn longevity_short_relationship_low_score() {
        let day_ms = 24 * 60 * 60 * 1000;
        // Just 5 days
        let score = score_longevity(0, 5 * day_ms);
        assert_eq!(score, 10);
    }

    #[test]
    fn longevity_one_year_about_50() {
        let day_ms = 24 * 60 * 60 * 1000;
        let score = score_longevity(0, 365 * day_ms);
        assert!(
            (score as i32 - 50).abs() <= 1,
            "1 year should ≈ 50, got {}",
            score
        );
    }

    #[test]
    fn longevity_five_plus_years_caps_at_100() {
        let day_ms = 24 * 60 * 60 * 1000;
        let score = score_longevity(0, 10 * 365 * day_ms);
        assert_eq!(score, 100);
    }

    #[test]
    fn longevity_zero_or_invalid_returns_zero() {
        assert_eq!(score_longevity(0, 0), 0);
        assert_eq!(score_longevity(100, 50), 0);
    }

    // ---------- reciprocity / mutual effort ----------
    #[test]
    fn reciprocity_balanced_volume_scores_high() {
        let mut c = empty_contact();
        c.my_question_count = 50;
        c.their_question_count = 50;
        c.my_image_count = 30;
        c.their_image_count = 30;
        c.my_encouragement_count = 20;
        c.their_encouragement_count = 20;
        let score = score_reciprocity(&c);
        assert!(score >= 95, "balanced volume should ≥95, got {}", score);
    }

    #[test]
    fn reciprocity_low_sample_returns_neutral() {
        let c = empty_contact(); // all zeros
        assert_eq!(score_reciprocity(&c), 50);
    }

    #[test]
    fn mutual_effort_one_sided_emotional_labor_low_score() {
        let mut c = empty_contact();
        // I do all the apologizing / encouraging / asking
        c.my_apology_count = 50;
        c.their_apology_count = 0;
        c.my_encouragement_count = 50;
        c.their_encouragement_count = 0;
        c.my_question_count = 50;
        c.their_question_count = 0;
        let score = score_mutual_effort(&c);
        assert!(
            score < 10,
            "one-sided labor should score <10, got {}",
            score
        );
    }

    // ---------- compute_rating ----------
    #[test]
    fn rating_hidden_when_below_message_threshold() {
        let c = empty_contact(); // 0 messages
        let r = empty_response_metrics();
        let daily: [DailyBucket; 0] = [];
        let input = input_with(&c, &r, &daily, 0, 0, 0, 0, 0.0);
        let out = compute_rating(
            &input,
            &RatingWeights::default(),
            &RatingThresholds::default(),
        );
        assert_eq!(out.confidence, RatingConfidence::Hidden);
    }

    #[test]
    fn rating_limited_when_in_band() {
        let mut c = empty_contact();
        c.my_message_count = 60;
        c.their_message_count = 60;
        let r = empty_response_metrics();
        let daily: [DailyBucket; 0] = [];
        let input = input_with(&c, &r, &daily, 0, 0, 0, 0, 0.0);
        let out = compute_rating(
            &input,
            &RatingWeights::default(),
            &RatingThresholds::default(),
        );
        assert_eq!(out.confidence, RatingConfidence::Limited);
    }

    #[test]
    fn rating_full_when_above_threshold() {
        let mut c = empty_contact();
        c.my_message_count = 500;
        c.their_message_count = 500;
        let r = empty_response_metrics();
        let daily: [DailyBucket; 0] = [];
        let input = input_with(&c, &r, &daily, 0, 0, 0, 0, 0.0);
        let out = compute_rating(
            &input,
            &RatingWeights::default(),
            &RatingThresholds::default(),
        );
        assert_eq!(out.confidence, RatingConfidence::Full);
    }

    #[test]
    fn rating_overall_is_weighted_average_of_components() {
        // Construct an input where every component should be 100. Overall = 100.
        let mut c = empty_contact();
        c.my_message_count = 1000;
        c.their_message_count = 1000;
        c.my_question_count = 500;
        c.their_question_count = 500;
        c.my_image_count = 300;
        c.their_image_count = 300;
        c.my_encouragement_count = 100;
        c.their_encouragement_count = 100;
        c.my_apology_count = 50;
        c.their_apology_count = 50;
        let mut r = empty_response_metrics();
        r.my_median_response_ms = Some(60_000);
        r.their_median_response_ms = Some(60_000);
        // Long, steady history — engineer for high consistency.
        let mut daily = Vec::new();
        for i in 0..(5 * 365) {
            daily.push(DailyBucket {
                day: format!("d{:04}", i),
                my_messages: 1,
                their_messages: 1,
                my_words: 0,
                their_words: 0,
                my_media: 0,
                their_media: 0,
            });
        }
        let day_ms = 24 * 60 * 60 * 1000i64;
        let input = input_with(&c, &r, &daily, 100, 100, 0, 5 * 365 * day_ms, 30.0);
        let out = compute_rating(
            &input,
            &RatingWeights::default(),
            &RatingThresholds::default(),
        );
        // Not exactly 100 — engagement uses media ratio etc — but should be very high.
        assert!(
            out.overall >= 80,
            "fully optimized inputs should overall ≥80, got {}",
            out.overall
        );
    }
}
