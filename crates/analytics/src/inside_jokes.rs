//! Inside-jokes detection.
//!
//! For a contact pair, find n-gram phrases (2-3 words) that recur often in
//! their messages but contain no generic stopwords. These are essentially
//! the pair's idiomatic vocabulary — running gags, shared references,
//! pet names, recurring topics.
//!
//! # Algorithm
//!
//! 1. Tokenize every message body (lowercase, alpha-only).
//! 2. Generate bigrams and trigrams from each message.
//! 3. Skip n-grams that contain ANY token from the stopword list (the/and/etc).
//! 4. Skip n-grams where any token is too short (<3 chars) or numeric.
//! 5. Count occurrences across all messages.
//! 6. Filter by minimum count (default 3).
//! 7. Sort by count desc, take top N (default 20).
//!
//! # Caveats
//!
//! Without comparison against the global corpus we can't tell "phrases this
//! pair uses uniquely" from "phrases everyone uses". The stopword filter
//! takes a big bite out of the false-positive set (kills "and the", "in the",
//! "a lot", etc.) but generic phrases like "this morning" can still make
//! the cut. Comparing against a global corpus is a future enhancement.

use crate::aggregator::AggregatorMessage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// English stopwords — we skip any n-gram that contains one of these.
const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "are",
    "as",
    "at",
    "be",
    "but",
    "by",
    "for",
    "from",
    "has",
    "have",
    "had",
    "he",
    "her",
    "him",
    "his",
    "how",
    "i",
    "if",
    "in",
    "is",
    "it",
    "its",
    "i'm",
    "i'll",
    "i've",
    "i'd",
    "me",
    "my",
    "no",
    "not",
    "of",
    "on",
    "or",
    "she",
    "so",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "to",
    "too",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
    "will",
    "with",
    "would",
    "you",
    "your",
    "yours",
    "you're",
    "you'll",
    "you've",
    "you'd",
    "u",
    "ur",
    "yeah",
    "yes",
    "yep",
    "yup",
    "ok",
    "okay",
    "oh",
    "uh",
    "um",
    "well",
    "just",
    "really",
    "very",
    "much",
    "lol",
    "lmao",
    "haha",
    "hahaha",
    "hi",
    "hey",
    "hello",
    "bye",
    "good",
    "got",
    "get",
    "go",
    "gonna",
    "going",
    "do",
    "does",
    "did",
    "done",
    "doing",
    "can",
    "could",
    "should",
    "would",
    "may",
    "might",
    "must",
    "let",
    "lets",
    "let's",
    "tell",
    "told",
    "say",
    "said",
    "see",
    "saw",
    "look",
    "know",
    "knew",
    "want",
    "need",
    "think",
    "thought",
    "make",
    "made",
    "take",
    "took",
    "give",
    "gave",
    "come",
    "came",
    "way",
    "thing",
    "things",
    "time",
    "day",
    "today",
    "tomorrow",
    "yesterday",
    "now",
    "later",
    "tonight",
    "morning",
    "night",
    "right",
    "wrong",
    "back",
    "out",
    "up",
    "down",
    "off",
    "over",
    "into",
    "after",
    "before",
    "again",
    "still",
    "even",
    "only",
    "also",
    "more",
    "most",
    "some",
    "any",
    "all",
    "every",
    "each",
    "other",
    "both",
    "few",
    "many",
    "most",
    "much",
    "such",
    "same",
    "own",
    "than",
    "next",
    "first",
    "last",
    "long",
    "new",
    "old",
    "big",
    "small",
    "little",
    "high",
    "low",
    "lot",
    "lots",
    "always",
    "never",
    "sometimes",
    "often",
    "usually",
    "almost",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsideJoke {
    pub phrase: String,
    pub count: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct InsideJokesConfig {
    /// Minimum n-gram occurrence count to qualify. Default 3.
    pub min_count: u32,
    /// Maximum number of jokes to return. Default 20.
    pub top_n: usize,
    /// Minimum length per token. Default 3 (skips "yo", "wp", "wd").
    pub min_token_len: usize,
}

impl Default for InsideJokesConfig {
    fn default() -> Self {
        Self {
            min_count: 3,
            top_n: 20,
            min_token_len: 3,
        }
    }
}

/// Find recurring 2- and 3-word phrases in this pair's messages.
pub fn detect_inside_jokes(
    messages: &[AggregatorMessage],
    config: &InsideJokesConfig,
) -> Vec<InsideJoke> {
    if messages.is_empty() {
        return Vec::new();
    }
    let stops: std::collections::HashSet<&str> = STOPWORDS.iter().copied().collect();
    let mut counts: HashMap<String, u32> = HashMap::new();

    for msg in messages {
        let tokens = tokenize(&msg.body, config.min_token_len);
        if tokens.len() < 2 {
            continue;
        }
        // Bigrams.
        for window in tokens.windows(2) {
            if window.iter().any(|t| stops.contains(t.as_str())) {
                continue;
            }
            let phrase = window.join(" ");
            *counts.entry(phrase).or_insert(0) += 1;
        }
        // Trigrams.
        if tokens.len() >= 3 {
            for window in tokens.windows(3) {
                if window.iter().any(|t| stops.contains(t.as_str())) {
                    continue;
                }
                let phrase = window.join(" ");
                *counts.entry(phrase).or_insert(0) += 1;
            }
        }
    }

    let mut filtered: Vec<InsideJoke> = counts
        .into_iter()
        .filter(|(_, c)| *c >= config.min_count)
        .map(|(phrase, count)| InsideJoke { phrase, count })
        .collect();

    // Sort by count desc, then phrase asc for determinism.
    filtered.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.phrase.cmp(&b.phrase)));
    filtered.truncate(config.top_n);
    filtered
}

