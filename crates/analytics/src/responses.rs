//! Response-time math: per-side median/mean, first-response, awake-vs-overnight
//! split, rapid-response percentage, and a log-scale histogram.
//!
//! # What counts as a "response pair"
//!
//! A response pair is two consecutive messages within the same conversation
//! where the senders are different. The time between them is the response
//! time, attributed to whoever sent the *second* message (the responder).
//!
//! Messages from the same sender in a row do NOT form response pairs — those
//! are "double messages" and are counted separately.
//!
//! # First response vs subsequent response
//!
//! Inside a conversation, the *first* time the OTHER side replies after the
//! opener is the "first response" for the responder. Every subsequent flip
//! is just an ordinary response. Mimoto's "Avg 1st Response" panel uses this
//! definition asymmetrically: it shows how long *I* take to reply to a convo
//! they started, vs how long *they* take to reply to a convo I started.
//!
//! # Overnight tagging
//!
//! A response pair is "overnight" if it was likely separated by sleep:
//! - The gap is ≥ 8 hours (regardless of clock time), OR
//! - The gap is ≥ 4 hours AND the prev message was sent during the user's
//!   configured overnight window (default 23:00-07:00 local).
//!
//! Otherwise it's "awake." This split is shown side-by-side on the dashboard
//! so users can see "you reply to her in 2 minutes during the day, 8 hours
//! overnight" without the overnight times polluting the awake median.
//!
//! # Histogram
//!
//! 8 log-spaced buckets covering 0-30s / 30s-1m / 1m-5m / 5m-15m / 15m-1h /
//! 1h-6h / 6h-24h / 24h+. Cheap (`O(1)` per pair) and unlocks a sparkline
//! distribution chart on the dashboard.

