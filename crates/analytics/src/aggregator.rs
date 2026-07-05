//! Single-pass aggregation over a contact's messages.
//!
//! Computes everything that's expressible as "walk every message once and
//! tally": per-side counts, daily / hourly buckets, emoji totals, pattern
//! flags, media-by-category breakdowns. The aggregator does NOT compute
//! response times or rating components — those need pre-segmented
//! conversations and live in their own modules.
//!
//! # Design
//!
//! The aggregator is a pure function over an in-memory message slice. It
//! produces an [`AggregatesOutput`] that the orchestrator writes to
//! `contact_analytics`, `activity_daily`, and `activity_hourly`.
//!
//! # Memory
//!
//! For one contact with 1M messages averaging ~10 words each, the unique-word
//! HashSet peaks at ~50k entries (~5MB). Emoji maps are tiny. Daily buckets
//! cap at ~4500 entries for a 12-year relationship. Hourly buckets are
//! bounded at 7 × 24 = 168 entries. Total worst-case footprint: < 20MB.

use crate::emoji::{extract_emojis, top_emojis, EmojiCount};
use crate::media::{classify_media, MediaCategory};
use crate::patterns::{contains_link, is_apology, is_encouragement, is_laugh, is_question};
use crate::types::Participant;
use chrono::{Datelike, FixedOffset, TimeZone, Timelike};
use std::collections::{HashMap, HashSet};

/// One message presented to the aggregator. Heavier than the segmenter's
/// `MessageRef` because we need the body and attachment metadata.
#[derive(Debug, Clone)]
pub struct AggregatorMessage {
    pub db_rowid: i64,
    pub timestamp_ms: i64,
    pub sender: Participant,
    pub body: String,
    /// MIME types of every attachment on this message. May be empty.
    pub mime_types: Vec<String>,
}

impl AggregatorMessage {
    fn has_media(&self) -> bool {
        !self.mime_types.is_empty()
    }
}

/// Top-level output of `compute_aggregates`. Fields map directly to schema
/// columns in `contact_analytics` plus the daily/hourly bucket tables.
#[derive(Debug, Clone, Default)]
pub struct AggregatesOutput {
    pub contact: ContactAggregates,
    pub daily: Vec<DailyBucket>,
    pub hourly: Vec<HourlyBucket>,
}

#[derive(Debug, Clone, Default)]
pub struct ContactAggregates {
    // Volume
    pub my_message_count: u32,
    pub their_message_count: u32,
    pub my_word_count: u64,
    pub their_word_count: u64,
    pub my_unique_word_count: u32,
    pub their_unique_word_count: u32,
    pub my_character_count: u64,
    pub their_character_count: u64,

    // Media
    pub my_image_count: u32,
    pub their_image_count: u32,
    pub my_video_count: u32,
    pub their_video_count: u32,
    pub my_audio_count: u32,
    pub their_audio_count: u32,
    pub my_gif_count: u32,
    pub their_gif_count: u32,
    pub my_link_count: u32,
    pub their_link_count: u32,

    // Language patterns
    pub my_emoji_total: u32,
    pub their_emoji_total: u32,
    pub my_top_emojis: Vec<EmojiCount>,
    pub their_top_emojis: Vec<EmojiCount>,
    pub my_laugh_count: u32,
    pub their_laugh_count: u32,
    pub my_apology_count: u32,
    pub their_apology_count: u32,
    pub my_question_count: u32,
    pub their_question_count: u32,
    pub my_encouragement_count: u32,
    pub their_encouragement_count: u32,
}

#[derive(Debug, Clone)]
pub struct DailyBucket {
    /// `YYYY-MM-DD` in the user's local timezone.
    pub day: String,
    pub my_messages: u32,
    pub their_messages: u32,
    pub my_words: u32,
    pub their_words: u32,
    pub my_media: u32,
    pub their_media: u32,
    // `my_points` and `their_points` are filled later by the scoring module.
}

#[derive(Debug, Clone)]
pub struct HourlyBucket {
    /// 0 = Sunday, 6 = Saturday (matches schema).
    pub day_of_week: u8,
    /// 0-23 in the user's local timezone.
    pub hour: u8,
    pub message_count: u32,
}

/// Per-side mutable accumulator. Internal helper.
struct SideAccum {
    message_count: u32,
    word_count: u64,
    char_count: u64,
    unique_words: HashSet<String>,

