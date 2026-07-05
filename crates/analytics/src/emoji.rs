//! Grapheme-cluster-aware emoji extraction.
//!
//! Counting emoji is harder than it looks. A single visually-distinct emoji
//! can be encoded as:
//! - **One code point**, e.g. `😂` (U+1F602)
//! - **Base + variation selector**, e.g. `❤️` (U+2764 U+FE0F) — the FE0F
//!   forces emoji presentation
//! - **Modifier sequence**, e.g. `👍🏻` (👍 + 🏻 skin tone)
//! - **ZWJ sequence**, e.g. `👨‍👩‍👧‍👦` (multiple emoji glued by U+200D)
//! - **Regional indicator pair**, e.g. `🇺🇸` (U+1F1FA U+1F1F8) → US flag
//!
//! All of these render as one glyph and should count as ONE emoji each. The
//! `unicode-segmentation` crate's grapheme-cluster iteration handles every
//! case above for free, so we walk graphemes and check whether each cluster
//! contains any emoji-property code points.
//!
//! The output is keyed by the cluster string itself, so 👨‍👩‍👧‍👦 stays one
//! key (not split into its components).

use std::collections::HashMap;
use unic_emoji_char::{is_emoji, is_emoji_modifier_base};
use unicode_segmentation::UnicodeSegmentation;

