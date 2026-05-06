//! Topic clustering — TF-IDF distinctive phrases.
//!
//! Goal: surface the **distinctive vocabulary** of a contact pair —
//! phrases they use disproportionately compared to the user's other
//! conversations. These tend to map to topics, in-jokes, shared interests,
//! and recurring conversational themes.
//!
//! # Why TF-IDF and not embedding clustering
//!
//! Embedding-based clustering (k-means over per-message vectors) is more
//! "correct" but requires every message to have an embedding row in the
//! `embeddings` table — which means the user has to run the embedding
//! pipeline first. TF-IDF works on raw bodies, has no dep on a pre-pipeline,
//! and produces interpretable phrase-level results that are arguably
//! more useful for human-readable topics.
//!
//! Future enhancement: optional embedding-based mode that activates when
//! embeddings are available, doing a k-means pass and labeling each cluster
//! by its top TF-IDF phrases.
//!
//! # Algorithm
//!
//! 1. Tokenize every message body (lowercase alphabetics only).
//! 2. Generate bigrams + trigrams; skip stopword-laced ones.
//! 3. Count term frequency in **this pair's** corpus.
//! 4. Count term frequency in a **global background** corpus
//!    (provided by caller — typically a sample from other contacts).
//! 5. Compute TF-IDF: `tf_pair * log((global_total_messages + 1) / (df_global + 1))`
//! 6. Filter: phrase must occur ≥ `min_pair_count` times in the pair (so
//!    we don't surface vanity-rare phrases).
//! 7. Sort by TF-IDF score desc, take top N.

