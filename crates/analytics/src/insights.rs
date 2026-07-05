//! Insight rule engine — generates the colored callouts on the dashboard
//! ("You laugh more than your contact", "They initiate far more conversations",
//! etc.). Hardcoded rules for v1; declarative DSL is a future upgrade.
//!
//! # Architecture
//!
//! Each rule is a pure function `(InsightCtx) -> Option<Insight>`. The engine
//! runs every rule, collects the `Some` results, sorts by `(category,
//! confidence DESC, tier DESC)`, and caps each category at a small number
//! per Q7b.
//!
//! # Tier classification
//!
//! Per Q7a we use a tiered ratio + absolute-difference scheme:
//!
//! | Tier | Min ratio (larger/smaller) | Wording          |
//! |------|----------------------------|------------------|
//! | 1    | 1.2×                       | "slightly more"  |
//! | 2    | 1.5×                       | "more"           |
//! | 3    | 2.0×                       | "much more"      |
//! | 4    | 3.0×                       | "far more"       |
//!
//! Plus a sample-size floor: the absolute difference must be at least 1% of
//! the combined total (or 50, whichever is larger). This prevents tiny
//! samples from firing.

use crate::aggregator::ContactAggregates;
use crate::responses::ResponseMetrics;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InsightCategory {
    YourSide,
    TheirSide,
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Insight {
    pub id: &'static str,
    pub icon: char,
    pub headline: String,
    pub detail: String,
    pub tier: u8,
    pub category: InsightCategory,
    pub confidence: Confidence,
}

/// Bundle of pre-computed metrics passed to every insight rule. Built by the
/// orchestrator once and shared across rule evaluations.
#[derive(Debug, Clone)]
pub struct InsightCtx<'a> {
    pub contact: &'a ContactAggregates,
    pub responses: &'a ResponseMetrics,
    pub conversations_started_by_me: u32,
    pub conversations_started_by_them: u32,
    pub conversations_closed_by_me: u32,
    pub conversations_closed_by_them: u32,
    pub my_convos_missed: u32,
    pub their_convos_missed: u32,
    pub reconnect_count_total: u32,
    pub total_conversations: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// Cap on insights shown per category. Default 3 — matches Q7b.
    pub max_per_category: usize,
    /// Minimum combined sample size before any rule on a count-comparison
    /// fires at Low confidence. Lower than this → no insight at all.
    pub min_sample_for_low_confidence: u32,
    /// Combined sample size at which we upgrade Low → Medium.
    pub min_sample_for_medium: u32,
    /// Combined sample size at which we upgrade Medium → High.
    pub min_sample_for_high: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            // Persist all insights to JSON; the UI applies its own per-category
            // display cap and reveals the full set behind a "Show all" toggle.
            // 100 is "effectively unlimited" for the v1 ruleset of 11 rules.
            max_per_category: 100,
            min_sample_for_low_confidence: 50,
            min_sample_for_medium: 100,
            min_sample_for_high: 500,
        }
    }
}