/// Walk `body`'s grapheme clusters and count emoji-bearing ones.
///
/// Returns a map of `cluster_string → count`. Order is undefined; use
/// [`top_emojis`] to get a sorted top-N view.
///
/// "Emoji-bearing" means the cluster contains at least one code point with
/// the `Emoji` Unicode property AND is not a plain ASCII digit (digits have
/// the Emoji property because of keycap sequences like `1️⃣`, but a bare `1`
/// is not what people mean by emoji).
pub fn extract_emojis(body: &str) -> HashMap<String, u32> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for grapheme in body.graphemes(true) {
        if cluster_is_emoji(grapheme) {
            *counts.entry(grapheme.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

/// Return the top `n` emoji from a count map, sorted by count descending.
/// Ties are broken by the emoji's bytes (deterministic, but visually arbitrary).
pub fn top_emojis(counts: &HashMap<String, u32>, n: usize) -> Vec<EmojiCount> {
    let mut entries: Vec<EmojiCount> = counts
        .iter()
        .map(|(emoji, count)| EmojiCount {
            emoji: emoji.clone(),
            count: *count,
        })
        .collect();
    entries.sort_unstable_by(|a, b| b.count.cmp(&a.count).then_with(|| a.emoji.cmp(&b.emoji)));
    entries.truncate(n);
    entries
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmojiCount {
    pub emoji: String,
    pub count: u32,
}

/// True iff a grapheme cluster should be counted as an emoji.
///
/// Rules:
/// 1. If the cluster contains any code point classified as an emoji modifier
///    base (e.g. 👍 with optional skin tone) → count it.
/// 2. If the cluster contains any code point that's either an Emoji per
///    `unic-emoji-char` OR one of the well-known emoji-cluster special code
///    points (see [`is_emoji_cluster_special`]) AND is outside ASCII → count it.
///
/// The non-ASCII filter (`>= 0x80`) keeps plain digits and `#`/`*` from
/// counting as emoji on their own — those code points have the Emoji
/// property only because they participate in keycap sequences, but a bare
/// "1" in a sentence is not what users mean by emoji.
///
/// We supplement `unic-emoji-char` with a hardcoded set of cluster-special
/// code points because the crate's data tables (Unicode 12) classify some
/// keycap and joiner code points as components only, and the version of the
/// crate's `is_emoji_component` predicate doesn't reliably catch them. The
/// special-set ranges below are stable Unicode assignments that have not
/// changed since their introduction.
fn cluster_is_emoji(cluster: &str) -> bool {
    let mut has_emoji_modifier_base = false;
    let mut has_qualifying_codepoint = false;
    let mut regional_indicators = 0usize;
    let mut non_regional_qualifier = false;

    for c in cluster.chars() {
        // VS15 explicitly requests TEXT presentation (the inverse of VS16) —
        // "❤︎" is deliberately not an emoji.
        if c as u32 == 0xFE0E {
            return false;
        }
        if is_emoji_modifier_base(c) {
            has_emoji_modifier_base = true;
        }
        if (0x1F1E6..=0x1F1FF).contains(&(c as u32)) {
            regional_indicators += 1;
            continue;
        }
        if (c as u32) >= 0x80 && (is_emoji(c) || is_emoji_cluster_special(c)) {
            has_qualifying_codepoint = true;
            non_regional_qualifier = true;
        }
    }

    // Regional indicators only count when paired (two form a flag); a lone,
    // truncated indicator letter is not an emoji.
    if regional_indicators >= 2 {
        has_qualifying_codepoint = true;
    } else if regional_indicators == 1 && !non_regional_qualifier && !has_emoji_modifier_base {
        return false;
    }

    has_emoji_modifier_base || has_qualifying_codepoint
}

/// Code points that are *part* of an emoji cluster but may not register as
/// "emoji" via `unic-emoji-char::is_emoji`. Hardcoded because Unicode has
/// kept these stable since they were assigned, and we don't want to depend
/// on the crate's version-specific data tables for anything load-bearing.
fn is_emoji_cluster_special(c: char) -> bool {
    matches!(
        c as u32,
        0x200D                  // ZWJ — joins ZWJ sequences (👨‍👩‍👧‍👦)
        | 0x20E3                // combining enclosing keycap — the visible square in 1️⃣
        | 0xFE0F                // variation selector-16 — forces emoji presentation
        | 0x1F3FB..=0x1F3FF // skin-tone modifiers
    )
    // Regional indicators (0x1F1E6..=0x1F1FF) are handled directly in
    // cluster_is_emoji — they only qualify when paired into a flag.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_body_yields_empty_map() {
        let counts = extract_emojis("");
        assert!(counts.is_empty());
    }

    #[test]
    fn paired_flag_counts_but_lone_regional_indicator_does_not() {
        let counts = extract_emojis("go \u{1F1FA}\u{1F1F8} team");
        assert_eq!(counts.get("\u{1F1FA}\u{1F1F8}"), Some(&1));
        // A truncated, unpaired indicator letter is not an emoji.
        let counts = extract_emojis("broken \u{1F1FA} flag");
        assert!(counts.is_empty());
    }

    #[test]
    fn text_presentation_selector_is_not_an_emoji() {
        // U+FE0E explicitly requests text rendering; U+FE0F requests emoji.
        assert!(extract_emojis("\u{2764}\u{FE0E}").is_empty());
        assert_eq!(extract_emojis("\u{2764}\u{FE0F}").len(), 1);
    }

    #[test]
    fn plain_text_yields_empty_map() {
        let counts = extract_emojis("hello world this has no emoji");
        assert!(counts.is_empty());
    }

    #[test]
    fn single_codepoint_emoji_counted_once() {
        let counts = extract_emojis("hi 😂");
        assert_eq!(counts.get("😂"), Some(&1));
        assert_eq!(counts.len(), 1);
    }

    #[test]
    fn repeated_emoji_counted() {
        let counts = extract_emojis("😂😂😂 lol 😂");
        assert_eq!(counts.get("😂"), Some(&4));
    }

    #[test]
    fn mixed_emoji_independently_counted() {
        let counts = extract_emojis("🤣 then 💀💀 and a 😂");
        assert_eq!(counts.get("🤣"), Some(&1));
        assert_eq!(counts.get("💀"), Some(&2));
        assert_eq!(counts.get("😂"), Some(&1));
    }

    #[test]
    fn skin_tone_modifier_counts_as_one_cluster() {
        // "👍🏻" is U+1F44D (👍) + U+1F3FB (🏻 light skin tone).
        // Single grapheme cluster.
        let counts = extract_emojis("nice 👍🏻 work");
        // Either ‘👍🏻’ as one cluster, or ‘👍’ and ‘🏻’ — depends on Unicode tables.
        // We just need exactly one entry summing to 1 representing this whole emoji.
        let total: u32 = counts.values().sum();
        assert_eq!(
            total, 1,
            "skin-toned thumbs-up should count exactly once, got {:?}",
            counts
        );
    }

    #[test]
    fn zwj_family_emoji_counts_as_one() {
        // 👨‍👩‍👧‍👦 = man + ZWJ + woman + ZWJ + girl + ZWJ + boy. One grapheme cluster.
        let counts = extract_emojis("family 👨‍👩‍👧‍👦 portrait");
        let total: u32 = counts.values().sum();
        assert_eq!(total, 1, "ZWJ family should count as one, got {:?}", counts);
    }

    #[test]
    fn regional_indicator_flag_counts_as_one() {
        // 🇺🇸 = U+1F1FA + U+1F1F8 (regional indicators U + S). One cluster.
        let counts = extract_emojis("from the 🇺🇸 today");
        let total: u32 = counts.values().sum();
        assert_eq!(total, 1, "flag should count as one, got {:?}", counts);
    }

    #[test]
    fn ascii_digits_are_not_emoji() {
        let counts = extract_emojis("number 1 and 2 and 3");
        assert!(
            counts.is_empty(),
            "bare ASCII digits must not count as emoji"
        );
    }

    #[test]
    fn keycap_sequence_does_count() {
        // "1️⃣" = '1' + U+FE0F + U+20E3. Built from explicit escapes so this
        // test is robust against editor / source-file encoding quirks.
        let s = "press \u{0031}\u{FE0F}\u{20E3} for english";
        let counts = extract_emojis(s);
        let total: u32 = counts.values().sum();
        assert_eq!(total, 1, "keycap sequence should count, got {:?}", counts);
    }

    #[test]
    fn cluster_special_codepoints_classified() {
        // Direct sanity check on the hardcoded special set. If this passes but
        // keycap_sequence_does_count fails, the bug is in grapheme segmentation,
        // not in classification.
        assert!(
            is_emoji_cluster_special('\u{200D}'),
            "ZWJ should be cluster-special"
        );
        assert!(
            is_emoji_cluster_special('\u{20E3}'),
            "combining keycap should be cluster-special"
        );
        assert!(
            is_emoji_cluster_special('\u{FE0F}'),
            "VS-16 should be cluster-special"
        );
        // Regional indicators are deliberately NOT cluster-special — they
        // are handled in cluster_is_emoji and only count when paired into a
        // flag (a lone indicator letter is not an emoji).
        assert!(!is_emoji_cluster_special('\u{1F1E6}'));
        assert!(!is_emoji_cluster_special('\u{1F1FF}'));
        assert!(
            is_emoji_cluster_special('\u{1F3FB}'),
            "skin tone-1 should be cluster-special"
        );
        assert!(
            is_emoji_cluster_special('\u{1F3FF}'),
            "skin tone-5 should be cluster-special"
        );
        // And things that should NOT be cluster-special:
        assert!(!is_emoji_cluster_special('a'));
        assert!(!is_emoji_cluster_special('1'));
        assert!(
            !is_emoji_cluster_special('\u{1F602}'),
            "😂 is a regular emoji, not a special cluster element"
        );
    }

    #[test]
    fn top_emojis_sorts_descending_with_truncation() {
        let mut counts = HashMap::new();
        counts.insert("😂".to_string(), 100);
        counts.insert("🤣".to_string(), 50);
        counts.insert("💀".to_string(), 200);
        counts.insert("😅".to_string(), 30);

        let top2 = top_emojis(&counts, 2);
        assert_eq!(top2.len(), 2);
        assert_eq!(top2[0].emoji, "💀");
        assert_eq!(top2[0].count, 200);
        assert_eq!(top2[1].emoji, "😂");
        assert_eq!(top2[1].count, 100);
    }

    #[test]
    fn top_emojis_handles_n_larger_than_map() {
        let mut counts = HashMap::new();
        counts.insert("😂".to_string(), 5);
        let top = top_emojis(&counts, 100);
        assert_eq!(top.len(), 1);
    }
}