use crate::types::{Participant, SegmentationConfig};
use chrono::{FixedOffset, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

/// Slim per-message struct — just the fields the response calculator needs.
/// The orchestrator derives this from the heavier `AggregatorMessage` for
/// free (both fields are `Copy`).
#[derive(Debug, Clone, Copy)]
pub struct ResponseMessage {
    pub timestamp_ms: i64,
    pub sender: Participant,
}

/// Configuration for response classification. All fields can be overridden
/// per-contact via `analytics_overrides`; defaults match the seeded values
/// in `analytics_meta`.
#[derive(Debug, Clone, Copy)]
pub struct ResponseConfig {
    /// A response under this threshold counts toward `rapid_response_pct`.
    pub rapid_response_threshold_ms: i64,
    /// Local hour at which "overnight" begins (inclusive). Default 23 (11pm).
    pub overnight_start_hour: u8,
    /// Local hour at which "overnight" ends (exclusive). Default 7 (7am).
    pub overnight_end_hour: u8,
    /// Gap above which a response is automatically tagged overnight regardless
    /// of clock time. Default 8 hours.
    pub overnight_force_gap_ms: i64,
    /// Gap above which a response is overnight if prev message was sent during
    /// the overnight window. Default 4 hours.
    pub overnight_evening_gap_ms: i64,
}

impl Default for ResponseConfig {
    fn default() -> Self {
        Self {
            rapid_response_threshold_ms: 60 * 1000, // 60 seconds
            overnight_start_hour: 23,
            overnight_end_hour: 7,
            overnight_force_gap_ms: 8 * 60 * 60 * 1000, // 8 hours
            overnight_evening_gap_ms: 4 * 60 * 60 * 1000, // 4 hours
        }
    }
}

/// Output of `compute_response_metrics`. Field names map directly to columns
/// in `pair_analytics`.
#[derive(Debug, Clone, Default)]
pub struct ResponseMetrics {
    // Within-conversation responses (ALL response pairs)
    pub my_median_response_ms: Option<i64>,
    pub their_median_response_ms: Option<i64>,
    pub my_mean_response_ms: Option<i64>,
    pub their_mean_response_ms: Option<i64>,

    // Rapid response percentages (0.0-1.0)
    pub my_rapid_response_pct: Option<f64>,
    pub their_rapid_response_pct: Option<f64>,

    // First responses (only when the OTHER side opened the conversation)
    pub my_median_first_response_ms: Option<i64>,
    pub their_median_first_response_ms: Option<i64>,
    pub my_mean_first_response_ms: Option<i64>,
    pub their_mean_first_response_ms: Option<i64>,

    // Awake vs overnight split (medians)
    pub my_median_response_awake_ms: Option<i64>,
    pub their_median_response_awake_ms: Option<i64>,
    pub my_median_response_overnight_ms: Option<i64>,
    pub their_median_response_overnight_ms: Option<i64>,

    // Log-scale histogram (8 buckets per side)
    pub my_histogram: [u32; 8],
    pub their_histogram: [u32; 8],

    // Double messages (consecutive same-sender, NOT counted as responses)
    pub my_double_messages: u32,
    pub their_double_messages: u32,
}

/// Histogram bucket boundaries in milliseconds. A response_ms `r` lands in
/// bucket `i` where `BOUNDS[i-1] <= r < BOUNDS[i]` (with `BOUNDS[-1] = 0`).
/// 8 buckets total: [0, 30s), [30s, 1m), [1m, 5m), [5m, 15m), [15m, 1h),
/// [1h, 6h), [6h, 24h), [24h, +∞).
const HIST_BOUNDS_MS: [i64; 8] = [
    30_000,     // 30 seconds
    60_000,     // 1 minute
    300_000,    // 5 minutes
    900_000,    // 15 minutes
    3_600_000,  // 1 hour
    21_600_000, // 6 hours
    86_400_000, // 24 hours
    i64::MAX,
];

/// Bucket labels (for documentation / future debugging UIs). Index-aligned
/// with `HIST_BOUNDS_MS`.
pub const HIST_BUCKET_LABELS: [&str; 8] =
    ["<30s", "<1m", "<5m", "<15m", "<1h", "<6h", "<24h", "≥24h"];

/// Run the response calculator over a contact's full message stream.
///
/// `messages` MUST be pre-sorted by `(timestamp_ms ASC, db_rowid ASC)`. The
/// caller (orchestrator) guarantees this through SQL `ORDER BY`.
///
/// `seg_config` is reused from the segmenter for conversation-boundary
/// detection — keeps "what counts as one convo" consistent across modules.
///
/// `tz_offset_secs` is the user's local UTC offset, used for overnight-window
/// classification.
pub fn compute_response_metrics(
    messages: &[ResponseMessage],
    config: &ResponseConfig,
    seg_config: &SegmentationConfig,
    tz_offset_secs: i32,
) -> ResponseMetrics {
    if messages.is_empty() {
        return ResponseMetrics::default();
    }

    let tz = FixedOffset::east_opt(tz_offset_secs)
        .unwrap_or_else(|| FixedOffset::east_opt(0).expect("UTC always valid"));

    // Per-side accumulators.
    let mut me_responses_ms: Vec<i64> = Vec::new();
    let mut them_responses_ms: Vec<i64> = Vec::new();
    let mut me_first_responses_ms: Vec<i64> = Vec::new();
    let mut them_first_responses_ms: Vec<i64> = Vec::new();
    let mut me_awake_ms: Vec<i64> = Vec::new();
    let mut me_overnight_ms: Vec<i64> = Vec::new();
    let mut them_awake_ms: Vec<i64> = Vec::new();
    let mut them_overnight_ms: Vec<i64> = Vec::new();
    let mut me_rapid_count: u32 = 0;
    let mut them_rapid_count: u32 = 0;
    let mut me_histogram = [0u32; 8];
    let mut their_histogram = [0u32; 8];
    let mut me_doubles: u32 = 0;
    let mut them_doubles: u32 = 0;

    // Per-conversation tracking. Reset on every conversation boundary.
    let mut convo_started_by: Option<Participant> = None;
    let mut me_first_response_seen_in_convo = false;
    let mut them_first_response_seen_in_convo = false;

    let mut prev: Option<ResponseMessage> = None;

    for msg in messages.iter().copied() {
        let new_convo = match prev {
            Some(p) => msg.timestamp_ms - p.timestamp_ms > seg_config.conversation_timeout_ms,
            None => true,
        };

        if new_convo {
            convo_started_by = Some(msg.sender);
            me_first_response_seen_in_convo = false;
            them_first_response_seen_in_convo = false;
            prev = Some(msg);
            continue;
        }

        // Safe: new_convo handles the prev=None case via early continue above.
        let p = prev.expect("non-first iteration must have prev set");

        if msg.sender == p.sender {
            // Double message — same sender twice in a row. NOT a response.
            match msg.sender {
                Participant::Me => me_doubles += 1,
                Participant::Them => them_doubles += 1,
            }
        } else {
            // Sender flipped. This is a response pair.
            let response_ms = msg.timestamp_ms - p.timestamp_ms;
            let bucket = histogram_bucket(response_ms);
            let is_rapid = response_ms <= config.rapid_response_threshold_ms;
            let is_overnight = is_overnight_response(p.timestamp_ms, response_ms, &tz, config);

            match msg.sender {
                Participant::Me => {
                    me_responses_ms.push(response_ms);
                    me_histogram[bucket] += 1;
                    if is_rapid {
                        me_rapid_count += 1;
                    }
                    if is_overnight {
                        me_overnight_ms.push(response_ms);
                    } else {
                        me_awake_ms.push(response_ms);
                    }
                }
                Participant::Them => {
                    them_responses_ms.push(response_ms);
                    their_histogram[bucket] += 1;
                    if is_rapid {
                        them_rapid_count += 1;
                    }
                    if is_overnight {
                        them_overnight_ms.push(response_ms);
                    } else {
                        them_awake_ms.push(response_ms);
                    }
                }
            }

            // First-response logic: only fires when the OTHER side started this
            // conversation, and only the FIRST time the responder flips back.
            if convo_started_by != Some(msg.sender) {
                let already_seen = match msg.sender {
                    Participant::Me => me_first_response_seen_in_convo,
                    Participant::Them => them_first_response_seen_in_convo,
                };
                if !already_seen {
                    match msg.sender {
                        Participant::Me => {
                            me_first_responses_ms.push(response_ms);
                            me_first_response_seen_in_convo = true;
                        }
                        Participant::Them => {
                            them_first_responses_ms.push(response_ms);
                            them_first_response_seen_in_convo = true;
                        }
                    }
                }
            }
        }

        prev = Some(msg);
    }

    ResponseMetrics {
        my_median_response_ms: median(&me_responses_ms),
        their_median_response_ms: median(&them_responses_ms),
        my_mean_response_ms: mean(&me_responses_ms),
        their_mean_response_ms: mean(&them_responses_ms),

        my_rapid_response_pct: pct(me_rapid_count, me_responses_ms.len()),
        their_rapid_response_pct: pct(them_rapid_count, them_responses_ms.len()),

        my_median_first_response_ms: median(&me_first_responses_ms),
        their_median_first_response_ms: median(&them_first_responses_ms),
        my_mean_first_response_ms: mean(&me_first_responses_ms),
        their_mean_first_response_ms: mean(&them_first_responses_ms),

        my_median_response_awake_ms: median(&me_awake_ms),
        their_median_response_awake_ms: median(&them_awake_ms),
        my_median_response_overnight_ms: median(&me_overnight_ms),
        their_median_response_overnight_ms: median(&them_overnight_ms),

        my_histogram: me_histogram,
        their_histogram,

        my_double_messages: me_doubles,
        their_double_messages: them_doubles,
    }
}

/// Classify a response pair as overnight per [`ResponseConfig`] heuristics.
///
/// Two firing conditions:
/// 1. Gap >= `overnight_force_gap_ms` (default 8h): always overnight.
/// 2. Gap >= `overnight_evening_gap_ms` (default 4h) AND prev message hour was
///    in the configured overnight window: probably overnight.
fn is_overnight_response(
    prev_timestamp_ms: i64,
    gap_ms: i64,
    tz: &FixedOffset,
    config: &ResponseConfig,
) -> bool {
    if gap_ms >= config.overnight_force_gap_ms {
        return true;
    }
    if gap_ms < config.overnight_evening_gap_ms {
        return false;
    }
    let prev_hour = match tz.timestamp_millis_opt(prev_timestamp_ms) {
        chrono::LocalResult::Single(dt) => dt.hour() as u8,
        _ => return false,
    };
    hour_in_overnight_window(
        prev_hour,
        config.overnight_start_hour,
        config.overnight_end_hour,
    )
}

/// Whether a clock hour falls inside `[start, end)` taken cyclically over
/// 24 hours. Handles the common case where the window wraps midnight (e.g.
/// 23 → 7).
fn hour_in_overnight_window(hour: u8, start: u8, end: u8) -> bool {
    if start <= end {
        // Same-day window (start=22, end=23 → only 22 inclusive)
        hour >= start && hour < end
    } else {
        // Wraps midnight (start=23, end=7 → [23, 24) ∪ [0, 7))
        hour >= start || hour < end
    }
}

/// Map a response time in ms to its histogram bucket index 0-7.
fn histogram_bucket(response_ms: i64) -> usize {
    for (i, &boundary) in HIST_BOUNDS_MS.iter().enumerate() {
        if response_ms < boundary {
            return i;
        }
    }
    HIST_BOUNDS_MS.len() - 1
}

/// Standard median with averaging for even-sized inputs.
fn median(values: &[i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<i64> = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        Some(sorted[n / 2])
    } else {
        // Average of the two middles, rounded toward zero (i64 division).
        Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2)
    }
}