/// Run all insight rules and return the curated, ordered list.
pub fn compute_insights(ctx: &InsightCtx, config: &EngineConfig) -> Vec<Insight> {
    let raw: Vec<Insight> = ALL_RULES
        .iter()
        .filter_map(|rule| rule(ctx, config))
        .collect();

    // Group by category, sort each group by (confidence DESC, tier DESC), cap.
    let mut by_cat: [Vec<Insight>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for insight in raw {
        let bucket = match insight.category {
            InsightCategory::YourSide => &mut by_cat[0],
            InsightCategory::TheirSide => &mut by_cat[1],
            InsightCategory::Shared => &mut by_cat[2],
        };
        bucket.push(insight);
    }
    for bucket in by_cat.iter_mut() {
        bucket.sort_by(|a, b| {
            b.confidence
                .cmp(&a.confidence)
                .then_with(|| b.tier.cmp(&a.tier))
                .then_with(|| a.id.cmp(b.id))
        });
        bucket.truncate(config.max_per_category);
    }

    // Flatten in (YourSide, TheirSide, Shared) order — matches dashboard layout.
    let mut all = Vec::new();
    for bucket in by_cat {
        all.extend(bucket);
    }
    all
}

// =========================================================================
// TIER CLASSIFICATION
// =========================================================================

#[derive(Debug, Clone, Copy)]
struct TierResult {
    tier: u8,
    ratio: f64,
    mine_higher: bool,
}

/// Classify a count comparison into a tier 1-4 (or None if not significant).
/// Requires both a ratio threshold AND an absolute-difference floor that
/// scales with combined sample size.
fn classify_count_comparison(my: u32, their: u32) -> Option<TierResult> {
    let total = my + their;
    if total == 0 {
        return None;
    }
    let larger = my.max(their);
    let smaller = my.min(their);
    let abs_diff = larger - smaller;

    // Floor: at least 50, or 1% of combined total (whichever is larger).
    let floor = ((total as f64) * 0.01).max(50.0) as u32;
    if abs_diff < floor {
        return None;
    }

    let ratio = if smaller == 0 {
        // Pure asymmetry — treat as max tier.
        f64::INFINITY
    } else {
        larger as f64 / smaller as f64
    };

    let tier = if ratio >= 3.0 {
        4
    } else if ratio >= 2.0 {
        3
    } else if ratio >= 1.5 {
        2
    } else if ratio >= 1.2 {
        1
    } else {
        return None;
    };

    Some(TierResult {
        tier,
        ratio,
        mine_higher: my > their,
    })
}

/// Map a tier 1-4 to the human phrase modifier ("slightly", "much", etc).
fn tier_word(tier: u8) -> &'static str {
    match tier {
        1 => "slightly more",
        2 => "more",
        3 => "much more",
        4 => "far more",
        _ => "more",
    }
}

/// Map combined sample size to confidence band.
fn confidence_for_sample(combined: u32, config: &EngineConfig) -> Option<Confidence> {
    if combined < config.min_sample_for_low_confidence {
        None
    } else if combined < config.min_sample_for_medium {
        Some(Confidence::Low)
    } else if combined < config.min_sample_for_high {
        Some(Confidence::Medium)
    } else {
        Some(Confidence::High)
    }
}

/// Format a "X vs Y, ±Z%" detail string. Handles the pure-asymmetry case
/// (one side has zero) by reading "vastly more often" instead of "inf×".
fn format_detail_compare(my: u32, their: u32, ratio: f64) -> String {
    let larger = my.max(their) as f64;
    let smaller = my.min(their) as f64;
    if smaller == 0.0 {
        return format!("{} vs {} (entirely one-sided)", my, their);
    }
    let pct = ((larger - smaller) / smaller * 100.0).round() as i64;
    format!("{:.2}× as often ({} vs {}, +{}%)", ratio, my, their, pct)
}

// =========================================================================
// CHI-SQUARE HELPER (public, available for rules that want extra rigor)
// =========================================================================

/// 2×2 chi-square test statistic. Returns the chi-square value; the caller
/// compares to a threshold (3.84 for p<0.05, 6.63 for p<0.01).
///
/// `(observed_a, total_a)` and `(observed_b, total_b)` describe two groups
/// with a count of "interesting" events out of total events each.
///
/// Returns 0.0 if either total is zero (no test possible).
pub fn chi_square_2x2(observed_a: u32, total_a: u32, observed_b: u32, total_b: u32) -> f64 {
    if total_a == 0 || total_b == 0 {
        return 0.0;
    }
    let total_observed = observed_a + observed_b;
    let total_population = total_a + total_b;
    if total_population == 0 {
        return 0.0;
    }
    let expected_a = total_observed as f64 * (total_a as f64 / total_population as f64);
    let expected_b = total_observed as f64 * (total_b as f64 / total_population as f64);
    let mut chi: f64 = 0.0;
    for (obs, exp) in [
        (observed_a as f64, expected_a),
        (observed_b as f64, expected_b),
    ] {
        if exp > 0.0 {
            chi += (obs - exp).powi(2) / exp;
        }
    }
    // We're computing 1-DF chi-square for the "interesting" column. The
    // "non-interesting" column contributes symmetrically; standard 2×2
    // formula doubles this. We approximate by multiplying by 2 — fast and
    // close enough for our gating use case.
    chi * 2.0
}

// =========================================================================
// RULES
// =========================================================================

type Rule = fn(&InsightCtx, &EngineConfig) -> Option<Insight>;

const ALL_RULES: &[Rule] = &[
    rule_laughs,
    rule_apologies,
    rule_encouragement,
    rule_questions,
    rule_messages_volume,
    rule_initiation,
    rule_response_speed,
    rule_first_response_speed,
    rule_endings_even,
    rule_missed_even,
    rule_rarely_reconnect,
];