use crate::aggregator::AggregatorMessage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// English stopwords reused from the inside-jokes module's logic. Keeping
/// a copy here so the two modules stay independent — they're allowed to
/// drift if their vocabulary needs differ.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "has",
    "have", "had", "he", "her", "him", "his", "how", "i", "if", "in", "is", "it",
    "its", "i'm", "i'll", "i've", "i'd", "me", "my", "no", "not", "of", "on", "or",
    "she", "so", "that", "the", "their", "them", "then", "there", "these", "they",
    "this", "to", "too", "was", "we", "were", "what", "when", "where", "which",
    "who", "why", "will", "with", "would", "you", "your", "yours", "you're",
    "you'll", "you've", "you'd", "u", "ur", "yeah", "yes", "yep", "yup", "ok",
    "okay", "oh", "uh", "um", "well", "just", "really", "very", "much", "lol",
    "lmao", "haha", "hahaha", "hi", "hey", "hello", "bye", "good", "got", "get",
    "go", "gonna", "going", "do", "does", "did", "done", "doing", "can", "could",
    "should", "would", "may", "might", "must", "let", "lets", "let's", "tell",
    "told", "say", "said", "see", "saw", "look", "know", "knew", "want", "need",
    "think", "thought", "make", "made", "take", "took", "give", "gave", "come",
    "came", "way", "thing", "things", "time", "day", "today", "tomorrow",
    "yesterday", "now", "later", "tonight", "morning", "night", "right",
    "wrong", "back", "out", "up", "down", "off", "over", "into", "after",
    "before", "again", "still", "even", "only", "also", "more", "most", "some",
    "any", "all", "every", "each", "other", "both", "few", "many", "such",
    "same", "own", "than", "next", "first", "last", "long", "new", "old",
    "big", "small", "little", "high", "low", "lot", "lots", "always", "never",
    "sometimes", "often", "usually", "almost",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicPhrase {
    pub phrase: String,
    pub pair_count: u32,
    /// TF-IDF score (relative; bigger = more distinctive to this pair).
    pub score: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct TopicsConfig {
    pub min_pair_count: u32,
    pub min_token_len: usize,
    pub top_n: usize,
}

impl Default for TopicsConfig {
    fn default() -> Self {
        Self {
            min_pair_count: 3,
            min_token_len: 3,
            top_n: 30,
        }
    }
}

/// Compute distinctive phrases for a pair. `global_phrases` should be a
/// pre-built phrase-frequency map sampled from messages OUTSIDE the pair —
/// the caller (orchestrator) builds it once per analytics run and shares it
/// across all topic computations on that DB.
pub fn compute_topics(
    pair_messages: &[AggregatorMessage],
    global_phrases: &HashMap<String, u32>,
    global_message_count: u32,
    config: &TopicsConfig,
) -> Vec<TopicPhrase> {
    if pair_messages.is_empty() {
        return Vec::new();
    }
    let pair_phrases = build_phrase_counts(
        pair_messages.iter().map(|m| m.body.as_str()),
        config.min_token_len,
    );
    if pair_phrases.is_empty() {
        return Vec::new();
    }

    let bg_total = (global_message_count.max(1)) as f64;
    let mut scored: Vec<TopicPhrase> = pair_phrases
        .into_iter()
        .filter(|(_, count)| *count >= config.min_pair_count)
        .map(|(phrase, pair_count)| {
            let df_global = *global_phrases.get(&phrase).unwrap_or(&0) as f64;
            let idf = ((bg_total + 1.0) / (df_global + 1.0)).ln();
            let tf = pair_count as f64;
            let score = tf * idf;
            TopicPhrase {
                phrase,
                pair_count,
                score,
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.phrase.cmp(&b.phrase))
    });
    scored.truncate(config.top_n);
    scored
}

/// Tally bigrams + trigrams across an iterator of bodies (used for both the
/// pair's own corpus and the global background corpus). Stopword-containing
/// n-grams are skipped to avoid surfacing "in the" / "and the" / etc.
pub fn build_phrase_counts<'a>(
    bodies: impl IntoIterator<Item = &'a str>,
    min_token_len: usize,
) -> HashMap<String, u32> {
    let stops: std::collections::HashSet<&str> = STOPWORDS.iter().copied().collect();
    let mut counts: HashMap<String, u32> = HashMap::new();
    for body in bodies {
        let tokens: Vec<String> = body
            .split_whitespace()
            .map(|w| {
                w.chars()
                    .filter(|c| c.is_alphabetic() || *c == '\'')
                    .collect::<String>()
                    .to_lowercase()
            })
            .filter(|t| t.len() >= min_token_len && !t.chars().all(|c| c.is_numeric()))
            .collect();
        for window in tokens.windows(2) {
            if window.iter().any(|t| stops.contains(t.as_str())) {
                continue;
            }
            *counts.entry(window.join(" ")).or_insert(0) += 1;
        }
        if tokens.len() >= 3 {
            for window in tokens.windows(3) {
                if window.iter().any(|t| stops.contains(t.as_str())) {
                    continue;
                }
                *counts.entry(window.join(" ")).or_insert(0) += 1;
            }
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Participant;

    fn am(body: &str) -> AggregatorMessage {
        AggregatorMessage {
            db_rowid: 1,
            timestamp_ms: 0,
            sender: Participant::Me,
            body: body.to_string(),
            mime_types: Vec::new(),
        }
    }

    #[test]
    fn empty_pair_returns_empty() {
        let out = compute_topics(&[], &HashMap::new(), 0, &TopicsConfig::default());
        assert!(out.is_empty());
    }

    #[test]
    fn distinctive_phrases_outrank_common_ones() {
        let pair = vec![
            am("rocket science again"),
            am("more rocket science"),
            am("rocket science talk"),
            am("rocket science conclusions"),
        ];
        // "rocket science" appears 4 times in the pair. Background says it's
        // common in 100 of 1000 other messages — still highly distinctive.
        let mut bg = HashMap::new();
        bg.insert("rocket science".to_string(), 100);
        bg.insert("hello there".to_string(), 800); // very common globally
        let out = compute_topics(&pair, &bg, 1000, &TopicsConfig::default());
        assert!(out.iter().any(|t| t.phrase == "rocket science"));
    }

    #[test]
    fn ignores_phrases_below_min_count() {
        let pair = vec![am("famous purple rocket once")];
        let bg: HashMap<String, u32> = HashMap::new();
        let out = compute_topics(&pair, &bg, 100, &TopicsConfig::default());
        assert!(out.is_empty(), "single-occurrence phrase should be filtered");
    }

    #[test]
    fn truncates_to_top_n() {
        // 50 distinct repeated bigrams.
        let mut messages = Vec::new();
        for i in 0..50 {
            for _ in 0..4 {
                messages.push(am(&format!("alphax{} betax{}", i, i)));
            }
        }
        let bg: HashMap<String, u32> = HashMap::new();
        let mut config = TopicsConfig::default();
        config.top_n = 5;
        let out = compute_topics(&messages, &bg, 100, &config);
        assert_eq!(out.len(), 5);
    }
}