/// Arithmetic mean as i64 (rounded toward zero).
fn mean(values: &[i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    // Use i128 accumulation to avoid overflow on millions of large values.
    let sum: i128 = values.iter().map(|&v| v as i128).sum();
    Some((sum / values.len() as i128) as i64)
}

/// Convert a count + total to a 0.0-1.0 percentage. Returns None if total is 0.
fn pct(count: u32, total: usize) -> Option<f64> {
    if total == 0 {
        None
    } else {
        Some(count as f64 / total as f64)
    }
}

// =========================================================================
// Type alias: ResponseHistogram for ease of use in serialized form.
// =========================================================================

/// Serializable response-time histogram, ready to drop into
/// `pair_analytics.response_histogram_json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseHistogramJson {
    pub my: [u32; 8],
    pub their: [u32; 8],
}

impl ResponseHistogramJson {
    pub fn from_metrics(m: &ResponseMetrics) -> Self {
        Self {
            my: m.my_histogram,
            their: m.their_histogram,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(ts_ms: i64, sender: Participant) -> ResponseMessage {
        ResponseMessage {
            timestamp_ms: ts_ms,
            sender,
        }
    }

    fn cfg() -> ResponseConfig {
        ResponseConfig::default()
    }

    fn seg() -> SegmentationConfig {
        SegmentationConfig::default()
    }

    // ---------- bucket helper ----------
    #[test]
    fn histogram_bucket_boundaries() {
        assert_eq!(histogram_bucket(0), 0);
        assert_eq!(histogram_bucket(29_999), 0);
        assert_eq!(histogram_bucket(30_000), 1);
        assert_eq!(histogram_bucket(59_999), 1);
        assert_eq!(histogram_bucket(60_000), 2);
        assert_eq!(histogram_bucket(299_999), 2);
        assert_eq!(histogram_bucket(300_000), 3);
        assert_eq!(histogram_bucket(86_399_999), 6);
        assert_eq!(histogram_bucket(86_400_000), 7);
        assert_eq!(histogram_bucket(i64::MAX), 7);
    }

    // ---------- median / mean ----------
    #[test]
    fn median_handles_even_and_odd_lengths() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[5]), Some(5));
        assert_eq!(median(&[5, 10]), Some(7)); // (5+10)/2 = 7 (integer div)
        assert_eq!(median(&[5, 10, 100]), Some(10));
        assert_eq!(median(&[100, 5, 10]), Some(10)); // sorts internally
        assert_eq!(median(&[1, 2, 3, 4]), Some(2)); // (2+3)/2 = 2 (integer div)
    }

    #[test]
    fn mean_handles_simple_cases() {
        assert_eq!(mean(&[]), None);
        assert_eq!(mean(&[10]), Some(10));
        assert_eq!(mean(&[5, 10, 15]), Some(10));
        assert_eq!(mean(&[1, 2, 3, 4, 5]), Some(3));
    }

    // ---------- overnight window logic ----------
    #[test]
    fn overnight_window_wrap_midnight() {
        // start=23, end=7 — wraps midnight.
        assert!(hour_in_overnight_window(23, 23, 7));
        assert!(hour_in_overnight_window(0, 23, 7));
        assert!(hour_in_overnight_window(3, 23, 7));
        assert!(hour_in_overnight_window(6, 23, 7));
        assert!(!hour_in_overnight_window(7, 23, 7)); // end is exclusive
        assert!(!hour_in_overnight_window(12, 23, 7));
        assert!(!hour_in_overnight_window(22, 23, 7));
    }

    #[test]
    fn overnight_window_same_day() {
        // start=12, end=14 — same day, no wrap.
        assert!(!hour_in_overnight_window(11, 12, 14));
        assert!(hour_in_overnight_window(12, 12, 14));
        assert!(hour_in_overnight_window(13, 12, 14));
        assert!(!hour_in_overnight_window(14, 12, 14));
    }

    // ---------- empty / minimal inputs ----------
    #[test]
    fn empty_input_yields_default_metrics() {
        let metrics = compute_response_metrics(&[], &cfg(), &seg(), 0);
        assert!(metrics.my_median_response_ms.is_none());
        assert!(metrics.their_median_response_ms.is_none());
        assert_eq!(metrics.my_double_messages, 0);
        assert_eq!(metrics.their_double_messages, 0);
    }

    #[test]
    fn single_message_yields_no_response_pairs() {
        let messages = vec![r(1_000, Participant::Me)];
        let metrics = compute_response_metrics(&messages, &cfg(), &seg(), 0);
        assert!(metrics.my_median_response_ms.is_none());
        assert_eq!(metrics.my_double_messages, 0);
    }

    // ---------- response pairs ----------
    #[test]
    fn basic_back_and_forth_one_response_each_side() {
        let messages = vec![
            r(0, Participant::Them),       // they open
            r(60_000, Participant::Me),    // I reply (60s)
            r(120_000, Participant::Them), // they reply (60s)
        ];
        let metrics = compute_response_metrics(&messages, &cfg(), &seg(), 0);
        assert_eq!(metrics.my_median_response_ms, Some(60_000));
        assert_eq!(metrics.their_median_response_ms, Some(60_000));
        // Their open + my reply → my first response (they started)
        assert_eq!(metrics.my_median_first_response_ms, Some(60_000));
        // I never started, so they had no opportunity for first response
        assert!(metrics.their_median_first_response_ms.is_none());
    }

    #[test]
    fn double_messages_excluded_from_response_pairs() {
        let messages = vec![
            r(0, Participant::Me),        // I open
            r(30_000, Participant::Me),   // I send another (double)
            r(60_000, Participant::Me),   // I send another (double)
            r(90_000, Participant::Them), // they reply (90s from my last)
        ];
        let metrics = compute_response_metrics(&messages, &cfg(), &seg(), 0);
        // Two doubles from me (msg 2 and 3 are after msg 1 and 2 with same sender).
        assert_eq!(metrics.my_double_messages, 2);
        // One response from them (90 - 60 = 30s, NOT 90s — pair is from immediate predecessor).
        assert_eq!(metrics.their_median_response_ms, Some(30_000));
        // I never replied to them in this convo, so my response list is empty.
        assert!(metrics.my_median_response_ms.is_none());
    }

    #[test]
    fn rapid_response_percentage_correct() {
        // Their: 30s (rapid), 90s (not), 10s (rapid). Mine: empty.
        let messages = vec![
            r(0, Participant::Me),
            r(30_000, Participant::Them),  // 30s rapid (≤60s default)
            r(120_000, Participant::Me),   // 90s
            r(210_000, Participant::Them), // 90s NOT rapid
            r(220_000, Participant::Me),   // 10s
            r(230_000, Participant::Them), // 10s rapid
        ];
        let metrics = compute_response_metrics(&messages, &cfg(), &seg(), 0);
        // Their responses: 30s (yes), 90s (no), 10s (yes) → 2/3 rapid.
        assert_eq!(metrics.their_rapid_response_pct, Some(2.0 / 3.0));
        // My responses: 90s (no), 10s (yes) → 1/2 rapid.
        assert_eq!(metrics.my_rapid_response_pct, Some(0.5));
    }

    // ---------- first-response logic ----------
    #[test]
    fn first_response_only_fires_for_other_side_opener() {
        // Convo started by me. They reply twice. Only the first counts as
        // their "first response".
        let messages = vec![
            r(0, Participant::Me),
            r(60_000, Participant::Them), // their first response (60s)
            r(120_000, Participant::Me),
            r(180_000, Participant::Them), // not "first"
        ];
        let metrics = compute_response_metrics(&messages, &cfg(), &seg(), 0);
        // Their first responses (in convos where I opened): just 60s.
        assert_eq!(metrics.their_median_first_response_ms, Some(60_000));
        // I never had a "first response opportunity" (they never opened a convo).
        assert!(metrics.my_median_first_response_ms.is_none());
    }

    #[test]
    fn first_response_resets_per_conversation() {
        let timeout = seg().conversation_timeout_ms;
        let messages = vec![
            // Convo 1: I open, they reply
            r(0, Participant::Me),
            r(60_000, Participant::Them),
            // Big gap → new convo
            r(timeout + 1_000_000, Participant::Them), // they open this one
            r(timeout + 1_120_000, Participant::Me),   // my first response (120s)
        ];
        let metrics = compute_response_metrics(&messages, &cfg(), &seg(), 0);
        // Their first response in convo 1: 60s.
        assert_eq!(metrics.their_median_first_response_ms, Some(60_000));
        // My first response in convo 2: 120s.
        assert_eq!(metrics.my_median_first_response_ms, Some(120_000));
    }

    // ---------- overnight tagging ----------
    #[test]
    fn long_gap_force_overnight_regardless_of_clock_time() {
        // 9-hour gap. To keep this as one conversation, override timeout to 24h.
        // (Default 4h timeout would split, generating no response pair.)
        let mut long_seg = seg();
        long_seg.conversation_timeout_ms = 24 * 60 * 60 * 1000;
        let messages = vec![
            r(0, Participant::Me), // 1970-01-01T00:00:00Z; offset 0 → 0:00 local
            r(9 * 60 * 60 * 1000, Participant::Them), // 9h later → 9am local
        ];
        let metrics = compute_response_metrics(&messages, &cfg(), &long_seg, 0);
        // 9h ≥ force_gap_ms (8h), so this is overnight regardless of clock time.
        assert_eq!(
            metrics.their_median_response_overnight_ms,
            Some(9 * 60 * 60 * 1000)
        );
        assert!(metrics.their_median_response_awake_ms.is_none());
    }

    #[test]
    fn medium_gap_at_evening_hour_is_overnight() {
        // 5-hour gap, prev message at 23:00 (in overnight window).
        // Default overnight_evening_gap = 4h, so this should fire as overnight.
        let mut long_seg = seg();
        long_seg.conversation_timeout_ms = 12 * 60 * 60 * 1000; // accommodate 5h gap

        // Prev at unix 0, +0 offset → 1970-01-01T00:00 local (hour 0). 0 IS in [23,7) overnight window.
        let messages = vec![
            r(0, Participant::Me),                    // hour 0 local — in overnight window
            r(5 * 60 * 60 * 1000, Participant::Them), // 5h later
        ];
        let metrics = compute_response_metrics(&messages, &cfg(), &long_seg, 0);
        assert_eq!(
            metrics.their_median_response_overnight_ms,
            Some(5 * 60 * 60 * 1000)
        );
    }

    #[test]
    fn medium_gap_during_daytime_is_awake() {
        // Prev at 13:00 local. 5h gap → 18:00. 5h gap doesn't force overnight,
        // and 13 is not in [23, 7) → awake.
        let mut long_seg = seg();
        long_seg.conversation_timeout_ms = 12 * 60 * 60 * 1000;
        let prev_ts: i64 = 13 * 60 * 60 * 1000; // 13:00 UTC, +0 offset → 13:00 local
        let messages = vec![
            r(prev_ts, Participant::Me),
            r(prev_ts + 5 * 60 * 60 * 1000, Participant::Them), // 5h later (18:00 local)
        ];
        let metrics = compute_response_metrics(&messages, &cfg(), &long_seg, 0);
        assert_eq!(
            metrics.their_median_response_awake_ms,
            Some(5 * 60 * 60 * 1000)
        );
        assert!(metrics.their_median_response_overnight_ms.is_none());
    }

    #[test]
    fn rapid_response_is_never_overnight() {
        // 2-min gap, prev at 23:30 local — in overnight window but gap is way
        // under the evening threshold, so awake.
        let prev_ts: i64 = 23 * 60 * 60 * 1000 + 30 * 60 * 1000; // 23:30
        let messages = vec![
            r(prev_ts, Participant::Me),
            r(prev_ts + 2 * 60 * 1000, Participant::Them), // 2 min later
        ];
        let metrics = compute_response_metrics(&messages, &cfg(), &seg(), 0);
        assert_eq!(metrics.their_median_response_awake_ms, Some(2 * 60 * 1000));
        assert!(metrics.their_median_response_overnight_ms.is_none());
    }

    // ---------- histogram ----------
    #[test]
    fn histogram_distributes_across_buckets() {
        let mut long_seg = seg();
        long_seg.conversation_timeout_ms = 48 * 60 * 60 * 1000;

        let messages = vec![
            r(0, Participant::Me),
            r(5_000, Participant::Them), // 5s → bucket 0 (<30s)
            r(60_000, Participant::Me),  // 55s → bucket 1 (<1m)
            r(60_000 + 120_000, Participant::Them), // 2m → bucket 2 (<5m)
            r(60_000 + 120_000 + 600_000, Participant::Me), // 10m → bucket 3 (<15m)
        ];
        let metrics = compute_response_metrics(&messages, &cfg(), &long_seg, 0);
        // Their bucket counts: 1 in bucket 0 (5s), 1 in bucket 2 (2m).
        assert_eq!(metrics.their_histogram[0], 1);
        assert_eq!(metrics.their_histogram[2], 1);
        // My bucket counts: 1 in bucket 1 (55s), 1 in bucket 3 (10m).
        assert_eq!(metrics.my_histogram[1], 1);
        assert_eq!(metrics.my_histogram[3], 1);
    }

    #[test]
    fn histogram_total_equals_response_count() {
        let messages = vec![
            r(0, Participant::Them),
            r(10_000, Participant::Me),
            r(70_000, Participant::Them),
            r(370_000, Participant::Me),
        ];
        let metrics = compute_response_metrics(&messages, &cfg(), &seg(), 0);
        let my_total: u32 = metrics.my_histogram.iter().sum();
        let their_total: u32 = metrics.their_histogram.iter().sum();
        // 2 my responses, 1 their response.
        assert_eq!(my_total, 2);
        assert_eq!(their_total, 1);
    }

    // ---------- median vs mean ----------
    #[test]
    fn median_robust_to_outlier_vs_mean() {
        // Eight 1-min responses + one 100-min response.
        let mut long_seg = seg();
        long_seg.conversation_timeout_ms = 6 * 60 * 60 * 1000;

        let mut messages = vec![r(0, Participant::Me)];
        let mut t = 60_000i64; // first response at +60s
        for _ in 0..8 {
            messages.push(r(t, Participant::Them));
            t += 60_000;
            messages.push(r(t, Participant::Me));
            t += 60_000;
        }
        // Now add an outlier their-response 100 minutes later.
        messages.push(r(t + 100 * 60 * 1000, Participant::Them));

        let metrics = compute_response_metrics(&messages, &cfg(), &long_seg, 0);
        // Their responses: 8 × 60s + 1 × 6000s. Median should be ~60s, mean ~700s.
        let med = metrics.their_median_response_ms.unwrap();
        let mean = metrics.their_mean_response_ms.unwrap();
        assert!(med < 5 * 60 * 1000, "median ({}) should be near 60s", med);
        assert!(
            mean > 5 * 60 * 1000,
            "mean ({}) should be pulled by outlier",
            mean
        );
    }
}