/// Helper that builds a comparative-counts insight if the data warrants it.
/// `id` and `icon` are insight metadata; `verb` is the action verb in present
/// tense — passed in because some pairs use different verbs ("ask" for
/// questions, "send" for messages).
#[allow(clippy::too_many_arguments)]
fn build_compare(
    _ctx: &InsightCtx,
    config: &EngineConfig,
    id: &'static str,
    icon: char,
    verb: &str,
    my: u32,
    their: u32,
) -> Option<Insight> {
    let tier_result = classify_count_comparison(my, their)?;
    let confidence = confidence_for_sample(my + their, config)?;
    let (subject, category) = if tier_result.mine_higher {
        ("You", InsightCategory::YourSide)
    } else {
        ("They", InsightCategory::TheirSide)
    };
    let headline = format!(
        "{} {} {} than {}",
        subject,
        verb,
        tier_word(tier_result.tier),
        if tier_result.mine_higher {
            "they do"
        } else {
            "you do"
        }
    );
    let detail = format_detail_compare(my, their, tier_result.ratio);
    Some(Insight {
        id,
        icon,
        headline,
        detail,
        tier: tier_result.tier,
        category,
        confidence,
    })
}

fn rule_laughs(ctx: &InsightCtx, config: &EngineConfig) -> Option<Insight> {
    build_compare(
        ctx,
        config,
        "laughs",
        '\u{1F602}', // 😂
        "laugh",
        ctx.contact.my_laugh_count,
        ctx.contact.their_laugh_count,
    )
}

fn rule_apologies(ctx: &InsightCtx, config: &EngineConfig) -> Option<Insight> {
    build_compare(
        ctx,
        config,
        "apologies",
        '\u{1F54A}', // 🕊️
        "apologize",
        ctx.contact.my_apology_count,
        ctx.contact.their_apology_count,
    )
}

fn rule_encouragement(ctx: &InsightCtx, config: &EngineConfig) -> Option<Insight> {
    build_compare(
        ctx,
        config,
        "encouragement",
        '\u{1F44F}', // 👏
        "send encouragement",
        ctx.contact.my_encouragement_count,
        ctx.contact.their_encouragement_count,
    )
}

fn rule_questions(ctx: &InsightCtx, config: &EngineConfig) -> Option<Insight> {
    build_compare(
        ctx,
        config,
        "questions",
        '\u{2753}', // ❓
        "ask questions",
        ctx.contact.my_question_count,
        ctx.contact.their_question_count,
    )
}

fn rule_messages_volume(ctx: &InsightCtx, config: &EngineConfig) -> Option<Insight> {
    build_compare(
        ctx,
        config,
        "messages_volume",
        '\u{1F4AC}', // 💬
        "send messages",
        ctx.contact.my_message_count,
        ctx.contact.their_message_count,
    )
}

fn rule_initiation(ctx: &InsightCtx, config: &EngineConfig) -> Option<Insight> {
    build_compare(
        ctx,
        config,
        "initiation",
        '\u{1F44B}', // 👋
        "initiate conversations",
        ctx.conversations_started_by_me,
        ctx.conversations_started_by_them,
    )
}

/// Response speed: lower median = faster. Phrased differently from count
/// comparisons because the direction is inverted.
fn rule_response_speed(ctx: &InsightCtx, config: &EngineConfig) -> Option<Insight> {
    let mine = ctx.responses.my_median_response_ms?;
    let theirs = ctx.responses.their_median_response_ms?;
    if mine <= 0 || theirs <= 0 {
        return None;
    }
    let larger = mine.max(theirs) as f64;
    let smaller = mine.min(theirs) as f64;
    if smaller <= 0.0 {
        return None;
    }
    let ratio = larger / smaller;
    let tier = if ratio >= 3.0 {
        4
    } else if ratio >= 2.0 {
        3
    } else if ratio >= 1.5 {
        2
    } else if ratio >= 1.2 {
        1
    } else {
        return None;
    };

    // Confidence on responses based on combined message count (proxy for
    // how many response pairs we likely had).
    let confidence = confidence_for_sample(
        ctx.contact.my_message_count + ctx.contact.their_message_count,
        config,
    )?;

    let i_am_faster = mine < theirs;
    let (subject, category) = if i_am_faster {
        ("You", InsightCategory::YourSide)
    } else {
        ("They", InsightCategory::TheirSide)
    };
    let headline = format!(
        "{} respond {} than {}",
        subject,
        match tier {
            1 => "slightly faster",
            2 => "faster",
            3 => "much faster",
            _ => "far faster",
        },
        if i_am_faster { "they do" } else { "you do" }
    );
    let detail = format!(
        "{:.2}× faster (median {} vs {})",
        ratio,
        format_duration_short(mine),
        format_duration_short(theirs)
    );
    Some(Insight {
        id: "response_speed",
        icon: '\u{1F552}', // 🕒
        headline,
        detail,
        tier,
        category,
        confidence,
    })
}

