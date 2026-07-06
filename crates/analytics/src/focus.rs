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
///
/// This compiles the "other names" regex from `other_first_names` on every
/// call. That's fine for a single contact, but callers recomputing analytics
/// for *every* contact in the archive should use [`compute_focus_shared`]
/// instead: it takes a regex built once (over every contact) plus a cheap
/// per-contact self-exclusion token, so the O(N)-size regex is compiled a
/// single time for the whole run instead of once per contact.
pub fn compute_focus(messages: &[AggregatorMessage], other_first_names: &[String]) -> FocusOutput {
    let other_re = build_other_names_regex(other_first_names);
    compute_focus_matching(messages, other_re.as_ref(), None)
}

/// Compute focus percentages using a *pre-built* "other names" regex shared
/// across an entire analytics run (e.g. built once from every contact in the
/// archive, rather than once per contact — see `orchestrator::compute_for_all_contacts`).
///
/// Because the shared regex is built from *every* contact's name, it may
/// also match the current contact's own first name. `self_exclude_token`
/// (lower-cased) is used to discard those self-mentions from the "other"
/// bucket, so results are identical to calling [`compute_focus`] with a list
/// that explicitly excludes this contact. When the current contact's first
/// name is shared with another contact, pass `None`: the name is still a
/// legitimate "other contact" from that other contact's point of view, and
/// the original per-contact behavior never excluded shared names either.
pub(crate) fn compute_focus_shared(
    messages: &[AggregatorMessage],
    shared_other_names_re: Option<&Regex>,
    self_exclude_token: Option<&str>,
) -> FocusOutput {
    compute_focus_matching(messages, shared_other_names_re, self_exclude_token)
}

/// Shared implementation for [`compute_focus`] and [`compute_focus_shared`].
fn compute_focus_matching(
    messages: &[AggregatorMessage],
    other_re: Option<&Regex>,
    self_exclude_token: Option<&str>,
) -> FocusOutput {
    if messages.is_empty() {
        return FocusOutput::default();
    }

    let mut about_me: u64 = 0;
    let mut about_them: u64 = 0;
    let mut about_other: u64 = 0;

    for msg in messages {
        let body = msg.body.as_str();
        let me_count = ME_RE.find_iter(body).count() as u64;
        let you_count = YOU_RE.find_iter(body).count() as u64;
        let other_count = other_re
            .map(|re| count_other_hits(re, body, self_exclude_token))
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

/// Count regex hits in `body`, discarding any hit that case-insensitively
/// equals `self_exclude_token` (used to drop self-mentions when matching
/// against a shared, multi-contact regex).
fn count_other_hits(re: &Regex, body: &str, self_exclude_token: Option<&str>) -> u64 {
    match self_exclude_token {
        None => re.find_iter(body).count() as u64,
        Some(tok) => re
            .find_iter(body)
            .filter(|m| !m.as_str().eq_ignore_ascii_case(tok))
            .count() as u64,
    }
}

/// Clean a display name down to a safe, regex-matchable "first name" token.
/// Picks the first whitespace-delimited token and strips everything but
/// alphanumerics and apostrophes (so we never regex-inject). Returns `None`
/// for names that don't survive cleaning (empty, or under 2 chars — single
/// letters cause too many false-positive matches to be useful).
pub(crate) fn clean_first_name_token(name: &str) -> Option<String> {
    let t = name.trim();
    if t.is_empty() {
        return None;
    }
    let first = t.split_whitespace().next().unwrap_or(t);
    let safe: String = first
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '\'')
        .collect();
    if safe.len() < 2 {
        None
    } else {
        Some(safe)
    }
}

/// Build a single OR'd word-boundary regex from the supplied other-names list.
/// Returns None if the list is empty (skip allocation).
pub(crate) fn build_other_names_regex(names: &[String]) -> Option<Regex> {
    let cleaned: Vec<String> = names
        .iter()
        .filter_map(|n| clean_first_name_token(n))
        .map(|safe| regex::escape(&safe))
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

    // ---------------------------------------------------------------
    // compute_focus_shared: shared regex + self-exclusion
    // ---------------------------------------------------------------

    #[test]
    fn shared_regex_matches_compute_focus_when_no_self_exclusion_needed() {
        // A shared regex built over *all* contacts (Anna, Justin) should
        // produce identical output to the per-contact `compute_focus` call
        // when the current contact's own name isn't in the candidate set.
        let messages = vec![
            am(1, Participant::Me, "Anna and I are heading out"),
            am(2, Participant::Them, "tell Justin hi"),
        ];
        let names = vec!["Anna".to_string(), "Justin".to_string()];
        let expected = compute_focus(&messages, &names);

        let shared_re = build_other_names_regex(&names);
        let actual = compute_focus_shared(&messages, shared_re.as_ref(), None);

        assert_eq!(actual.focus_me_pct, expected.focus_me_pct);
        assert_eq!(actual.focus_them_pct, expected.focus_them_pct);
        assert_eq!(actual.focus_other_pct, expected.focus_other_pct);
    }

    #[test]
    fn shared_regex_excludes_self_mentions() {
        // The shared regex is built from EVERY contact's name, including the
        // contact whose thread we're currently scoring. A mention of that
        // contact's own name must NOT count as an "other" mention — it must
        // match `compute_focus` called with a names list that omits self.
        let messages = vec![
            am(1, Participant::Me, "Anna, are you free tonight?"),
            am(2, Participant::Them, "tell Justin hi"),
        ];
        // Shared regex built from ALL contacts, including "Anna" (the
        // contact under analysis) and "Justin" (a genuine other contact).
        let all_names = vec!["Anna".to_string(), "Justin".to_string()];
        let shared_re = build_other_names_regex(&all_names);

        // Analyzing Anna's own thread: self-exclude "anna".
        let with_exclusion = compute_focus_shared(&messages, shared_re.as_ref(), Some("anna"));

        // Equivalent to calling compute_focus with Anna already excluded
        // from the candidate list (the pre-fix, per-contact-query behavior).
        let other_names_excluding_self = vec!["Justin".to_string()];
        let expected = compute_focus(&messages, &other_names_excluding_self);

        assert_eq!(with_exclusion.focus_other_pct, expected.focus_other_pct);
        assert_eq!(with_exclusion.focus_me_pct, expected.focus_me_pct);
        assert_eq!(with_exclusion.focus_them_pct, expected.focus_them_pct);

        // Sanity: without self-exclusion, "Anna" mention would have counted
        // too, producing a strictly larger other-bucket.
        let without_exclusion = compute_focus_shared(&messages, shared_re.as_ref(), None);
        assert!(without_exclusion.focus_other_pct > with_exclusion.focus_other_pct);
    }

    #[test]
    fn shared_regex_self_exclusion_is_case_insensitive() {
        // "I" guarantees a non-zero total so `focus_other_pct == 0.0` proves
        // the ANNA/anna hit was actually excluded, not just an empty result.
        let messages = vec![am(1, Participant::Me, "I think ANNA is the best")];
        let names = vec!["Anna".to_string()];
        let shared_re = build_other_names_regex(&names);
        let out = compute_focus_shared(&messages, shared_re.as_ref(), Some("anna"));
        assert_eq!(out.focus_other_pct, 0.0);
        assert_eq!(out.focus_me_pct, 1.0);
    }
}
