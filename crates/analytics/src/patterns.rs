//! Regex pattern detectors for language features (laughs, apologies, questions,
//! encouragement, links).
//!
//! Each detector is a single boolean predicate over a message body. The
//! aggregator calls these once per message; over millions of messages, the
//! cost of regex *compilation* would dominate, so we compile once via
//! `std::sync::LazyLock` and reuse the compiled `Regex` for every call.
//!
//! Patterns are intentionally permissive over precise — false positives are
//! cheap, false negatives miss interesting signal. The dashboard already
//! contextualizes counts with side-by-side comparisons, so a small constant
//! over- or under-count washes out.

use regex::Regex;
use std::sync::LazyLock;

// =========================================================================
// Laugh detection.
// Catches: lol / lool / loooool, lmao, lmfao, rofl, haha (any length),
// 😂 face with tears of joy, 🤣 ROFL emoji, 💀 skull (Gen-Z "I'm dead").
// Word boundaries are tricky because emoji aren't word characters; for them
// we don't anchor with \b.
// =========================================================================
static LAUGH_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Repetition forms tolerate odd lengths and casual suffixes: "hahah",
    // "heheh", "lolol", "loll", "lolz", "lmaooo" are all real-world laughs
    // that the strict even-repeat forms (`ha(?:ha)+` etc.) rejected because
    // the stray trailing letter broke the `\b`.
    Regex::new(
        r"(?i)\b(l(?:o+l)+l*z*|lmf?ao+|rofl|ha(?:ha)+h?|he(?:he)+h?)\b|[\u{1F602}\u{1F923}\u{1F480}]",
    )
    .unwrap()
});

pub fn is_laugh(body: &str) -> bool {
    LAUGH_RE.is_match(body)
}

// =========================================================================
// Apology detection.
// Catches: sorry (any case), my bad, apologi[sz]e, my fault, forgive me.
// =========================================================================
static APOLOGY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(sorry|my bad|apologi[sz]e|apologi[sz]ed|my fault|forgive me)\b").unwrap()
});

pub fn is_apology(body: &str) -> bool {
    APOLOGY_RE.is_match(body)
}

// =========================================================================
// Question detection.
// Strategy: trailing '?' is the dominant signal in casual texting and is
// extremely fast to check. Anchored at the trimmed end so trailing emoji
// don't cause false negatives ("are you sure? 😬" → "are you sure?" once
// emoji are stripped). For simplicity we don't strip emoji here — the
// trailing '?' before emoji is more common than after, and we accept the
// occasional miss.
// =========================================================================
pub fn is_question(body: &str) -> bool {
    // Strip trailing whitespace then look for '?'. Doesn't handle "are you sure?😬"
    // (where ? is mid-string), but matches mimoto's expected behavior.
    let trimmed = body.trim_end();
    trimmed.ends_with('?')
}

// =========================================================================
// Encouragement detection.
// Catches recognizable phrases: "you got this", "proud of you", "you can do it",
// "good job", "great job", "well done", "nice work", "keep it up", "amazing",
// "you're awesome", "let's go", "hell yeah".
// =========================================================================
static ENCOURAGEMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(you got this|proud of you|believe in you|you can do it|good job|great job|well done|nice work|keep it up|keep going|you'?re amazing|you'?re awesome|that'?s amazing|that'?s awesome|hell yeah|let'?s go|atta (?:boy|girl))\b"
    ).unwrap()
});

pub fn is_encouragement(body: &str) -> bool {
    ENCOURAGEMENT_RE.is_match(body)
}

// =========================================================================
// Link detection.
// Catches: http(s)://..., www...., common-TLD bare links (slightly riskier).
// Bare-TLD detection is omitted to avoid false positives on "anything.com"
// that happens to be a sentence ending. We only fire on http/https/www
// prefixes — strict but defensible.
// =========================================================================
static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:https?://|www\.)\S+").unwrap());