fn rule_first_response_speed(ctx: &InsightCtx, config: &EngineConfig) -> Option<Insight> {
    let mine = ctx.responses.my_median_first_response_ms?;
    let theirs = ctx.responses.their_median_first_response_ms?;
    if mine <= 0 || theirs <= 0 {
        return None;
    }
    let larger = mine.max(theirs) as f64;
    let smaller = mine.min(theirs) as f64;
    if smaller <= 0.0 {
        return None;
    }
    let ratio = larger / smaller;
    let tier = if ratio >= 3.0 {
        4
    } else if ratio >= 2.0 {
        3
    } else if ratio >= 1.5 {
        2
    } else if ratio >= 1.2 {
        1
    } else {
        return None;
    };
    let confidence = confidence_for_sample(
        ctx.conversations_started_by_me + ctx.conversations_started_by_them,
        config,
    )?;

    let i_am_faster = mine < theirs;
    let (subject, category) = if i_am_faster {
        ("You", InsightCategory::YourSide)
    } else {
        ("They", InsightCategory::TheirSide)
    };
    let headline = format!(
        "{} respond {} when new conversations begin",
        subject,
        match tier {
            1 => "slightly faster",
            2 => "faster",
            3 => "much faster",
            _ => "far faster",
        }
    );
    let detail = format!(
        "{:.2}× faster (first-response median {} vs {})",
        ratio,
        format_duration_short(mine),
        format_duration_short(theirs)
    );
    Some(Insight {
        id: "first_response_speed",
        icon: '\u{26A1}', // ⚡
        headline,
        detail,
        tier,
        category,
        confidence,
    })
}

/// Conversation endings shared evenly: fires when |closed_by_me - closed_by_them|
/// is small AND the total is meaningful.
fn rule_endings_even(ctx: &InsightCtx, config: &EngineConfig) -> Option<Insight> {
    let total = ctx.conversations_closed_by_me + ctx.conversations_closed_by_them;
    let confidence = confidence_for_sample(total, config)?;
    if classify_count_comparison(
        ctx.conversations_closed_by_me,
        ctx.conversations_closed_by_them,
    )
    .is_some()
    {
        // There IS a significant difference — different rule territory.
        return None;
    }
    Some(Insight {
        id: "endings_even",
        icon: '\u{1F44B}', // 👋
        headline: "Conversation endings are evenly shared".to_string(),
        detail: format!(
            "You closed {}, they closed {}",
            ctx.conversations_closed_by_me, ctx.conversations_closed_by_them
        ),
        tier: 2,
        category: InsightCategory::Shared,
        confidence,
    })
}

fn rule_missed_even(ctx: &InsightCtx, config: &EngineConfig) -> Option<Insight> {
    let total = ctx.my_convos_missed + ctx.their_convos_missed;
    let confidence = confidence_for_sample(total, config)?;
    if classify_count_comparison(ctx.my_convos_missed, ctx.their_convos_missed).is_some() {
        return None;
    }
    Some(Insight {
        id: "missed_even",
        icon: '\u{1F47B}', // 👻
        headline: "Your missed conversations are evenly matched".to_string(),
        detail: format!(
            "You missed {}, they missed {}",
            ctx.my_convos_missed, ctx.their_convos_missed
        ),
        tier: 2,
        category: InsightCategory::Shared,
        confidence,
    })
}