fn tokenize(body: &str, min_len: usize) -> Vec<String> {
    body.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|t| t.len() >= min_len && !t.chars().all(|c| c.is_numeric()))
        .collect()
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)] // fixture builders read better as assignments
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
    fn empty_input_returns_empty() {
        let out = detect_inside_jokes(&[], &InsideJokesConfig::default());
        assert!(out.is_empty());
    }

    #[test]
    fn detects_repeated_phrase() {
        let messages = vec![
            am("the secret garden was lovely"),
            am("we should visit secret garden again"),
            am("secret garden it is then"),
        ];
        let out = detect_inside_jokes(&messages, &InsideJokesConfig::default());
        assert!(out
            .iter()
            .any(|j| j.phrase == "secret garden" && j.count >= 3));
    }

    #[test]
    fn skips_phrases_with_stopwords() {
        let messages = vec![am("at the park"), am("at the park"), am("at the park")];
        let out = detect_inside_jokes(&messages, &InsideJokesConfig::default());
        // "at the" and "the park" both contain stopwords ("at", "the") → filtered.
        assert!(
            out.is_empty(),
            "expected stopwords to suppress, got {:?}",
            out
        );
    }

    #[test]
    fn ignores_phrases_below_threshold() {
        let messages = vec![am("rare phrase here only once")];
        let out = detect_inside_jokes(&messages, &InsideJokesConfig::default());
        assert!(out.is_empty());
    }

    #[test]
    fn truncates_to_top_n() {
        // Generate 25 distinct repeated phrases, request top 5.
        let mut messages = Vec::new();
        for i in 0..25 {
            for _ in 0..4 {
                messages.push(am(&format!("alphax{} betax{}", i, i)));
            }
        }
        let mut config = InsideJokesConfig::default();
        config.top_n = 5;
        let out = detect_inside_jokes(&messages, &config);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn counts_trigrams() {
        let messages = vec![
            am("famous purple rocket scientist"),
            am("famous purple rocket again"),
            am("third famous purple rocket"),
        ];
        let out = detect_inside_jokes(&messages, &InsideJokesConfig::default());
        assert!(out
            .iter()
            .any(|j| j.phrase == "famous purple rocket" && j.count >= 3));
    }
}