pub fn contains_link(body: &str) -> bool {
    LINK_RE.is_match(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- laughs ----------
    #[test]
    fn laughs_matches_common_forms() {
        assert!(is_laugh("lol"));
        assert!(is_laugh("LOL!"));
        assert!(is_laugh("loool that's wild"));
        assert!(is_laugh("lmao"));
        assert!(is_laugh("lmfao that's nuts"));
        assert!(is_laugh("rofl"));
        assert!(is_laugh("haha what"));
        assert!(is_laugh("hahaha"));
        assert!(is_laugh("hahahaha"));
        assert!(is_laugh("😂"));
        assert!(is_laugh("dying 🤣"));
        assert!(is_laugh("💀💀💀"));
    }

    #[test]
    fn laughs_matches_odd_length_and_suffixed_forms() {
        assert!(is_laugh("hahah"));
        assert!(is_laugh("hahahah"));
        assert!(is_laugh("heheh"));
        assert!(is_laugh("lolol"));
        assert!(is_laugh("loll"));
        assert!(is_laugh("lolz"));
        assert!(is_laugh("lmaooo"));
    }

    #[test]
    fn laughs_does_not_match_random_text() {
        assert!(!is_laugh("hello"));
        assert!(!is_laugh("solo trip")); // contains "lo" but not "lol"
        assert!(!is_laugh("ha")); // single "ha" — not enough
        assert!(!is_laugh(""));
        assert!(!is_laugh("rolling")); // contains "rol" but not "rofl"
    }

    // ---------- apologies ----------
    #[test]
    fn apologies_matches_common_forms() {
        assert!(is_apology("sorry about that"));
        assert!(is_apology("Sorry"));
        assert!(is_apology("SO SORRY"));
        assert!(is_apology("my bad"));
        assert!(is_apology("I apologize for the delay"));
        assert!(is_apology("I apologise for being late")); // British
        assert!(is_apology("my fault"));
        assert!(is_apology("forgive me"));
    }

    #[test]
    fn apologies_does_not_match_neutral_text() {
        assert!(!is_apology("hello there"));
        assert!(!is_apology("sorrows are heavy")); // contains "sorrow" not "sorry"
        assert!(!is_apology(""));
    }

    // ---------- questions ----------
    #[test]
    fn questions_match_trailing_question_mark() {
        assert!(is_question("are you there?"));
        assert!(is_question("what?"));
        assert!(is_question("really?  ")); // trailing whitespace OK
    }

    #[test]
    fn questions_do_not_match_without_trailing_qm() {
        assert!(!is_question("are you there"));
        assert!(!is_question("?what")); // ? not at end
        assert!(!is_question(""));
    }

    // ---------- encouragement ----------
    #[test]
    fn encouragement_matches_common_phrases() {
        assert!(is_encouragement("you got this!"));
        assert!(is_encouragement("I'm proud of you"));
        assert!(is_encouragement("great job today"));
        assert!(is_encouragement("nice work on that"));
        assert!(is_encouragement("keep it up"));
        assert!(is_encouragement("you can do it"));
        assert!(is_encouragement("hell yeah"));
        assert!(is_encouragement("let's go"));
        assert!(is_encouragement("lets go"));
        assert!(is_encouragement("atta boy"));
        assert!(is_encouragement("you're amazing"));
    }

    #[test]
    fn encouragement_does_not_match_neutral_text() {
        assert!(!is_encouragement("ok"));
        assert!(!is_encouragement("amazing weather")); // "amazing" alone isn't enough
    }

    // ---------- links ----------
    #[test]
    fn links_matches_http_https_www() {
        assert!(contains_link("check this https://example.com/path"));
        assert!(contains_link("http://foo.bar/x"));
        assert!(contains_link("see www.google.com"));
        assert!(contains_link("HTTPS://uppercase.example/path"));
    }

    #[test]
    fn links_does_not_match_bare_tld() {
        assert!(!contains_link("just google it"));
        assert!(!contains_link("foo.com without prefix")); // strict by design
        assert!(!contains_link(""));
    }
}