/// "You rarely reconnect after long silences" — fires when reconnects are
/// less than 5% of total conversations on a meaningful sample.
fn rule_rarely_reconnect(ctx: &InsightCtx, config: &EngineConfig) -> Option<Insight> {
    if ctx.total_conversations < config.min_sample_for_low_confidence {
        return None;
    }
    let pct = ctx.reconnect_count_total as f64 / ctx.total_conversations as f64;
    if pct >= 0.05 {
        return None;
    }
    let confidence = confidence_for_sample(ctx.total_conversations, config)?;
    Some(Insight {
        id: "rarely_reconnect",
        icon: '\u{2744}', // ❄
        headline: "You rarely reconnect after long silences".to_string(),
        detail: format!(
            "{} reconnect{} out of {} conversations ({:.1}%)",
            ctx.reconnect_count_total,
            if ctx.reconnect_count_total == 1 {
                ""
            } else {
                "s"
            },
            ctx.total_conversations,
            pct * 100.0
        ),
        tier: 2,
        category: InsightCategory::Shared,
        confidence,
    })
}

/// Format a millisecond duration as a short human string for insight details.
fn format_duration_short(ms: i64) -> String {
    if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else if ms < 60 * 60_000 {
        format!("{}m", ms / 60_000)
    } else if ms < 24 * 60 * 60_000 {
        format!("{}h", ms / (60 * 60_000))
    } else {
        format!("{}d", ms / (24 * 60 * 60_000))
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)] // fixture builders read better as assignments
mod tests {
    use super::*;

    fn empty_ctx() -> (ContactAggregates, ResponseMetrics) {
        (ContactAggregates::default(), ResponseMetrics::default())
    }