    image_count: u32,
    video_count: u32,
    audio_count: u32,
    gif_count: u32,
    link_count: u32,

    emoji_counts: HashMap<String, u32>,
    emoji_total: u32,
    laugh_count: u32,
    apology_count: u32,
    question_count: u32,
    encouragement_count: u32,
}

impl SideAccum {
    fn new() -> Self {
        Self {
            message_count: 0,
            word_count: 0,
            char_count: 0,
            unique_words: HashSet::new(),
            image_count: 0,
            video_count: 0,
            audio_count: 0,
            gif_count: 0,
            link_count: 0,
            emoji_counts: HashMap::new(),
            emoji_total: 0,
            laugh_count: 0,
            apology_count: 0,
            question_count: 0,
            encouragement_count: 0,
        }
    }
}

/// Internal scratch space for daily aggregation.
struct DailyAccum {
    my_messages: u32,
    their_messages: u32,
    my_words: u32,
    their_words: u32,
    my_media: u32,
    their_media: u32,
}

impl DailyAccum {
    fn new() -> Self {
        Self {
            my_messages: 0,
            their_messages: 0,
            my_words: 0,
            their_words: 0,
            my_media: 0,
            their_media: 0,
        }
    }
}

/// Run the single-pass aggregation.
///
/// `tz_offset_secs` is the local UTC offset to use when bucketing timestamps
/// into days and hours. Positive for east of UTC (e.g. +28800 for UTC+8).
/// SMS Backup & Restore stores timestamps in UTC ms regardless of user
/// location, so this offset is the user's preference, not the data's
/// origin TZ.
///
/// `messages` does NOT need to be sorted — aggregation is order-independent
/// for everything except daily/hourly bucket ordering, which we sort at the
/// end before returning.
pub fn compute_aggregates(
    messages: &[AggregatorMessage],
    tz_offset_secs: i32,
    top_emoji_count: usize,
) -> AggregatesOutput {
    let mut me = SideAccum::new();
    let mut them = SideAccum::new();
    let mut daily: HashMap<String, DailyAccum> = HashMap::new();
    let mut hourly: HashMap<(u8, u8), u32> = HashMap::new();

    let tz = FixedOffset::east_opt(tz_offset_secs)
        .unwrap_or_else(|| FixedOffset::east_opt(0).expect("UTC offset is always valid"));

    for msg in messages {
        let acc: &mut SideAccum = match msg.sender {
            Participant::Me => &mut me,
            Participant::Them => &mut them,
        };

        acc.message_count += 1;
        acc.char_count += msg.body.chars().count() as u64;

        // Words: split on whitespace, lowercased for unique-set membership.
        // Word count uses the *count* of tokens; unique set dedupes them.
        let mut words_in_msg: u32 = 0;
        for token in msg.body.split_whitespace() {
            words_in_msg += 1;
            // Lower-case for dedupe; we don't strip punctuation aggressively
            // because casual chat has a lot of "okay,", "really?", etc. and
            // canonical tokenization isn't the goal — rough uniqueness is.
            acc.unique_words.insert(token.to_lowercase());
        }
        acc.word_count += words_in_msg as u64;

        // Emoji extraction. We aggregate counts into the side map, not
        // per-message — the dashboard wants top-N over the whole relationship.
        let msg_emojis = extract_emojis(&msg.body);
        for (cluster, count) in &msg_emojis {
            *acc.emoji_counts.entry(cluster.clone()).or_insert(0) += count;
            acc.emoji_total += count;
        }

        // Pattern flags. Each is independent.
        if is_laugh(&msg.body) {
            acc.laugh_count += 1;
        }
        if is_apology(&msg.body) {
            acc.apology_count += 1;
        }
        if is_question(&msg.body) {
            acc.question_count += 1;
        }
        if is_encouragement(&msg.body) {
            acc.encouragement_count += 1;
        }
        if contains_link(&msg.body) {
            acc.link_count += 1;
        }

        // Media — per-attachment, not per-message. A message with 3 images
        // contributes 3 to image_count.
        for mime in &msg.mime_types {
            match classify_media(mime) {
                MediaCategory::Image => acc.image_count += 1,
                MediaCategory::Video => acc.video_count += 1,
                MediaCategory::Audio => acc.audio_count += 1,
                MediaCategory::Gif => acc.gif_count += 1,
                MediaCategory::Other => {}
            }
        }

        // Daily and hourly bucket increments. Convert UTC ms → local datetime.
        let local = match tz.timestamp_millis_opt(msg.timestamp_ms) {
            chrono::LocalResult::Single(dt) => dt,
            _ => continue, // ambiguous or invalid timestamp — skip its bucket entry
        };
        let day_key = format!(
            "{:04}-{:02}-{:02}",
            local.year(),
            local.month(),
            local.day()
        );
        let bucket = daily.entry(day_key).or_insert_with(DailyAccum::new);
        match msg.sender {
            Participant::Me => {
                bucket.my_messages += 1;
                bucket.my_words += words_in_msg;
                if msg.has_media() {
                    bucket.my_media += msg.mime_types.len() as u32;
                }
            }
            Participant::Them => {
                bucket.their_messages += 1;
                bucket.their_words += words_in_msg;
                if msg.has_media() {
                    bucket.their_media += msg.mime_types.len() as u32;
                }
            }
        }

        // Hourly bucket. Sunday=0 in our schema; chrono uses Mon=0..Sun=6 via
        // num_days_from_monday(), or Sun=0..Sat=6 via num_days_from_sunday().
        // We want Sunday=0.
        let dow = local.weekday().num_days_from_sunday() as u8;
        let hour = local.hour() as u8;
        *hourly.entry((dow, hour)).or_insert(0) += 1;
    }

    // Materialize side accumulators into the public structs.
    let contact = ContactAggregates {
        my_message_count: me.message_count,
        their_message_count: them.message_count,
        my_word_count: me.word_count,
        their_word_count: them.word_count,
        my_unique_word_count: me.unique_words.len() as u32,
        their_unique_word_count: them.unique_words.len() as u32,
        my_character_count: me.char_count,
        their_character_count: them.char_count,

        my_image_count: me.image_count,
        their_image_count: them.image_count,
        my_video_count: me.video_count,
        their_video_count: them.video_count,
        my_audio_count: me.audio_count,
        their_audio_count: them.audio_count,
        my_gif_count: me.gif_count,
        their_gif_count: them.gif_count,
        my_link_count: me.link_count,
        their_link_count: them.link_count,

        my_emoji_total: me.emoji_total,
        their_emoji_total: them.emoji_total,
        my_top_emojis: top_emojis(&me.emoji_counts, top_emoji_count),
        their_top_emojis: top_emojis(&them.emoji_counts, top_emoji_count),
        my_laugh_count: me.laugh_count,
        their_laugh_count: them.laugh_count,
        my_apology_count: me.apology_count,
        their_apology_count: them.apology_count,
        my_question_count: me.question_count,
        their_question_count: them.question_count,
        my_encouragement_count: me.encouragement_count,
        their_encouragement_count: them.encouragement_count,
    };

    // Daily buckets — sort by day so callers get chronological order without
    // having to re-sort.
    let mut daily_out: Vec<DailyBucket> = daily
        .into_iter()
        .map(|(day, accum)| DailyBucket {
            day,
            my_messages: accum.my_messages,
            their_messages: accum.their_messages,
            my_words: accum.my_words,
            their_words: accum.their_words,
            my_media: accum.my_media,
            their_media: accum.their_media,
        })
        .collect();
    daily_out.sort_by(|a, b| a.day.cmp(&b.day));

    let mut hourly_out: Vec<HourlyBucket> = hourly
        .into_iter()
        .map(|((dow, hour), count)| HourlyBucket {
            day_of_week: dow,
            hour,
            message_count: count,
        })
        .collect();
    hourly_out.sort_by(|a, b| (a.day_of_week, a.hour).cmp(&(b.day_of_week, b.hour)));

    AggregatesOutput {
        contact,
        daily: daily_out,
        hourly: hourly_out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(
        rowid: i64,
        ts_ms: i64,
        sender: Participant,
        body: &str,
        mimes: Vec<&str>,
    ) -> AggregatorMessage {
        AggregatorMessage {
            db_rowid: rowid,
            timestamp_ms: ts_ms,
            sender,
            body: body.to_string(),
            mime_types: mimes.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn empty_input_yields_zero_counts() {
        let out = compute_aggregates(&[], 0, 5);
        assert_eq!(out.contact.my_message_count, 0);
        assert_eq!(out.contact.their_message_count, 0);
        assert!(out.daily.is_empty());
        assert!(out.hourly.is_empty());
    }

    #[test]
    fn message_volume_split_by_side() {
        let messages = vec![
            m(1, 1_000_000, Participant::Me, "hello there", vec![]),
            m(2, 1_500_000, Participant::Them, "hi back", vec![]),
            m(3, 2_000_000, Participant::Me, "how are you?", vec![]),
        ];
        let out = compute_aggregates(&messages, 0, 5);
        assert_eq!(out.contact.my_message_count, 2);
        assert_eq!(out.contact.their_message_count, 1);
    }

    #[test]
    fn word_and_char_counts_correct() {
        let messages = vec![
            m(1, 1_000, Participant::Me, "hello world", vec![]), // 2 words, 11 chars
            m(2, 2_000, Participant::Them, "hi", vec![]),        // 1 word, 2 chars
        ];
        let out = compute_aggregates(&messages, 0, 5);
        assert_eq!(out.contact.my_word_count, 2);
        assert_eq!(out.contact.my_character_count, 11);
        assert_eq!(out.contact.their_word_count, 1);
        assert_eq!(out.contact.their_character_count, 2);
    }

    #[test]
    fn unique_words_dedupe_case_insensitively() {
        let messages = vec![m(
            1,
            1_000,
            Participant::Me,
            "Hello hello HELLO world",
            vec![],
        )];
        let out = compute_aggregates(&messages, 0, 5);
        // total tokens: 4. unique (lowered): {"hello", "world"} = 2.
        assert_eq!(out.contact.my_word_count, 4);
        assert_eq!(out.contact.my_unique_word_count, 2);
    }

    #[test]
    fn media_attachments_classified_by_mime() {
        let messages = vec![
            m(
                1,
                1_000,
                Participant::Me,
                "",
                vec!["image/jpeg", "image/png"],
            ),
            m(2, 2_000, Participant::Me, "", vec!["video/mp4"]),
            m(3, 3_000, Participant::Me, "", vec!["audio/mp3"]),
            m(4, 4_000, Participant::Me, "", vec!["image/gif"]),
            m(
                5,
                5_000,
                Participant::Them,
                "",
                vec!["image/heic", "image/heic", "image/heic"],
            ),
        ];
        let out = compute_aggregates(&messages, 0, 5);
        assert_eq!(out.contact.my_image_count, 2);
        assert_eq!(out.contact.my_video_count, 1);
        assert_eq!(out.contact.my_audio_count, 1);
        assert_eq!(out.contact.my_gif_count, 1);
        assert_eq!(out.contact.their_image_count, 3);
    }

    #[test]
    fn link_count_per_message_not_per_url() {
        let messages = vec![
            m(
                1,
                1_000,
                Participant::Me,
                "check https://a.com and https://b.com",
                vec![],
            ),
            m(2, 2_000, Participant::Me, "no link here", vec![]),
            m(3, 3_000, Participant::Them, "www.example.com", vec![]),
        ];
        let out = compute_aggregates(&messages, 0, 5);
        // First message has 2 URLs but counts as 1 link-message.
        assert_eq!(out.contact.my_link_count, 1);
        assert_eq!(out.contact.their_link_count, 1);
    }

    #[test]
    fn pattern_flags_independent() {
        let messages = vec![
            m(1, 1_000, Participant::Me, "lol that's funny haha", vec![]),
            m(2, 2_000, Participant::Me, "sorry about earlier", vec![]),
            m(3, 3_000, Participant::Me, "you got this!", vec![]),
            m(4, 4_000, Participant::Me, "are you ok?", vec![]),
            m(5, 5_000, Participant::Them, "no comment", vec![]),
        ];
        let out = compute_aggregates(&messages, 0, 5);
        assert_eq!(out.contact.my_laugh_count, 1);
        assert_eq!(out.contact.my_apology_count, 1);
        assert_eq!(out.contact.my_encouragement_count, 1);
        assert_eq!(out.contact.my_question_count, 1);
        assert_eq!(out.contact.their_laugh_count, 0);
    }

    #[test]
    fn emoji_aggregation_counts_correctly_per_side() {
        let messages = vec![
            m(1, 1_000, Participant::Me, "😂😂 lol", vec![]),
            m(2, 2_000, Participant::Me, "💀", vec![]),
            m(3, 3_000, Participant::Them, "🤣🤣🤣", vec![]),
        ];
        let out = compute_aggregates(&messages, 0, 5);
        assert_eq!(out.contact.my_emoji_total, 3); // 2 + 1
        assert_eq!(out.contact.their_emoji_total, 3); // 3
        let my_top = &out.contact.my_top_emojis;
        assert_eq!(my_top[0].emoji, "😂");
        assert_eq!(my_top[0].count, 2);
        let their_top = &out.contact.their_top_emojis;
        assert_eq!(their_top[0].emoji, "🤣");
        assert_eq!(their_top[0].count, 3);
    }

    #[test]
    fn daily_buckets_split_by_local_date() {
        // Two messages on the same UTC date but different local dates.
        // ts1 = 2024-03-21T22:00:00Z = 2024-03-22 04:00 in UTC+6
        // ts2 = 2024-03-21T15:00:00Z = 2024-03-21 21:00 in UTC+6
        // With tz_offset = +21600 (UTC+6), the two messages should land on
        // 2024-03-22 and 2024-03-21 respectively.
        let ts1: i64 = 1711058400_000; // 2024-03-21T22:00:00Z
        let ts2: i64 = 1711033200_000; // 2024-03-21T15:00:00Z
        let messages = vec![
            m(1, ts1, Participant::Me, "late", vec![]),
            m(2, ts2, Participant::Them, "earlier", vec![]),
        ];
        let out = compute_aggregates(&messages, 6 * 3600, 5);
        assert_eq!(out.daily.len(), 2);
        // sorted ascending — earlier date first
        assert_eq!(out.daily[0].day, "2024-03-21");
        assert_eq!(out.daily[0].their_messages, 1);
        assert_eq!(out.daily[1].day, "2024-03-22");
        assert_eq!(out.daily[1].my_messages, 1);
    }

    #[test]
    fn hourly_buckets_use_local_dow_and_hour() {
        // ts = 2024-03-21T15:30:00Z. UTC+0 → Thursday 15:00.
        // chrono num_days_from_sunday: Sun=0, Mon=1, ..., Thu=4, Sat=6.
        let ts: i64 = 1711034999_000; // 2024-03-21T15:29:59Z (close enough)
        let messages = vec![m(1, ts, Participant::Me, "hi", vec![])];
        let out = compute_aggregates(&messages, 0, 5);
        assert_eq!(out.hourly.len(), 1);
        let h = &out.hourly[0];
        assert_eq!(h.day_of_week, 4); // Thursday
        assert_eq!(h.hour, 15);
        assert_eq!(h.message_count, 1);
    }

    #[test]
    fn daily_words_and_media_accumulate() {
        let messages = vec![
            m(1, 1_000, Participant::Me, "two words", vec!["image/jpeg"]),
            m(
                2,
                60_000,
                Participant::Me,
                "one",
                vec!["image/jpeg", "image/png"],
            ),
            m(
                3,
                120_000,
                Participant::Them,
                "many many many words",
                vec![],
            ),
        ];
        let out = compute_aggregates(&messages, 0, 5);
        // All three on the same UTC date (1970-01-01).
        assert_eq!(out.daily.len(), 1);
        let day = &out.daily[0];
        assert_eq!(day.my_words, 3); // 2 + 1
        assert_eq!(day.their_words, 4);
        assert_eq!(day.my_media, 3); // 1 + 2 attachments
        assert_eq!(day.their_media, 0);
    }

    #[test]
    fn order_independence_for_aggregates() {
        // Same fixture, scrambled order. Aggregates must be identical.
        let m1 = m(1, 1_000, Participant::Me, "hi", vec![]);
        let m2 = m(2, 2_000, Participant::Them, "hello", vec!["image/jpeg"]);
        let m3 = m(3, 3_000, Participant::Me, "ok?", vec![]);

        let out1 = compute_aggregates(&[m1.clone(), m2.clone(), m3.clone()], 0, 5);
        let out2 = compute_aggregates(&[m3, m1, m2], 0, 5);

        assert_eq!(out1.contact.my_message_count, out2.contact.my_message_count);
        assert_eq!(
            out1.contact.my_question_count,
            out2.contact.my_question_count
        );
        assert_eq!(
            out1.contact.their_image_count,
            out2.contact.their_image_count
        );
    }
}
