//! Direction-of-conversation analysis.
//!
//! For each message body we count three buckets:
//! - `me_pronouns` — first-person markers (I, me, my, mine, myself)
//! - `you_pronouns` — second-person markers (you, your, yours, yourself)
//! - `other_name_hits` — occurrences of any *other* contact's first name
//!
//! Then we attribute by sender:
//! - When **I** speak: my "I" pronouns talk *about me*; my "you" pronouns
//!   talk *about them*.
//! - When **they** speak: their "I" pronouns talk *about them*; their "you"
//!   pronouns talk *about me*.
//! - Mentions of other-contact names always count as *about others*.
//!
//! This is heuristic, not NER, but it produces the right shape of donut
//! mimoto shows ("47% you / 37% them / 16% other") without dragging in a
//! NER model. Future work: a proper NER pass for richer "others" detection.

use crate::aggregator::AggregatorMessage;
use crate::types::Participant;
use regex::Regex;
use std::sync::LazyLock;

static ME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(i|me|my|mine|myself|i'?m|i'?ll|i'?ve|i'?d)\b").unwrap());
static YOU_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(you|your|yours|yourself|you'?re|you'?ll|you'?ve|you'?d|u)\b").unwrap()
});

/// Focus output. Sums to 1.0 unless ALL three buckets are zero (no signal),
/// in which case all fields are 0.0.
#[derive(Debug, Clone, Copy, Default)]
pub struct FocusOutput {
    pub focus_me_pct: f64,
    pub focus_them_pct: f64,
    pub focus_other_pct: f64,
}

/// Compute focus percentages for a contact.
///
/// `other_first_names` should be a deduplicated list of first names belonging
/// to *other* contacts (not the pair under analysis). Lower-cased; we
/// case-insensitive-match in the regex layer.
pub fn compute_focus(messages: &[AggregatorMessage], other_first_names: &[String]) -> FocusOutput {
    if messages.is_empty() {
        return FocusOutput::default();
    }

    // Compile a single regex over all other-name candidates so we walk each
    // body just once. Skip if the list is empty.
    let other_re = build_other_names_regex(other_first_names);

    let mut about_me: u64 = 0;
    let mut about_them: u64 = 0;
    let mut about_other: u64 = 0;

    for msg in messages {
        let body = msg.body.as_str();
        let me_count = ME_RE.find_iter(body).count() as u64;
        let you_count = YOU_RE.find_iter(body).count() as u64;
        let other_count = other_re
            .as_ref()
            .map(|re| re.find_iter(body).count() as u64)
            .unwrap_or(0);

        match msg.sender {
            Participant::Me => {
                about_me += me_count;
                about_them += you_count;
            }
            Participant::Them => {
                about_me += you_count;
                about_them += me_count;
            }
        }
        about_other += other_count;
    }

    let total = (about_me + about_them + about_other) as f64;
    if total <= 0.0 {
        return FocusOutput::default();
    }
    FocusOutput {
        focus_me_pct: about_me as f64 / total,
        focus_them_pct: about_them as f64 / total,
        focus_other_pct: about_other as f64 / total,
    }
}

/// Build a single OR'd word-boundary regex from the supplied other-names list.
/// Returns None if the list is empty (skip allocation).
fn build_other_names_regex(names: &[String]) -> Option<Regex> {
    let cleaned: Vec<String> = names
        .iter()
        .filter_map(|n| {
            let t = n.trim();
            if t.is_empty() {
                return None;
            }
            // Pick the first whitespace-delimited token as a "first name".
            // Strip non-alphanumeric for safety so we don't regex-inject.
            let first = t.split_whitespace().next().unwrap_or(t);
            let safe: String = first
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .collect();
            if safe.len() < 2 {
                None
            } else {
                Some(regex::escape(&safe))
            }
        })
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    let pattern = format!(r"(?i)\b({})\b", cleaned.join("|"));
    Regex::new(&pattern).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn am(rowid: i64, sender: Participant, body: &str) -> AggregatorMessage {
        AggregatorMessage {
            db_rowid: rowid,
            timestamp_ms: rowid * 1000,
            sender,
            body: body.to_string(),
            mime_types: Vec::new(),
        }
    }

    #[test]
    fn empty_input_returns_zeros() {
        let out = compute_focus(&[], &[]);
        assert_eq!(out.focus_me_pct, 0.0);
        assert_eq!(out.focus_them_pct, 0.0);
        assert_eq!(out.focus_other_pct, 0.0);
    }

    #[test]
    fn me_pronoun_in_my_message_counts_as_focus_on_me() {
        let messages = vec![am(1, Participant::Me, "I think I'm tired")];
        let out = compute_focus(&messages, &[]);
        assert!(out.focus_me_pct > 0.9);
        assert_eq!(out.focus_them_pct, 0.0);
    }

    #[test]
    fn you_pronoun_in_my_message_counts_as_focus_on_them() {
        let messages = vec![am(1, Participant::Me, "You should try this")];
        let out = compute_focus(&messages, &[]);
        assert!(out.focus_them_pct > 0.9);
        assert_eq!(out.focus_me_pct, 0.0);
    }

    #[test]
    fn me_pronoun_in_their_message_counts_as_focus_on_them() {
        let messages = vec![am(1, Participant::Them, "I'm running late")];
        let out = compute_focus(&messages, &[]);
        assert!(out.focus_them_pct > 0.9);
    }

    #[test]
    fn you_pronoun_in_their_message_counts_as_focus_on_me() {
        let messages = vec![am(1, Participant::Them, "You're awesome")];
        let out = compute_focus(&messages, &[]);
        assert!(out.focus_me_pct > 0.9);
    }

    #[test]
    fn other_name_mentions_count_as_focus_on_others() {
        let messages = vec![
            am(1, Participant::Me, "Anna and I are heading out"),
            am(2, Participant::Them, "tell Justin hi"),
        ];
        let names = vec!["Anna".to_string(), "Justin".to_string()];
        let out = compute_focus(&messages, &names);
        // We have me-pronouns (I) + you-pronouns (none) + other-names (Anna, Justin).
        // "Anna" and "Justin" both fire other_count → focus_other_pct > 0.
        assert!(out.focus_other_pct > 0.3);
        // We have at least one me/them mention too.
        assert!(out.focus_me_pct > 0.0 || out.focus_them_pct > 0.0);
    }

    #[test]
    fn percentages_sum_to_one() {
        let messages = vec![
            am(1, Participant::Me, "I sent you that link"),
            am(2, Participant::Them, "thanks I'll check it"),
            am(3, Participant::Me, "no rush"),
        ];
        let out = compute_focus(&messages, &[]);
        let total = out.focus_me_pct + out.focus_them_pct + out.focus_other_pct;
        assert!(
            (total - 1.0).abs() < 1e-9,
            "expected sum=1.0, got {}",
            total
        );
    }

    #[test]
    fn other_names_short_or_empty_are_dropped() {
        // Single-letter names can cause too many false positives — they're filtered out.
        let messages = vec![am(1, Participant::Me, "a quick note")];
        let names = vec!["A".to_string(), "".to_string()];
        let out = compute_focus(&messages, &names);
        assert_eq!(out.focus_other_pct, 0.0);
    }
}