    fn build_ctx<'a>(
        contact: &'a ContactAggregates,
        responses: &'a ResponseMetrics,
    ) -> InsightCtx<'a> {
        InsightCtx {
            contact,
            responses,
            conversations_started_by_me: 0,
            conversations_started_by_them: 0,
            conversations_closed_by_me: 0,
            conversations_closed_by_them: 0,
            my_convos_missed: 0,
            their_convos_missed: 0,
            reconnect_count_total: 0,
            total_conversations: 0,
        }
    }

    // ---------- tier classification ----------
    #[test]
    fn tier_below_floor_returns_none() {
        // 51 vs 49: ratio 1.04, abs diff 2, well under floor.
        assert!(classify_count_comparison(51, 49).is_none());
    }

    #[test]
    fn tier_below_ratio_returns_none_even_with_diff() {
        // 1100 vs 1000: ratio 1.10 (under 1.2) → no insight, even though abs
        // diff 100 exceeds the floor.
        assert!(classify_count_comparison(1100, 1000).is_none());
    }

    #[test]
    fn tier_progression_increases_with_ratio() {
        // Floor is max(50, 1% of total). All test cases have totals ≥ 1000
        // so the floor is the absolute 50 minimum — abs_diffs of 100+ are fine.
        // 60 vs 50: abs diff 10 < floor 50 → no fire even though ratio is 1.2.
        assert!(classify_count_comparison(60, 50).is_none());

        // 600 vs 500: ratio 1.2, abs diff 100 → tier 1.
        let t1 = classify_count_comparison(600, 500).unwrap();
        assert_eq!(t1.tier, 1, "600/500 (ratio 1.2) → tier 1 (slightly more)");

        // 750 vs 500: ratio 1.5, abs diff 250 → tier 2.
        let t2 = classify_count_comparison(750, 500).unwrap();
        assert_eq!(t2.tier, 2, "750/500 (ratio 1.5) → tier 2 (more)");

        // 1000 vs 500: ratio 2.0, abs diff 500 → tier 3.
        let t3 = classify_count_comparison(1000, 500).unwrap();
        assert_eq!(t3.tier, 3, "1000/500 (ratio 2.0) → tier 3 (much more)");

        // 1500 vs 500: ratio 3.0 — boundaries are >= so this hits tier 4.
        let t4 = classify_count_comparison(1500, 500).unwrap();
        assert_eq!(t4.tier, 4, "1500/500 (ratio 3.0) → tier 4 (far more)");
    }

    #[test]
    fn tier_pure_asymmetry_is_max_tier() {
        let t = classify_count_comparison(100, 0).unwrap();
        assert_eq!(t.tier, 4);
    }

    // ---------- confidence ----------
    #[test]
    fn confidence_thresholds() {
        let c = EngineConfig::default();
        assert_eq!(confidence_for_sample(10, &c), None);
        assert_eq!(confidence_for_sample(60, &c), Some(Confidence::Low));
        assert_eq!(confidence_for_sample(200, &c), Some(Confidence::Medium));
        assert_eq!(confidence_for_sample(1000, &c), Some(Confidence::High));
    }

    // ---------- chi-square ----------
    #[test]
    fn chi_square_returns_zero_on_empty_groups() {
        assert_eq!(chi_square_2x2(0, 0, 0, 0), 0.0);
        assert_eq!(chi_square_2x2(5, 0, 5, 10), 0.0);
    }

    #[test]
    fn chi_square_zero_when_proportions_equal() {
        // 50/100 in both groups → expected matches observed → chi = 0.
        let chi = chi_square_2x2(50, 100, 50, 100);
        assert!(chi < 0.01, "chi={} expected near 0", chi);
    }

    #[test]
    fn chi_square_large_when_proportions_differ() {
        // 90/100 vs 10/100 → very large chi-square.
        let chi = chi_square_2x2(90, 100, 10, 100);
        assert!(chi > 50.0, "chi={} expected > 50", chi);
    }

    // ---------- format helpers ----------
    #[test]
    fn duration_format_picks_appropriate_unit() {
        assert_eq!(format_duration_short(45_000), "45s");
        assert_eq!(format_duration_short(120_000), "2m");
        assert_eq!(format_duration_short(7_200_000), "2h");
        assert_eq!(format_duration_short(2 * 24 * 60 * 60_000), "2d");
    }

    // ---------- individual rules ----------
    #[test]
    fn laughs_rule_fires_for_clear_majority() {
        let mut contact = ContactAggregates::default();
        contact.my_laugh_count = 200;
        contact.their_laugh_count = 100;
        let responses = ResponseMetrics::default();
        let ctx = build_ctx(&contact, &responses);
        let cfg = EngineConfig::default();
        let insight = rule_laughs(&ctx, &cfg).expect("rule should fire");
        assert_eq!(insight.id, "laughs");
        assert_eq!(insight.category, InsightCategory::YourSide);
        assert!(insight.headline.contains("You laugh"));
        assert_eq!(insight.tier, 3, "200/100 → tier 3");
    }

    #[test]
    fn laughs_rule_does_not_fire_for_balanced_counts() {
        let mut contact = ContactAggregates::default();
        contact.my_laugh_count = 100;
        contact.their_laugh_count = 95;
        let responses = ResponseMetrics::default();
        let ctx = build_ctx(&contact, &responses);
        let cfg = EngineConfig::default();
        assert!(rule_laughs(&ctx, &cfg).is_none());
    }

    #[test]
    fn questions_rule_attributes_correctly_to_their_side() {
        let mut contact = ContactAggregates::default();
        contact.my_question_count = 50;
        contact.their_question_count = 200;
        let responses = ResponseMetrics::default();
        let ctx = build_ctx(&contact, &responses);
        let cfg = EngineConfig::default();
        let insight = rule_questions(&ctx, &cfg).expect("rule should fire");
        assert_eq!(insight.category, InsightCategory::TheirSide);
        assert!(insight.headline.contains("They"));
    }

    #[test]
    fn response_speed_rule_phrases_inverted() {
        let mut responses = ResponseMetrics::default();
        responses.my_median_response_ms = Some(60_000); // 1 min
        responses.their_median_response_ms = Some(180_000); // 3 min
        let mut contact = ContactAggregates::default();
        contact.my_message_count = 500;
        contact.their_message_count = 500;
        let ctx = build_ctx(&contact, &responses);
        let cfg = EngineConfig::default();
        let insight = rule_response_speed(&ctx, &cfg).expect("rule should fire");
        assert!(insight.headline.contains("You respond"));
        assert!(insight.headline.contains("faster"));
        // Ratio 3.0 hits tier 4 (boundaries are inclusive).
        assert_eq!(insight.tier, 4, "3× ratio → tier 4 (>=3 boundary)");
    }

    #[test]
    fn endings_even_fires_when_balanced_and_meaningful() {
        let contact = ContactAggregates::default();
        let responses = ResponseMetrics::default();
        let mut ctx = build_ctx(&contact, &responses);
        ctx.conversations_closed_by_me = 100;
        ctx.conversations_closed_by_them = 105;
        let cfg = EngineConfig::default();
        let insight = rule_endings_even(&ctx, &cfg).expect("rule should fire");
        assert!(insight.headline.contains("evenly shared"));
        assert_eq!(insight.category, InsightCategory::Shared);
    }

    #[test]
    fn endings_even_suppressed_when_imbalanced() {
        let contact = ContactAggregates::default();
        let responses = ResponseMetrics::default();
        let mut ctx = build_ctx(&contact, &responses);
        ctx.conversations_closed_by_me = 200;
        ctx.conversations_closed_by_them = 50;
        let cfg = EngineConfig::default();
        // Significant imbalance → endings_even should NOT fire.
        assert!(rule_endings_even(&ctx, &cfg).is_none());
    }

    #[test]
    fn rarely_reconnect_fires_when_under_5pct() {
        let contact = ContactAggregates::default();
        let responses = ResponseMetrics::default();
        let mut ctx = build_ctx(&contact, &responses);
        ctx.total_conversations = 1000;
        ctx.reconnect_count_total = 30; // 3%
        let cfg = EngineConfig::default();
        let insight = rule_rarely_reconnect(&ctx, &cfg).expect("rule should fire");
        assert!(insight.headline.contains("rarely reconnect"));
    }

    #[test]
    fn rarely_reconnect_suppressed_when_above_5pct() {
        let contact = ContactAggregates::default();
        let responses = ResponseMetrics::default();
        let mut ctx = build_ctx(&contact, &responses);
        ctx.total_conversations = 1000;
        ctx.reconnect_count_total = 100; // 10%
        let cfg = EngineConfig::default();
        assert!(rule_rarely_reconnect(&ctx, &cfg).is_none());
    }

    // ---------- compute_insights orchestration ----------
    #[test]
    fn compute_insights_returns_empty_for_empty_ctx() {
        let (contact, responses) = empty_ctx();
        let ctx = build_ctx(&contact, &responses);
        let cfg = EngineConfig::default();
        let out = compute_insights(&ctx, &cfg);
        assert!(out.is_empty());
    }

    #[test]
    fn compute_insights_caps_per_category() {
        // Construct a ctx that fires multiple YourSide rules. Confirm the cap.
        let mut contact = ContactAggregates::default();
        contact.my_laugh_count = 500;
        contact.their_laugh_count = 100;
        contact.my_apology_count = 500;
        contact.their_apology_count = 100;
        contact.my_encouragement_count = 500;
        contact.their_encouragement_count = 100;
        contact.my_question_count = 500;
        contact.their_question_count = 100;
        contact.my_message_count = 5000;
        contact.their_message_count = 1000;
        let responses = ResponseMetrics::default();
        let ctx = build_ctx(&contact, &responses);
        let cfg = EngineConfig {
            max_per_category: 3,
            ..EngineConfig::default()
        };
        let out = compute_insights(&ctx, &cfg);
        let your_side_count = out
            .iter()
            .filter(|i| i.category == InsightCategory::YourSide)
            .count();
        assert!(
            your_side_count <= 3,
            "expected ≤3 YourSide, got {}",
            your_side_count
        );
    }

    #[test]
    fn compute_insights_orders_yourside_then_theirside_then_shared() {
        let mut contact = ContactAggregates::default();
        contact.my_laugh_count = 500; // YourSide rule fires
        contact.their_laugh_count = 100;
        contact.my_question_count = 100; // TheirSide rule fires
        contact.their_question_count = 500;
        let responses = ResponseMetrics::default();
        let mut ctx = build_ctx(&contact, &responses);
        ctx.conversations_closed_by_me = 100; // Shared rule fires (even)
        ctx.conversations_closed_by_them = 100;
        let cfg = EngineConfig::default();
        let out = compute_insights(&ctx, &cfg);
        // Expect at least one of each category, in YourSide → TheirSide → Shared order.
        let categories: Vec<InsightCategory> = out.iter().map(|i| i.category).collect();
        let mut last = InsightCategory::YourSide;
        for c in categories {
            // YourSide(0) ≤ TheirSide(1) ≤ Shared(2). Encode for cmp.
            let order = |x: InsightCategory| match x {
                InsightCategory::YourSide => 0,
                InsightCategory::TheirSide => 1,
                InsightCategory::Shared => 2,
            };
            assert!(order(c) >= order(last), "categories must be non-decreasing");
            last = c;
        }
    }
}
