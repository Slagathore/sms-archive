//! Analytics tab — per-contact dashboard.
//!
//! Phase progression:
//!   3.1: tab plumbing + placeholder render               ✅
//!   3.2: contact picker (search + sort)                  ✅
//!   3.3: Run Analysis flow + cache state                 ✅
//!   3.4a (this file): number-heavy panels                🔄
//!   3.4b: charts / heatmaps / donut / rating gauge
//!   3.5: Settings → Analytics page (weights + overrides)

use crate::SmsArchiveApp;
use eframe::egui;
use rusqlite::params;
use serde::Deserialize;
use sms_config::ResourceProfile;
use sms_db::Database;
use std::path::Path;
use std::sync::{Arc, Mutex};

// ===================================================================
// STATE
// ===================================================================

#[derive(Debug, Clone, Default)]
pub struct AnalyticsTabState {
    // contact picker
    pub contacts: Vec<AnalyticsContactRow>,
    pub contacts_loaded: bool,
    pub loading: bool,
    pub load_error: Option<String>,
    pub search: String,
    pub selected_contact_id: Option<String>,
    pub min_messages: u32,

    // compute thread
    pub tz_offset_secs: i32,
    pub compute_pending: Option<Arc<Mutex<ComputeSlot>>>,
    pub compute_status_msg: String,

    // cached analytics for selected contact
    pub loaded: Option<LoadedAnalytics>,
    pub cache_loaded_for: Option<String>,

    // tunables loaded from analytics_meta (Phase 3.5)
    pub settings: AnalyticsSettings,
    pub settings_loaded: bool,
    pub settings_status: String,
    pub settings_open: bool,

    // expand all insights (default: top-3-per-category)
    pub insights_show_all: bool,
}

/// Mirror of every key seeded into `analytics_meta` by migration 0014.
/// The `Default` impl matches those seeds so a fresh install has sane numbers
/// before the user touches anything.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyticsSettings {
    // Segmentation
    pub conversation_timeout_secs: i64,
    pub big_moment_threshold_static: u32,
    pub big_moment_threshold_dynamic_pct: u8,
    pub big_moment_threshold_dynamic_floor: u32,
    pub reconnect_tier1_secs: i64,
    pub reconnect_tier2_secs: i64,
    pub reconnect_tier3_secs: i64,
    pub reconnect_tier4_multiplier: f64,

    // Response
    pub rapid_response_threshold_secs: i64,
    pub overnight_window_start_hour: u8,
    pub overnight_window_end_hour: u8,

    // Point weights
    pub weight_text_message: f64,
    pub weight_per_word_log: f64,
    pub weight_emoji: f64,
    pub weight_question: f64,
    pub weight_image: f64,
    pub weight_video: f64,
    pub weight_audio: f64,
    pub weight_gif: f64,
    pub weight_link: f64,
    pub weight_started_convo: f64,
    pub weight_rapid_response: f64,
    pub weight_encouragement: f64,
    pub weight_apology: f64,

    // Rating component weights
    pub rating_weight_responsiveness: f64,
    pub rating_weight_balance: f64,
    pub rating_weight_engagement: f64,
    pub rating_weight_consistency: f64,
    pub rating_weight_reciprocity: f64,
    pub rating_weight_longevity: f64,
    pub rating_weight_mutual_effort: f64,

    // Rating display thresholds
    pub rating_hide_below_messages: u32,
    pub rating_low_confidence_max_messages: u32,

    // Insight engine tuning
    pub insight_tier1_ratio: f64,
    pub insight_tier2_ratio: f64,
    pub insight_tier3_ratio: f64,
    pub insight_tier4_ratio: f64,
    pub insight_min_sample_per_rule: u32,
    pub insight_low_confidence_max_sample: u32,

    // Picker default
    pub contact_picker_min_messages: u32,
}

impl Default for AnalyticsSettings {
    fn default() -> Self {
        Self {
            conversation_timeout_secs: 14_400,
            big_moment_threshold_static: 20,
            big_moment_threshold_dynamic_pct: 90,
            big_moment_threshold_dynamic_floor: 10,
            reconnect_tier1_secs: 86_400,
            reconnect_tier2_secs: 604_800,
            reconnect_tier3_secs: 2_592_000,
            reconnect_tier4_multiplier: 3.0,

            rapid_response_threshold_secs: 60,
            overnight_window_start_hour: 23,
            overnight_window_end_hour: 7,

            weight_text_message: 1.0,
            weight_per_word_log: 0.1,
            weight_emoji: 0.2,
            weight_question: 0.5,
            weight_image: 3.0,
            weight_video: 5.0,
            weight_audio: 4.0,
            weight_gif: 2.0,
            weight_link: 2.0,
            weight_started_convo: 5.0,
            weight_rapid_response: 2.0,
            weight_encouragement: 3.0,
            weight_apology: 2.0,

            rating_weight_responsiveness: 0.20,
            rating_weight_balance: 0.15,
            rating_weight_engagement: 0.15,
            rating_weight_consistency: 0.15,
            rating_weight_reciprocity: 0.10,
            rating_weight_longevity: 0.10,
            rating_weight_mutual_effort: 0.15,

            rating_hide_below_messages: 50,
            rating_low_confidence_max_messages: 200,

            insight_tier1_ratio: 1.2,
            insight_tier2_ratio: 1.5,
            insight_tier3_ratio: 2.0,
            insight_tier4_ratio: 3.0,
            insight_min_sample_per_rule: 50,
            insight_low_confidence_max_sample: 200,

            contact_picker_min_messages: 50,
        }
    }
}

impl AnalyticsSettings {
    /// Build an `OrchestratorConfig` from these settings + a TZ offset.
    /// The orchestrator owns the canonical config shape; this is the
    /// single point where settings flow into compute.
    pub fn to_orchestrator_config(&self, tz_offset_secs: i32) -> sms_analytics::OrchestratorConfig {
        let mut cfg = sms_analytics::OrchestratorConfig::default();
        cfg.tz_offset_secs = tz_offset_secs;

        cfg.segmentation.conversation_timeout_ms = self.conversation_timeout_secs * 1_000;
        cfg.segmentation.big_moment_static_threshold = self.big_moment_threshold_static;
        cfg.segmentation.big_moment_dynamic_percentile = self.big_moment_threshold_dynamic_pct;
        cfg.segmentation.big_moment_dynamic_floor = self.big_moment_threshold_dynamic_floor;
        cfg.segmentation.reconnect_tier1_ms = self.reconnect_tier1_secs * 1_000;
        cfg.segmentation.reconnect_tier2_ms = self.reconnect_tier2_secs * 1_000;
        cfg.segmentation.reconnect_tier3_ms = self.reconnect_tier3_secs * 1_000;
        cfg.segmentation.reconnect_tier4_multiplier = self.reconnect_tier4_multiplier;

        cfg.response.rapid_response_threshold_ms = self.rapid_response_threshold_secs * 1_000;
        cfg.response.overnight_start_hour = self.overnight_window_start_hour;
        cfg.response.overnight_end_hour = self.overnight_window_end_hour;

        cfg.weights.text_message = self.weight_text_message;
        cfg.weights.per_word_log = self.weight_per_word_log;
        cfg.weights.emoji = self.weight_emoji;
        cfg.weights.question = self.weight_question;
        cfg.weights.image = self.weight_image;
        cfg.weights.video = self.weight_video;
        cfg.weights.audio = self.weight_audio;
        cfg.weights.gif = self.weight_gif;
        cfg.weights.link = self.weight_link;
        cfg.weights.started_convo = self.weight_started_convo;
        cfg.weights.rapid_response = self.weight_rapid_response;
        cfg.weights.encouragement = self.weight_encouragement;
        cfg.weights.apology = self.weight_apology;

        cfg.rating_weights.responsiveness = self.rating_weight_responsiveness;
        cfg.rating_weights.balance = self.rating_weight_balance;
        cfg.rating_weights.engagement = self.rating_weight_engagement;
        cfg.rating_weights.consistency = self.rating_weight_consistency;
        cfg.rating_weights.reciprocity = self.rating_weight_reciprocity;
        cfg.rating_weights.longevity = self.rating_weight_longevity;
        cfg.rating_weights.mutual_effort = self.rating_weight_mutual_effort;

        cfg.rating_thresholds.hide_below_messages = self.rating_hide_below_messages;
        cfg.rating_thresholds.low_confidence_max_messages = self.rating_low_confidence_max_messages;

        cfg.engine.min_sample_for_low_confidence = self.insight_min_sample_per_rule;
        cfg.engine.min_sample_for_medium = self.insight_low_confidence_max_sample;
        // (insight_tier*_ratio are not currently exposed on EngineConfig — they
        //  live as constants in the rule engine for v1; surfacing them is a
        //  later refactor of the analytics crate.)

        cfg.rapid_response_threshold_ms = self.rapid_response_threshold_secs * 1_000;
        cfg
    }
}

#[derive(Debug, Clone)]
pub struct AnalyticsContactRow {
    pub contact_id: String,
    pub display_name: String,
    pub source: String,
    pub message_count: i64,
    pub primary_address: String,
}

#[derive(Debug, Clone)]
pub enum ComputeSlot {
    Running,
    Done(sms_analytics::OrchestratorOutput),
    Failed(String),
}

/// Everything we read from analytics tables to render the dashboard. One
/// monolithic struct keeps DB I/O minimal (one query per table) and
/// centralizes the schema-mapping concern in `read_loaded_analytics`.
#[derive(Debug, Clone)]
pub struct LoadedAnalytics {
    // status
    pub last_computed_at: i64,
    pub is_stale: bool,
    pub last_compute_ms: Option<i64>,
    pub last_error: Option<String>,

    // contact_analytics — volume
    pub my_messages: i64,
    pub their_messages: i64,
    pub my_words: i64,
    pub their_words: i64,
    pub my_unique_words: i64,
    pub their_unique_words: i64,
    pub my_chars: i64,
    pub their_chars: i64,

    // contact_analytics — media
    pub my_images: i64,
    pub their_images: i64,
    pub my_videos: i64,
    pub their_videos: i64,
    pub my_audios: i64,
    pub their_audios: i64,
    pub my_gifs: i64,
    pub their_gifs: i64,
    pub my_links: i64,
    pub their_links: i64,

    // contact_analytics — language
    pub my_top_emojis: Vec<EmojiCount>,
    pub their_top_emojis: Vec<EmojiCount>,
    pub my_emoji_total: i64,
    pub their_emoji_total: i64,
    pub my_laughs: i64,
    pub their_laughs: i64,
    pub my_apologies: i64,
    pub their_apologies: i64,
    pub my_questions: i64,
    pub their_questions: i64,
    pub my_encouragement: i64,
    pub their_encouragement: i64,

    // pair_analytics — conversations
    pub total_conversations: i64,
    pub started_by_me: i64,
    pub started_by_them: i64,
    pub closed_by_me: i64,
    pub closed_by_them: i64,
    pub avg_convo_points: f64,
    pub median_convo_messages: f64,
    pub my_doubles: i64,
    pub their_doubles: i64,
    pub my_missed: i64,
    pub their_missed: i64,
    pub reconnect_t1: i64,
    pub reconnect_t2: i64,
    pub reconnect_t3: i64,
    pub reconnect_t4: i64,
    pub top_contributor: Option<i64>, // 1=me, 2=them

    // pair_analytics — responses
    pub my_median_resp_ms: Option<i64>,
    pub their_median_resp_ms: Option<i64>,
    pub my_mean_resp_ms: Option<i64>,
    pub their_mean_resp_ms: Option<i64>,
    pub my_rapid_pct: Option<f64>,
    pub their_rapid_pct: Option<f64>,
    pub my_first_median_ms: Option<i64>,
    pub their_first_median_ms: Option<i64>,
    pub my_awake_median_ms: Option<i64>,
    pub their_awake_median_ms: Option<i64>,
    pub my_overnight_median_ms: Option<i64>,
    pub their_overnight_median_ms: Option<i64>,

    // pair_analytics — points + score
    pub my_points: f64,
    pub their_points: f64,
    pub overall_score: Option<i64>,
    pub score_responsiveness: Option<i64>,
    pub score_balance: Option<i64>,
    pub score_engagement: Option<i64>,
    pub score_consistency: Option<i64>,
    pub score_reciprocity: Option<i64>,
    pub score_longevity: Option<i64>,
    pub score_mutual_effort: Option<i64>,

    // pair_analytics — JSON blobs (parsed)
    pub insights: Vec<LoadedInsight>,
    pub writing_milestones: Option<WritingMilestonesJson>,
    pub conversation_flow_json: Option<String>, // raw — parsed lazily by sankey renderer

    // pair_analytics — focus percentages (None until orchestrator computes them)
    pub focus_me_pct: Option<f64>,
    pub focus_them_pct: Option<f64>,
    pub focus_other_pct: Option<f64>,

    // post-mimoto wishlist
    pub sentiment_timeline: Option<SentimentTimelineLoaded>,
    pub inside_jokes: Vec<InsideJokeLoaded>,
    pub topics: Vec<TopicPhraseLoaded>,

    // pair_analytics — span
    pub first_message_ms: Option<i64>,
    pub last_message_ms: Option<i64>,

    // activity tables (chronological for daily, sorted by (dow, hour) for hourly)
    pub daily: Vec<DailyActivityPoint>,
    pub hourly: Vec<HourlyActivityBucket>,
}

#[derive(Debug, Clone)]
pub struct DailyActivityPoint {
    pub day: String, // "YYYY-MM-DD"
    pub my_msgs: i64,
    pub their_msgs: i64,
    pub my_points: f64,
    pub their_points: f64,
}

#[derive(Debug, Clone)]
pub struct HourlyActivityBucket {
    pub day_of_week: u8, // 0 = Sunday
    pub hour: u8,
    pub message_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmojiCount {
    pub emoji: String,
    pub count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoadedInsight {
    #[allow(dead_code)]
    pub id: String,
    pub icon: char,
    pub headline: String,
    pub detail: String,
    pub tier: u8,
    pub category: String,
    pub confidence: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WritingMilestonesJson {
    pub total_chars: u64,
    pub total_words: u64,
    pub harry_potter_equivalents: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentimentTimelineLoaded {
    pub days: Vec<SentimentDayLoaded>,
    pub overall_my: Option<f64>,
    pub overall_their: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentimentDayLoaded {
    pub day: String,
    pub my_score: Option<f64>,
    pub their_score: Option<f64>,
    pub my_messages: u32,
    pub their_messages: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InsideJokeLoaded {
    pub phrase: String,
    pub count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TopicPhraseLoaded {
    pub phrase: String,
    pub pair_count: u32,
    pub score: f64,
}

impl AnalyticsTabState {
    pub fn new() -> Self {
        Self {
            min_messages: 50,
            ..Self::default()
        }
    }
}

// ===================================================================
// TOP-LEVEL RENDER
// ===================================================================

pub fn render_analytics_tab(app: &mut SmsArchiveApp, ui: &mut egui::Ui) {
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.heading("Analytics");
            ui.add_space(4.0);

            if app.db_path.trim().is_empty() {
                ui.label("Open or create a database first (Import tab).");
                return;
            }

            if !app.analytics.contacts_loaded && !app.analytics.loading {
                load_contacts(app);
            }

            poll_compute_slot(app);
            sync_loaded_for_selection(app);

            render_settings_disclosure(app, ui);
            ui.add_space(8.0);

            render_picker_controls(app, ui);
            ui.add_space(8.0);
            render_contact_list(app, ui);
            ui.add_space(8.0);
            render_selection_panel(app, ui);
        });
}

fn render_picker_controls(app: &mut SmsArchiveApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label("Search:");
        ui.text_edit_singleline(&mut app.analytics.search);
        ui.separator();
        ui.label("Min messages:");
        let mut min_str = app.analytics.min_messages.to_string();
        if ui.text_edit_singleline(&mut min_str).changed() {
            if let Ok(n) = min_str.parse::<u32>() {
                app.analytics.min_messages = n;
            }
        }
        if ui.button("🔄 Refresh contacts").clicked() {
            load_contacts(app);
        }
        if let Some(err) = &app.analytics.load_error {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err.clone());
        }
    });
}

fn render_contact_list(app: &mut SmsArchiveApp, ui: &mut egui::Ui) {
    if app.analytics.contacts.is_empty() {
        if app.analytics.contacts_loaded {
            ui.label("No contacts at or above the message threshold. Lower it or import data.");
        } else {
            ui.label("Loading contacts…");
        }
        return;
    }

    let search = app.analytics.search.trim().to_lowercase();
    let min_msgs = app.analytics.min_messages as i64;
    let filtered_indices: Vec<usize> = app
        .analytics
        .contacts
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            if c.message_count < min_msgs {
                return None;
            }
            if !search.is_empty() {
                let hit_name = c.display_name.to_lowercase().contains(&search);
                let hit_addr = c.primary_address.to_lowercase().contains(&search);
                if !hit_name && !hit_addr {
                    return None;
                }
            }
            Some(i)
        })
        .collect();

    ui.label(format!(
        "{} contact(s) shown (of {} loaded)",
        filtered_indices.len(),
        app.analytics.contacts.len()
    ));

    egui::ScrollArea::vertical()
        .max_height(280.0)
        .id_source("analytics_contact_list")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_sized([260.0, 18.0], egui::Label::new(egui::RichText::new("Contact").strong()));
                ui.add_sized([100.0, 18.0], egui::Label::new(egui::RichText::new("Messages").strong()));
                ui.add_sized([80.0, 18.0], egui::Label::new(egui::RichText::new("Source").strong()));
            });
            ui.separator();
            for idx in filtered_indices {
                let row = &app.analytics.contacts[idx];
                let selected = app.analytics.selected_contact_id.as_deref() == Some(row.contact_id.as_str());
                let label = if row.display_name == row.primary_address {
                    row.display_name.clone()
                } else {
                    format!("{}  ({})", row.display_name, row.primary_address)
                };
                ui.horizontal(|ui| {
                    let resp = ui.add_sized(
                        [260.0, 22.0],
                        egui::SelectableLabel::new(selected, label),
                    );
                    if resp.clicked() {
                        app.analytics.selected_contact_id = Some(row.contact_id.clone());
                    }
                    ui.add_sized([100.0, 22.0], egui::Label::new(format!("{}", row.message_count)));
                    ui.add_sized([80.0, 22.0], egui::Label::new(row.source.clone()));
                });
            }
        });
}

fn render_selection_panel(app: &mut SmsArchiveApp, ui: &mut egui::Ui) {
    let Some(contact_id) = app.analytics.selected_contact_id.clone() else {
        ui.label("Pick a contact above to continue.");
        return;
    };
    let Some(row) = app
        .analytics
        .contacts
        .iter()
        .find(|c| c.contact_id == contact_id)
        .cloned()
    else {
        ui.label("Selected contact no longer in the list (refresh?).");
        return;
    };

    // Header card with cache status + Run Analysis.
    ui.group(|ui| {
        ui.heading(&row.display_name);
        ui.label(format!(
            "Address: {} • Messages: {} • Source: {}",
            row.primary_address, row.message_count, row.source
        ));
        ui.add_space(4.0);
        render_cache_status_line(app, ui);
        ui.add_space(4.0);
        render_run_analysis_controls(app, ui, &contact_id);
    });

    ui.add_space(10.0);

    // If no analytics yet, stop here.
    let Some(loaded) = app.analytics.loaded.clone() else {
        ui.weak("No analytics computed yet — click \"Run Analysis\" above.");
        return;
    };

    render_dashboard(app, ui, &loaded);
}

fn render_cache_status_line(app: &mut SmsArchiveApp, ui: &mut egui::Ui) {
    let Some(loaded) = app.analytics.loaded.as_ref() else {
        ui.weak("Cache status: ⛔ never computed");
        return;
    };
    if loaded.last_computed_at == 0 {
        ui.label("Cache status: ⛔ never computed");
        return;
    }
    let elapsed = current_unix_secs().saturating_sub(loaded.last_computed_at);
    let when = humanize_seconds_ago(elapsed);
    if loaded.is_stale {
        ui.colored_label(
            egui::Color32::from_rgb(220, 150, 0),
            format!("⚠ Cache is stale (last computed {} — re-run to refresh)", when),
        );
    } else {
        let ms_str = loaded
            .last_compute_ms
            .map(|ms| format!(" — last run took {} ms", ms))
            .unwrap_or_default();
        ui.label(format!("Cache status: ✅ fresh, computed {}{}", when, ms_str));
    }
    if let Some(err) = &loaded.last_error {
        ui.colored_label(
            egui::Color32::from_rgb(220, 80, 80),
            format!("Last compute error: {}", err),
        );
    }
}

fn render_run_analysis_controls(app: &mut SmsArchiveApp, ui: &mut egui::Ui, contact_id: &str) {
    let running = app.analytics.compute_pending.is_some();
    ui.horizontal(|ui| {
        let label = if app
            .analytics
            .loaded
            .as_ref()
            .is_some_and(|l| l.last_computed_at > 0)
        {
            "🔁  Refresh Analytics"
        } else {
            "▶  Run Analysis"
        };
        if ui
            .add_enabled(!running, egui::Button::new(label))
            .clicked()
        {
            spawn_compute(app, contact_id);
        }
        ui.label("TZ offset (s):");
        let mut tz_str = app.analytics.tz_offset_secs.to_string();
        if ui.text_edit_singleline(&mut tz_str).changed() {
            if let Ok(n) = tz_str.parse::<i32>() {
                app.analytics.tz_offset_secs = n;
            }
        }

        // Export button — only available once analytics are loaded.
        let export_enabled =
            !running && app.analytics.loaded.as_ref().is_some_and(|l| l.last_computed_at > 0);
        if ui
            .add_enabled(export_enabled, egui::Button::new("📄 Export HTML"))
            .clicked()
        {
            export_html_report(app, contact_id);
        }

        if running {
            ui.spinner();
            ui.label("computing…");
        } else if !app.analytics.compute_status_msg.is_empty() {
            ui.weak(app.analytics.compute_status_msg.clone());
        }
    });
}

/// Save the current dashboard to a self-contained HTML file the user can
/// open in any browser (and print-to-PDF from there). Path picked via a
/// native save dialog.
fn export_html_report(app: &mut SmsArchiveApp, contact_id: &str) {
    let Some(loaded) = app.analytics.loaded.as_ref() else {
        app.analytics.compute_status_msg = "No analytics loaded — run analysis first.".into();
        return;
    };
    let display_name = app
        .analytics
        .contacts
        .iter()
        .find(|c| c.contact_id == contact_id)
        .map(|c| c.display_name.clone())
        .unwrap_or_else(|| "contact".into());
    let safe_name: String = display_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let default_filename = format!("analytics-{}.html", safe_name);

    let Some(path) = rfd::FileDialog::new()
        .set_file_name(&default_filename)
        .add_filter("HTML", &["html"])
        .save_file()
    else {
        return;
    };

    let html = build_html_report(&display_name, loaded);
    match std::fs::write(&path, html) {
        Ok(()) => {
            app.analytics.compute_status_msg =
                format!("Exported to {}", path.display());
        }
        Err(e) => {
            app.analytics.compute_status_msg = format!("Export failed: {}", e);
        }
    }
}

fn build_html_report(display_name: &str, l: &LoadedAnalytics) -> String {
    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n");
    html.push_str(&format!(
        "<title>Analytics — {}</title>\n",
        html_escape(display_name)
    ));
    html.push_str(EXPORT_CSS);
    html.push_str("</head><body>\n");

    html.push_str(&format!(
        "<h1>Analytics: {}</h1>\n",
        html_escape(display_name)
    ));
    if let Some(score) = l.overall_score {
        html.push_str(&format!(
            "<div class=\"rating\"><div class=\"score\">{} / 100</div><div class=\"descriptor\">{}</div></div>\n",
            score,
            html_escape(&export_rating_descriptor(score))
        ));
    }

    // KPI strip
    let total_msgs = l.my_messages + l.their_messages;
    let span_label = match (l.first_message_ms, l.last_message_ms) {
        (Some(first), Some(last)) if last > first => {
            let days = (last - first) as f64 / (24.0 * 60.0 * 60.0 * 1000.0);
            if days >= 365.0 {
                format!("{:.1} years", days / 365.0)
            } else {
                format!("{:.0} days", days)
            }
        }
        _ => "—".into(),
    };
    html.push_str("<div class=\"kpis\">\n");
    html.push_str(&kpi_html("Chat Points", &format_with_commas((l.my_points + l.their_points).round() as i64)));
    html.push_str(&kpi_html("Time Period", &span_label));
    html.push_str(&kpi_html("Messages", &format_with_commas(total_msgs)));
    html.push_str(&kpi_html("Conversations", &format_with_commas(l.total_conversations)));
    html.push_str("</div>\n");

    // Component scores
    html.push_str("<h2>Chat Rating Components</h2>\n<table class=\"k-v\">\n");
    let comps = [
        ("Responsiveness", l.score_responsiveness),
        ("Balance", l.score_balance),
        ("Engagement", l.score_engagement),
        ("Consistency", l.score_consistency),
        ("Reciprocity", l.score_reciprocity),
        ("Longevity", l.score_longevity),
        ("Mutual Effort", l.score_mutual_effort),
    ];
    for (name, value) in comps {
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td></tr>\n",
            html_escape(name),
            value.map(|v| v.to_string()).unwrap_or_else(|| "—".into())
        ));
    }
    html.push_str("</table>\n");

    // Insights
    if !l.insights.is_empty() {
        html.push_str("<h2>Key Insights</h2>\n<ul class=\"insights\">\n");
        for ins in &l.insights {
            html.push_str(&format!(
                "<li class=\"cat-{}\"><span class=\"icon\">{}</span><div><strong>{}</strong><br><span class=\"detail\">{}</span></div></li>\n",
                html_escape(&ins.category),
                ins.icon,
                html_escape(&ins.headline),
                html_escape(&ins.detail),
            ));
        }
        html.push_str("</ul>\n");
    }

    // Number-heavy panels
    html.push_str("<div class=\"two-col\">");
    html.push_str(&panel_html(
        "Message Analysis",
        &[
            ("Messages", format_with_commas(l.my_messages), format_with_commas(l.their_messages)),
            ("Words", format_with_commas(l.my_words), format_with_commas(l.their_words)),
            ("Unique words", format_with_commas(l.my_unique_words), format_with_commas(l.their_unique_words)),
            ("Characters", format_with_commas(l.my_chars), format_with_commas(l.their_chars)),
        ],
    ));
    html.push_str(&panel_html(
        "Media Stats",
        &[
            ("Images", format_with_commas(l.my_images), format_with_commas(l.their_images)),
            ("Videos", format_with_commas(l.my_videos), format_with_commas(l.their_videos)),
            ("Audios", format_with_commas(l.my_audios), format_with_commas(l.their_audios)),
            ("GIFs", format_with_commas(l.my_gifs), format_with_commas(l.their_gifs)),
            ("Links", format_with_commas(l.my_links), format_with_commas(l.their_links)),
        ],
    ));
    html.push_str("</div>\n");

    html.push_str("<div class=\"two-col\">");
    html.push_str(&panel_html(
        "Language",
        &[
            ("Emojis", format_with_commas(l.my_emoji_total), format_with_commas(l.their_emoji_total)),
            ("Laughs", format_with_commas(l.my_laughs), format_with_commas(l.their_laughs)),
            ("Apologies", format_with_commas(l.my_apologies), format_with_commas(l.their_apologies)),
            ("Questions", format_with_commas(l.my_questions), format_with_commas(l.their_questions)),
            ("Encouragement", format_with_commas(l.my_encouragement), format_with_commas(l.their_encouragement)),
        ],
    ));
    html.push_str(&panel_html(
        "Responding",
        &[
            ("Median response", fmt_ms_or_dash(l.my_median_resp_ms), fmt_ms_or_dash(l.their_median_resp_ms)),
            ("Mean response", fmt_ms_or_dash(l.my_mean_resp_ms), fmt_ms_or_dash(l.their_mean_resp_ms)),
            ("Rapid response", fmt_pct_or_dash(l.my_rapid_pct), fmt_pct_or_dash(l.their_rapid_pct)),
            ("First-response median", fmt_ms_or_dash(l.my_first_median_ms), fmt_ms_or_dash(l.their_first_median_ms)),
            ("Awake median", fmt_ms_or_dash(l.my_awake_median_ms), fmt_ms_or_dash(l.their_awake_median_ms)),
            ("Overnight median", fmt_ms_or_dash(l.my_overnight_median_ms), fmt_ms_or_dash(l.their_overnight_median_ms)),
        ],
    ));
    html.push_str("</div>\n");

    html.push_str(&panel_html(
        "Conversation Analysis",
        &[
            ("Started", format_with_commas(l.started_by_me), format_with_commas(l.started_by_them)),
            ("Closed", format_with_commas(l.closed_by_me), format_with_commas(l.closed_by_them)),
            ("Missed", format_with_commas(l.my_missed), format_with_commas(l.their_missed)),
            ("Doubles", format_with_commas(l.my_doubles), format_with_commas(l.their_doubles)),
        ],
    ));

    // Top emojis
    if !l.my_top_emojis.is_empty() || !l.their_top_emojis.is_empty() {
        html.push_str("<h2>Top Emojis</h2>\n<div class=\"emojis\">\n");
        html.push_str(&emoji_row_html("You", &l.my_top_emojis));
        html.push_str(&emoji_row_html("Them", &l.their_top_emojis));
        html.push_str("</div>\n");
    }

    // Streaks
    if !l.daily.is_empty() {
        let s = compute_streaks(&l.daily);
        html.push_str("<h2>Streaks</h2>\n<table class=\"k-v\">\n");
        html.push_str(&format!("<tr><td>Current active streak</td><td>{} days</td></tr>\n", s.current_active_streak));
        html.push_str(&format!("<tr><td>Longest active streak</td><td>{} days</td></tr>\n", s.longest_active_streak));
        html.push_str(&format!("<tr><td>Current silence</td><td>{} days</td></tr>\n", s.current_silent_streak));
        html.push_str(&format!("<tr><td>Longest silence</td><td>{} days</td></tr>\n", s.longest_silent_streak));
        html.push_str("</table>\n");
    }

    // Relationship Growth line chart (cumulative points over time)
    if !l.daily.is_empty() {
        html.push_str("<h2>Relationship Growth</h2>\n");
        html.push_str(&build_growth_chart_svg(&l.daily));
    }

    // Sentiment timeline
    if let Some(timeline) = &l.sentiment_timeline {
        if !timeline.days.is_empty() {
            html.push_str("<h2>Sentiment Timeline</h2>\n");
            if let Some(my) = timeline.overall_my {
                html.push_str(&format!("<p>Your overall tone: <strong>{:+.2}</strong></p>", my));
            }
            if let Some(t) = timeline.overall_their {
                html.push_str(&format!("<p>Their overall tone: <strong>{:+.2}</strong></p>", t));
            }
            html.push_str(&build_sentiment_chart_svg(&timeline.days));
        }
    }

    // Sankey conversation flow
    if let Some(raw) = l.conversation_flow_json.as_deref() {
        if !raw.trim().is_empty() && raw.trim() != "{}" {
            if let Ok(data) = serde_json::from_str::<SankeyData>(raw) {
                if !data.nodes.is_empty() {
                    html.push_str("<h2>Conversation Flow</h2>\n");
                    html.push_str(&build_sankey_svg(&data));
                }
            }
        }
    }

    // Direction donut as SVG
    if let (Some(me), Some(them), Some(other)) =
        (l.focus_me_pct, l.focus_them_pct, l.focus_other_pct)
    {
        html.push_str("<h2>Direction of Conversation</h2>\n");
        html.push_str(&build_direction_donut_svg(me, them, other));
    }

    // Topics (distinctive phrases via TF-IDF)
    if !l.topics.is_empty() {
        html.push_str("<h2>What you talk about</h2>\n<table class=\"k-v\">\n");
        html.push_str("<tr><th>Phrase</th><th>Score</th><th>Pair count</th></tr>\n");
        for t in &l.topics {
            html.push_str(&format!(
                "<tr><td>{}</td><td>{:.1}</td><td>{}</td></tr>\n",
                html_escape(&t.phrase),
                t.score,
                t.pair_count
            ));
        }
        html.push_str("</table>\n");
    }

    // Inside jokes
    if !l.inside_jokes.is_empty() {
        html.push_str("<h2>Inside Jokes / Recurring Phrases</h2>\n<table class=\"k-v\">\n");
        html.push_str("<tr><th>Phrase</th><th>Count</th></tr>\n");
        for j in &l.inside_jokes {
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td></tr>\n",
                html_escape(&j.phrase),
                j.count
            ));
        }
        html.push_str("</table>\n");
    }

    // Daily heatmap as inline SVG
    if !l.daily.is_empty() {
        html.push_str("<h2>Daily Activity (last 500 days)</h2>\n");
        html.push_str(&build_daily_heatmap_svg(&l.daily));
    }

    // Hourly heatmap as inline SVG
    if !l.hourly.is_empty() {
        html.push_str("<h2>Messaging Times</h2>\n");
        html.push_str(&build_hourly_heatmap_svg(&l.hourly));
    }

    // Direction donut as text + numbers (SVG would be nice but not blocking).
    if let (Some(me), Some(them), Some(other)) =
        (l.focus_me_pct, l.focus_them_pct, l.focus_other_pct)
    {
        html.push_str("<h2>Direction of Conversation</h2>\n<table class=\"k-v\">\n");
        html.push_str(&format!("<tr><td>You</td><td>{:.1}%</td></tr>\n", me * 100.0));
        html.push_str(&format!("<tr><td>Them</td><td>{:.1}%</td></tr>\n", them * 100.0));
        html.push_str(&format!("<tr><td>Others</td><td>{:.1}%</td></tr>\n", other * 100.0));
        html.push_str("</table>\n");
    }

    // Writing milestones
    if let Some(m) = &l.writing_milestones {
        html.push_str("<h2>Summary of Writing</h2>\n<table class=\"k-v\">\n");
        html.push_str(&format!("<tr><td>Total characters</td><td>{}</td></tr>\n", format_with_commas(m.total_chars as i64)));
        html.push_str(&format!("<tr><td>Total words</td><td>{}</td></tr>\n", format_with_commas(m.total_words as i64)));
        html.push_str(&format!("<tr><td>Harry Potter equivalents</td><td>{:.2}</td></tr>\n", m.harry_potter_equivalents));
        html.push_str("</table>\n");
    }

    html.push_str("<footer><em>Generated by SMS Archive — print this page to PDF if you want a portable copy.</em></footer>\n");
    html.push_str("</body></html>\n");
    html
}

const EXPORT_CSS: &str = "<style>
body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; max-width: 1100px; margin: 30px auto; padding: 0 20px; color: #1a1a1a; background: #fafafa; }
h1 { border-bottom: 2px solid #333; padding-bottom: 8px; }
h2 { margin-top: 30px; color: #444; }
.rating { display: flex; align-items: baseline; gap: 16px; padding: 12px; background: #fff; border-radius: 8px; margin-bottom: 16px; }
.score { font-size: 42px; font-weight: 700; }
.descriptor { color: #666; font-size: 18px; }
.kpis { display: grid; grid-template-columns: repeat(4, 1fr); gap: 12px; margin: 16px 0; }
.kpi { background: #fff; border-radius: 6px; padding: 14px; box-shadow: 0 1px 3px rgba(0,0,0,0.06); }
.kpi .label { font-size: 12px; color: #666; text-transform: uppercase; letter-spacing: 0.5px; }
.kpi .value { font-size: 24px; font-weight: 600; margin-top: 4px; }
.two-col { display: grid; grid-template-columns: 1fr 1fr; gap: 16px; margin: 12px 0; }
.panel { background: #fff; border-radius: 6px; padding: 14px; box-shadow: 0 1px 3px rgba(0,0,0,0.06); }
.panel h3 { margin: 0 0 10px 0; font-size: 16px; }
table { width: 100%; border-collapse: collapse; }
table th { text-align: left; font-size: 12px; color: #666; padding: 4px 8px; }
table td { padding: 4px 8px; }
table.k-v { background: #fff; border-radius: 6px; overflow: hidden; }
table.k-v tr:nth-child(odd) { background: #f4f4f4; }
table.k-v td:first-child { font-weight: 500; width: 50%; }
.insights { list-style: none; padding: 0; }
.insights li { display: flex; align-items: flex-start; gap: 12px; padding: 10px; margin: 6px 0; border-radius: 6px; background: #fff; }
.insights .icon { font-size: 22px; }
.insights .detail { color: #666; }
.insights li.cat-YourSide { background: #e8f0fa; }
.insights li.cat-TheirSide { background: #fdf6e3; }
.insights li.cat-Shared { background: #ecf6ec; }
.emojis { display: flex; flex-direction: column; gap: 6px; }
.emojis .row { display: flex; gap: 8px; align-items: center; }
.emojis .row .who { font-weight: 600; min-width: 60px; }
svg { max-width: 100%; background: #fff; border-radius: 6px; padding: 10px; box-shadow: 0 1px 3px rgba(0,0,0,0.06); }
footer { margin-top: 40px; color: #888; font-size: 12px; }
@media print { body { background: #fff; } .panel, .kpi, table.k-v, svg { box-shadow: none; } }
</style>\n";

fn kpi_html(label: &str, value: &str) -> String {
    format!(
        "<div class=\"kpi\"><div class=\"label\">{}</div><div class=\"value\">{}</div></div>\n",
        html_escape(label),
        html_escape(value)
    )
}

fn panel_html(title: &str, rows: &[(&str, String, String)]) -> String {
    let mut out = String::new();
    out.push_str(&format!("<div class=\"panel\"><h3>{}</h3>\n<table>\n<thead><tr><th>Metric</th><th>You</th><th>Them</th></tr></thead>\n<tbody>\n", html_escape(title)));
    for (label, mine, theirs) in rows {
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            html_escape(label),
            html_escape(mine),
            html_escape(theirs)
        ));
    }
    out.push_str("</tbody>\n</table>\n</div>\n");
    out
}

fn emoji_row_html(who: &str, items: &[EmojiCount]) -> String {
    let mut out = format!("<div class=\"row\"><span class=\"who\">{}:</span> ", html_escape(who));
    if items.is_empty() {
        out.push_str("<em>(none)</em>");
    } else {
        for item in items.iter().take(5) {
            out.push_str(&format!(
                "<span>{} {}</span>&nbsp;&nbsp;",
                html_escape(&item.emoji),
                item.count
            ));
        }
    }
    out.push_str("</div>\n");
    out
}

fn export_rating_descriptor(score: i64) -> String {
    match score {
        90..=100 => "An exceptional relationship".into(),
        75..=89 => "A great relationship".into(),
        60..=74 => "A solid relationship".into(),
        45..=59 => "A casual connection".into(),
        25..=44 => "A loose tie".into(),
        _ => "An infrequent contact".into(),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn build_daily_heatmap_svg(daily: &[DailyActivityPoint]) -> String {
    use chrono::{Datelike, NaiveDate};
    let cell = 12.0;
    let gap = 2.0;
    let n = 500.min(daily.len());
    let recent: &[DailyActivityPoint] = &daily[daily.len().saturating_sub(n)..];
    if recent.is_empty() {
        return String::new();
    }
    let max_msgs = recent
        .iter()
        .map(|d| d.my_msgs + d.their_msgs)
        .max()
        .unwrap_or(1)
        .max(1) as f32;

    let first_date = NaiveDate::parse_from_str(&recent[0].day, "%Y-%m-%d").unwrap_or_default();
    let anchor = first_date
        - chrono::Duration::days(first_date.weekday().num_days_from_sunday() as i64);

    let mut cells = Vec::new();
    let mut min_col = usize::MAX;
    let mut max_col = 0usize;
    for d in recent {
        let Ok(date) = NaiveDate::parse_from_str(&d.day, "%Y-%m-%d") else {
            continue;
        };
        let days = (date - anchor).num_days();
        if days < 0 {
            continue;
        }
        let col = (days / 7) as usize;
        let row = date.weekday().num_days_from_sunday() as usize;
        let count = (d.my_msgs + d.their_msgs) as f32;
        let intensity = if count <= 0.0 {
            0.0
        } else {
            ((count + 1.0).ln() / (max_msgs + 1.0).ln()).clamp(0.0, 1.0)
        };
        if col < min_col {
            min_col = col;
        }
        if col > max_col {
            max_col = col;
        }
        cells.push((col, row, intensity, count as i64));
    }
    if cells.is_empty() {
        return String::new();
    }
    let cols = max_col - min_col + 1;
    let total_w = (cols as f32) * (cell + gap);
    let total_h = 7.0 * (cell + gap);

    let mut svg = format!(
        "<svg viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\">\n",
        total_w, total_h, total_w, total_h
    );
    // Background grid (dim)
    for r in 0..7 {
        for c in 0..cols {
            let x = (c as f32) * (cell + gap);
            let y = (r as f32) * (cell + gap);
            svg.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"2\" fill=\"#ebedf0\"/>\n",
                x, y, cell, cell
            ));
        }
    }
    // Active cells
    for (col, row, intensity, count) in cells {
        let c = col - min_col;
        let x = (c as f32) * (cell + gap);
        let y = (row as f32) * (cell + gap);
        let color = activity_hex(intensity);
        svg.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"2\" fill=\"{}\"><title>{} msgs</title></rect>\n",
            x, y, cell, cell, color, count
        ));
    }
    svg.push_str("</svg>\n");
    svg
}

fn build_hourly_heatmap_svg(hourly: &[HourlyActivityBucket]) -> String {
    let cell_w = 20.0;
    let cell_h = 22.0;
    let gap = 2.0;
    let label_w = 40.0;
    let max_count = hourly.iter().map(|h| h.message_count).max().unwrap_or(1).max(1) as f32;
    let mut grid = [[0i64; 24]; 7];
    for h in hourly {
        if (h.day_of_week as usize) < 7 && (h.hour as usize) < 24 {
            grid[h.day_of_week as usize][h.hour as usize] = h.message_count;
        }
    }
    let total_w = label_w + 24.0 * (cell_w + gap);
    let total_h = 18.0 + 7.0 * (cell_h + gap);
    let mut svg = format!(
        "<svg viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\">\n",
        total_w, total_h, total_w, total_h
    );

    // Hour labels every 3 hours.
    for hour in 0..24 {
        if hour % 3 == 0 {
            let x = label_w + (hour as f32) * (cell_w + gap) + cell_w / 2.0;
            svg.push_str(&format!(
                "<text x=\"{:.1}\" y=\"12\" font-size=\"10\" text-anchor=\"middle\" fill=\"#666\">{:02}</text>\n",
                x, hour
            ));
        }
    }

    let dow_labels = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    for dow in 0..7 {
        let row_top = 18.0 + (dow as f32) * (cell_h + gap);
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" text-anchor=\"end\" fill=\"#666\">{}</text>\n",
            label_w - 4.0,
            row_top + cell_h * 0.7,
            dow_labels[dow]
        ));
        for hour in 0..24 {
            let x = label_w + (hour as f32) * (cell_w + gap);
            let count = grid[dow][hour] as f32;
            let intensity = if count <= 0.0 {
                0.0
            } else {
                ((count + 1.0).ln() / (max_count + 1.0).ln()).clamp(0.0, 1.0)
            };
            let color = if count <= 0.0 {
                "#1a1a1a".to_string()
            } else {
                ironbow_hex(intensity)
            };
            svg.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"2\" fill=\"{}\"><title>{} msgs</title></rect>\n",
                x, row_top, cell_w, cell_h, color, count as i64
            ));
        }
    }
    svg.push_str("</svg>\n");
    svg
}

fn activity_hex(intensity: f32) -> String {
    let t = intensity.clamp(0.0, 1.0);
    let r = lerp_u8(45, 80, t);
    let g = lerp_u8(60, 200, t);
    let b = lerp_u8(45, 120, t);
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

fn ironbow_hex(intensity: f32) -> String {
    let c = ironbow_color(intensity);
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

fn build_growth_chart_svg(daily: &[DailyActivityPoint]) -> String {
    let width = 720.0;
    let height = 200.0;
    let pad_l = 50.0;
    let pad_r = 100.0;
    let pad_t = 16.0;
    let pad_b = 24.0;
    let plot_w = width - pad_l - pad_r;
    let plot_h = height - pad_t - pad_b;

    let mut my_cum = 0.0f64;
    let mut their_cum = 0.0f64;
    let mut my_pts: Vec<(f64, f64)> = Vec::new();
    let mut their_pts: Vec<(f64, f64)> = Vec::new();
    let mut total_pts: Vec<(f64, f64)> = Vec::new();
    for (i, d) in daily.iter().enumerate() {
        my_cum += d.my_points;
        their_cum += d.their_points;
        let x = i as f64;
        my_pts.push((x, my_cum));
        their_pts.push((x, their_cum));
        total_pts.push((x, my_cum + their_cum));
    }
    let max_x = (daily.len().saturating_sub(1)) as f64;
    let max_y = total_pts.last().map(|(_, y)| *y).unwrap_or(1.0).max(1.0);

    let to_svg = |x: f64, y: f64| -> (f64, f64) {
        let nx = if max_x > 0.0 { x / max_x } else { 0.5 };
        let ny = if max_y > 0.0 { 1.0 - y / max_y } else { 1.0 };
        (pad_l + nx * plot_w as f64, pad_t as f64 + ny * plot_h as f64)
    };

    let mut svg = format!(
        "<svg viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\">\n",
        width, height, width, height
    );
    svg.push_str(&format!(
        "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"#fafafa\" stroke=\"#ddd\" rx=\"4\"/>\n",
        pad_l, pad_t, plot_w, plot_h
    ));
    // Y labels
    svg.push_str(&format!(
        "<text x=\"{}\" y=\"{}\" font-size=\"10\" fill=\"#666\">{:.0}</text>\n",
        pad_l - 4.0, pad_t + 4.0, max_y
    ));
    svg.push_str(&format!(
        "<text x=\"{}\" y=\"{}\" font-size=\"10\" fill=\"#666\">0</text>\n",
        pad_l - 4.0, pad_t + plot_h
    ));

    let path_for = |pts: &[(f64, f64)]| -> String {
        let mut s = String::from("M");
        for (i, p) in pts.iter().enumerate() {
            let (x, y) = to_svg(p.0, p.1);
            if i == 0 {
                s.push_str(&format!("{:.1},{:.1}", x, y));
            } else {
                s.push_str(&format!(" L{:.1},{:.1}", x, y));
            }
        }
        s
    };
    svg.push_str(&format!(
        "<path d=\"{}\" fill=\"none\" stroke=\"#b4b4dc\" stroke-width=\"1.5\"/>\n",
        path_for(&total_pts)
    ));
    svg.push_str(&format!(
        "<path d=\"{}\" fill=\"none\" stroke=\"#5082c8\" stroke-width=\"1.5\"/>\n",
        path_for(&my_pts)
    ));
    svg.push_str(&format!(
        "<path d=\"{}\" fill=\"none\" stroke=\"#50c8a0\" stroke-width=\"1.5\"/>\n",
        path_for(&their_pts)
    ));
    // Legend on right
    let lx = width - pad_r + 8.0;
    svg.push_str(&format!("<text x=\"{}\" y=\"{}\" font-size=\"11\" fill=\"#b4b4dc\">— Total</text>\n", lx, pad_t + 14.0));
    svg.push_str(&format!("<text x=\"{}\" y=\"{}\" font-size=\"11\" fill=\"#5082c8\">— You</text>\n", lx, pad_t + 28.0));
    svg.push_str(&format!("<text x=\"{}\" y=\"{}\" font-size=\"11\" fill=\"#50c8a0\">— Them</text>\n", lx, pad_t + 42.0));
    // X-axis labels
    if let (Some(first), Some(last)) = (daily.first(), daily.last()) {
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"10\" fill=\"#666\">{}</text>\n",
            pad_l, height - 4.0, html_escape(&first.day)
        ));
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"10\" fill=\"#666\" text-anchor=\"end\">{}</text>\n",
            pad_l + plot_w, height - 4.0, html_escape(&last.day)
        ));
    }
    svg.push_str("</svg>\n");
    svg
}

fn build_sentiment_chart_svg(days: &[SentimentDayLoaded]) -> String {
    let width = 720.0;
    let height = 180.0;
    let pad_l = 40.0;
    let pad_r = 80.0;
    let pad_t = 16.0;
    let pad_b = 22.0;
    let plot_w = width - pad_l - pad_r;
    let plot_h = height - pad_t - pad_b;

    let mut min_y = -0.5f64;
    let mut max_y = 0.5f64;
    for d in days {
        for opt in [d.my_score, d.their_score] {
            if let Some(v) = opt {
                if v < min_y { min_y = v; }
                if v > max_y { max_y = v; }
            }
        }
    }
    let span = (max_y - min_y).max(1.0);
    let to_svg = |i: usize, y: f64| -> (f64, f64) {
        let nx = if days.len() > 1 { i as f64 / (days.len() - 1) as f64 } else { 0.5 };
        let ny = 1.0 - (y - min_y) / span;
        (pad_l + nx * plot_w, pad_t + ny * plot_h)
    };

    let mut svg = format!(
        "<svg viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\">\n",
        width, height, width, height
    );
    svg.push_str(&format!(
        "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"#fafafa\" stroke=\"#ddd\" rx=\"4\"/>\n",
        pad_l, pad_t, plot_w, plot_h
    ));
    // Zero line.
    let zy = pad_t + (1.0 - (0.0 - min_y) / span) * plot_h;
    svg.push_str(&format!(
        "<line x1=\"{}\" y1=\"{:.1}\" x2=\"{}\" y2=\"{:.1}\" stroke=\"#aaa\" stroke-dasharray=\"2 2\"/>\n",
        pad_l, zy, pad_l + plot_w, zy
    ));
    // Y labels.
    svg.push_str(&format!("<text x=\"{}\" y=\"{:.1}\" font-size=\"10\" fill=\"#666\">{:+.1}</text>\n", pad_l - 4.0, pad_t + 4.0, max_y));
    svg.push_str(&format!("<text x=\"{}\" y=\"{:.1}\" font-size=\"10\" fill=\"#666\">0</text>\n", pad_l - 4.0, zy));
    svg.push_str(&format!("<text x=\"{}\" y=\"{:.1}\" font-size=\"10\" fill=\"#666\">{:+.1}</text>\n", pad_l - 4.0, pad_t + plot_h, min_y));

    let path_for = |pts: &[(f64, f64)]| -> Option<String> {
        if pts.len() < 2 { return None; }
        let mut s = String::from("M");
        for (i, p) in pts.iter().enumerate() {
            let (x, y) = to_svg(p.0 as usize, p.1);
            if i == 0 {
                s.push_str(&format!("{:.1},{:.1}", x, y));
            } else {
                s.push_str(&format!(" L{:.1},{:.1}", x, y));
            }
        }
        Some(s)
    };
    let my_pts: Vec<(f64, f64)> = days
        .iter()
        .enumerate()
        .filter_map(|(i, d)| d.my_score.map(|s| (i as f64, s)))
        .collect();
    let their_pts: Vec<(f64, f64)> = days
        .iter()
        .enumerate()
        .filter_map(|(i, d)| d.their_score.map(|s| (i as f64, s)))
        .collect();
    if let Some(d) = path_for(&my_pts) {
        svg.push_str(&format!(
            "<path d=\"{}\" fill=\"none\" stroke=\"#5082c8\" stroke-width=\"1.5\"/>\n",
            d
        ));
    }
    if let Some(d) = path_for(&their_pts) {
        svg.push_str(&format!(
            "<path d=\"{}\" fill=\"none\" stroke=\"#50c8a0\" stroke-width=\"1.5\"/>\n",
            d
        ));
    }
    let lx = width - pad_r + 8.0;
    svg.push_str(&format!("<text x=\"{}\" y=\"{}\" font-size=\"11\" fill=\"#5082c8\">— You</text>\n", lx, pad_t + 14.0));
    svg.push_str(&format!("<text x=\"{}\" y=\"{}\" font-size=\"11\" fill=\"#50c8a0\">— Them</text>\n", lx, pad_t + 28.0));
    if let (Some(first), Some(last)) = (days.first(), days.last()) {
        svg.push_str(&format!("<text x=\"{}\" y=\"{}\" font-size=\"10\" fill=\"#666\">{}</text>\n", pad_l, height - 4.0, html_escape(&first.day)));
        svg.push_str(&format!("<text x=\"{}\" y=\"{}\" font-size=\"10\" fill=\"#666\" text-anchor=\"end\">{}</text>\n", pad_l + plot_w, height - 4.0, html_escape(&last.day)));
    }
    svg.push_str("</svg>\n");
    svg
}

fn build_direction_donut_svg(me: f64, them: f64, other: f64) -> String {
    let size = 220.0;
    let cx = size / 2.0;
    let cy = size / 2.0;
    let r = size * 0.42;
    let stroke = size * 0.16;
    let total = (me + them + other).max(1e-9);
    let me_f = me / total;
    let them_f = them / total;
    let other_f = other / total;

    let mut svg = format!(
        "<svg viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\">\n",
        size, size, size, size
    );
    let mut start = -std::f64::consts::FRAC_PI_2;
    for (frac, color) in [
        (me_f, "#5082c8"),
        (them_f, "#50c8a0"),
        (other_f, "#b4b4b4"),
    ] {
        if frac <= 0.0 { continue; }
        let end = start + std::f64::consts::TAU * frac;
        let (sx, sy) = (cx + r * start.cos(), cy + r * start.sin());
        let (ex, ey) = (cx + r * end.cos(), cy + r * end.sin());
        let large = if (end - start) > std::f64::consts::PI { 1 } else { 0 };
        svg.push_str(&format!(
            "<path d=\"M{:.1},{:.1} A{:.1},{:.1} 0 {} 1 {:.1},{:.1}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{:.1}\"/>\n",
            sx, sy, r, r, large, ex, ey, color, stroke
        ));
        start = end;
    }
    // Legend
    svg.push_str(&format!("<text x=\"{}\" y=\"{}\" font-size=\"11\" fill=\"#5082c8\">● You: {:.1}%</text>\n", cx + r + 16.0, cy - 14.0, me_f * 100.0));
    svg.push_str(&format!("<text x=\"{}\" y=\"{}\" font-size=\"11\" fill=\"#50c8a0\">● Them: {:.1}%</text>\n", cx + r + 16.0, cy, them_f * 100.0));
    svg.push_str(&format!("<text x=\"{}\" y=\"{}\" font-size=\"11\" fill=\"#888\">● Others: {:.1}%</text>\n", cx + r + 16.0, cy + 14.0, other_f * 100.0));
    svg.push_str("</svg>\n");
    svg
}

fn build_sankey_svg(data: &SankeyData) -> String {
    let width = 800.0;
    let height = 320.0;
    let pad_x = 40.0;
    let pad_y = 20.0;
    let plot_w = width - 2.0 * pad_x;
    let plot_h = height - 2.0 * pad_y;

    let max_col = data.nodes.iter().map(|n| n.column).max().unwrap_or(0) as usize + 1;
    let mut by_col: Vec<Vec<&SankeyNode>> = vec![Vec::new(); max_col];
    for n in &data.nodes {
        by_col[n.column as usize].push(n);
    }

    struct NB { x: f64, y: f64, w: f64, h: f64, value: u32, label: String, column: u8 }
    let mut boxes: std::collections::HashMap<String, NB> = std::collections::HashMap::new();

    let col_step = if max_col > 1 { plot_w / (max_col as f64 - 1.0) } else { plot_w };
    let node_w = (col_step * 0.18).clamp(30.0, 70.0);

    for (ci, col) in by_col.iter().enumerate() {
        let total: u32 = col.iter().map(|n| n.value).sum();
        if total == 0 { continue; }
        let gap = 8.0;
        let usable_h = plot_h - gap * col.len().saturating_sub(1) as f64;
        let mut y = pad_y;
        let cx = pad_x + ci as f64 * col_step;
        let xl = cx - node_w / 2.0;

        let mut sorted: Vec<&SankeyNode> = col.iter().copied().collect();
        sorted.sort_by(|a, b| a.id.cmp(&b.id));

        for n in sorted {
            let h = usable_h * n.value as f64 / total as f64;
            boxes.insert(n.id.clone(), NB {
                x: xl, y, w: node_w, h: h.max(2.0),
                value: n.value, label: n.label.clone(), column: n.column,
            });
            y += h + gap;
        }
    }

    let mut svg = format!(
        "<svg viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\" style=\"background:#fafafa\">\n",
        width, height, width, height
    );

    // Links.
    let mut src_off: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut tgt_off: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for link in &data.links {
        let (Some(src), Some(tgt)) = (boxes.get(&link.source), boxes.get(&link.target)) else { continue };
        let src_h = src.h * link.value as f64 / src.value.max(1) as f64;
        let tgt_h = tgt.h * link.value as f64 / tgt.value.max(1) as f64;
        let s_top = *src_off.entry(link.source.clone()).or_insert(0.0);
        let t_top = *tgt_off.entry(link.target.clone()).or_insert(0.0);
        let sy0 = src.y + s_top;
        let sy1 = sy0 + src_h;
        let ty0 = tgt.y + t_top;
        let ty1 = ty0 + tgt_h;
        let sx = src.x + src.w;
        let tx = tgt.x;
        let cx = (sx + tx) / 2.0;
        let color = sankey_link_color_html(&link.source);
        // Path: top edge bezier, then bottom edge bezier (reversed), close.
        svg.push_str(&format!(
            "<path d=\"M{:.1},{:.1} C{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} L{:.1},{:.1} C{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} Z\" fill=\"{}\" opacity=\"0.5\"/>\n",
            sx, sy0, cx, sy0, cx, ty0, tx, ty0,
            tx, ty1, cx, ty1, cx, sy1, sx, sy1,
            color
        ));
        *src_off.entry(link.source.clone()).or_insert(0.0) += src_h;
        *tgt_off.entry(link.target.clone()).or_insert(0.0) += tgt_h;
    }

    // Nodes.
    for (_, b) in &boxes {
        let color = sankey_node_color_html(b.column);
        svg.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"{}\" rx=\"2\"/>\n",
            b.x, b.y, b.w, b.h, color
        ));
        let (lx, anchor) = if b.column == 0 {
            (b.x - 4.0, "end")
        } else if b.column as usize == max_col - 1 {
            (b.x + b.w + 4.0, "start")
        } else {
            (b.x + b.w / 2.0, "middle")
        };
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" text-anchor=\"{}\" fill=\"#222\">{}</text>\n",
            lx, b.y + b.h / 2.0 - 4.0, anchor, html_escape(&b.label)
        ));
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" text-anchor=\"{}\" fill=\"#666\">{}</text>\n",
            lx, b.y + b.h / 2.0 + 8.0, anchor, b.value
        ));
    }

    svg.push_str("</svg>\n");
    svg
}

fn sankey_link_color_html(source_id: &str) -> &'static str {
    if source_id.starts_with("started_me") { "#5082c8" }
    else if source_id.starts_with("started_them") { "#50c8a0" }
    else if source_id.starts_with("big_moment") { "#dca03c" }
    else if source_id.starts_with("everyday") { "#78a0c8" }
    else if source_id.starts_with("no_reply") { "#dc5050" }
    else if source_id.starts_with("contrib_me") { "#5082c8" }
    else if source_id.starts_with("contrib_them") { "#50c8a0" }
    else { "#999" }
}

fn sankey_node_color_html(column: u8) -> &'static str {
    match column {
        0 => "#466eaa",
        1 => "#aa8246",
        2 => "#6ea082",
        3 => "#aa6e8c",
        _ => "#888",
    }
}

// ===================================================================
// DASHBOARD PANELS
// ===================================================================

fn render_dashboard(app: &mut SmsArchiveApp, ui: &mut egui::Ui, l: &LoadedAnalytics) {
    render_header_kpi_strip(ui, l);
    ui.add_space(10.0);
    render_chat_rating_summary(ui, l);
    ui.add_space(10.0);
    render_insights_panel(app, ui);
    ui.add_space(10.0);

    ui.columns(2, |cols| {
        render_message_analysis_panel(&mut cols[0], l);
        render_language_analysis_panel(&mut cols[1], l);
    });
    ui.add_space(8.0);

    ui.columns(2, |cols| {
        render_media_stats_panel(&mut cols[0], l);
        render_responding_panel(&mut cols[1], l);
    });
    ui.add_space(8.0);

    ui.columns(2, |cols| {
        render_conversation_analysis_panel(&mut cols[0], l);
        render_summary_of_writing_panel(&mut cols[1], l);
    });
    ui.add_space(8.0);

    render_relationship_growth_panel(ui, l);
    ui.add_space(8.0);

    render_sentiment_panel(ui, l);
    ui.add_space(8.0);

    ui.columns(2, |cols| {
        render_streak_panel(&mut cols[0], l);
        render_direction_donut_panel(&mut cols[1], l);
    });
    ui.add_space(8.0);

    render_topics_panel(ui, l);
    ui.add_space(8.0);

    render_inside_jokes_panel(ui, l);
    ui.add_space(8.0);

    ui.columns(2, |cols| {
        render_messaging_times_panel(&mut cols[0], l);
        render_daily_activity_panel(&mut cols[1], l);
    });
    ui.add_space(8.0);

    render_sankey_panel(ui, l);
}

// ---------- Header KPI ----------

fn render_header_kpi_strip(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.columns(4, |cols| {
        kpi_card(
            &mut cols[0],
            "Chat Points",
            format_with_commas((l.my_points + l.their_points).round() as i64),
            None,
        );
        let span_label = match (l.first_message_ms, l.last_message_ms) {
            (Some(first), Some(last)) if last > first => {
                let days = (last - first) as f64 / (24.0 * 60.0 * 60.0 * 1000.0);
                if days >= 365.0 {
                    format!("{:.1} years", days / 365.0)
                } else {
                    format!("{:.0} days", days)
                }
            }
            _ => "—".to_string(),
        };
        kpi_card(&mut cols[1], "Time Period", span_label, None);
        kpi_card(
            &mut cols[2],
            "Messages",
            format_with_commas(l.my_messages + l.their_messages),
            Some(format!(
                "you {} • them {}",
                format_with_commas(l.my_messages),
                format_with_commas(l.their_messages)
            )),
        );
        kpi_card(
            &mut cols[3],
            "Conversations",
            format_with_commas(l.total_conversations),
            None,
        );
    });
}

fn kpi_card(ui: &mut egui::Ui, label: &str, value: String, sub: Option<String>) {
    ui.group(|ui| {
        ui.vertical(|ui| {
            ui.weak(label);
            ui.label(egui::RichText::new(value).heading().size(20.0));
            if let Some(s) = sub {
                ui.weak(s);
            }
        });
    });
}

// ---------- Chat Rating ----------

fn render_chat_rating_summary(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Chat Rating");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            // Left: the gauge.
            if let Some(score) = l.overall_score {
                paint_rating_gauge(ui, score, 140.0);
            } else {
                ui.allocate_space(egui::vec2(140.0, 140.0));
            }

            ui.add_space(16.0);

            // Right: the components grid + descriptor.
            ui.vertical(|ui| {
                if let Some(score) = l.overall_score {
                    ui.label(
                        egui::RichText::new(rating_descriptor(score))
                            .size(16.0)
                            .strong(),
                    );
                } else {
                    ui.label("Insufficient data to compute rating.");
                }
                ui.add_space(4.0);

                egui::Grid::new("chat_rating_components")
                    .num_columns(2)
                    .spacing([20.0, 4.0])
                    .show(ui, |ui| {
                        rating_row(ui, "Responsiveness", l.score_responsiveness);
                        rating_row(ui, "Balance", l.score_balance);
                        rating_row(ui, "Engagement", l.score_engagement);
                        rating_row(ui, "Consistency", l.score_consistency);
                        rating_row(ui, "Reciprocity", l.score_reciprocity);
                        rating_row(ui, "Longevity", l.score_longevity);
                        rating_row(ui, "Mutual Effort", l.score_mutual_effort);
                    });
            });
        });

        ui.add_space(6.0);
        ui.label(egui::RichText::new("Balance — message volume").strong());
        paint_balance_bar(
            ui,
            l.my_messages,
            l.their_messages,
            "you",
            "them",
            22.0,
        );
    });
}

fn rating_row(ui: &mut egui::Ui, label: &str, value: Option<i64>) {
    ui.label(label);
    match value {
        Some(v) => ui.label(format!("{}", v)),
        None => ui.weak("—"),
    };
    ui.end_row();
}

fn rating_descriptor(score: i64) -> String {
    match score {
        90..=100 => "An exceptional relationship".into(),
        75..=89 => "A great relationship".into(),
        60..=74 => "A solid relationship".into(),
        45..=59 => "A casual connection".into(),
        25..=44 => "A loose tie".into(),
        _ => "An infrequent contact".into(),
    }
}

// ---------- Insights ----------

fn render_insights_panel(app: &mut SmsArchiveApp, ui: &mut egui::Ui) {
    let Some(loaded) = app.analytics.loaded.as_ref() else {
        return;
    };
    if loaded.insights.is_empty() {
        return;
    }

    // Group insights by category in their existing order (orchestrator already
    // sorted by category → confidence → tier).
    let mut your_side: Vec<&LoadedInsight> = Vec::new();
    let mut their_side: Vec<&LoadedInsight> = Vec::new();
    let mut shared: Vec<&LoadedInsight> = Vec::new();
    for ins in &loaded.insights {
        match ins.category.as_str() {
            "YourSide" => your_side.push(ins),
            "TheirSide" => their_side.push(ins),
            "Shared" => shared.push(ins),
            _ => shared.push(ins),
        }
    }

    let total = loaded.insights.len();
    const PER_CAT_DEFAULT: usize = 3;
    let show_all = app.analytics.insights_show_all;
    let visible_cap = if show_all { usize::MAX } else { PER_CAT_DEFAULT };
    let hidden = total.saturating_sub(
        your_side.len().min(visible_cap)
            + their_side.len().min(visible_cap)
            + shared.len().min(visible_cap),
    );

    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.heading("Key Insights");
            ui.add_space(8.0);
            ui.weak(format!("{} total", total));
        });
        ui.add_space(4.0);

        for group in [&your_side, &their_side, &shared] {
            for insight in group.iter().take(visible_cap) {
                render_one_insight(ui, insight);
            }
        }

        // Show-more / show-less toggle.
        if total > PER_CAT_DEFAULT * 3 || hidden > 0 {
            ui.add_space(4.0);
            let label = if show_all {
                "▲ Show fewer".to_string()
            } else {
                format!("▼ Show all ({} more hidden)", hidden)
            };
            if ui.button(label).clicked() {
                app.analytics.insights_show_all = !show_all;
            }
        }
    });
}

fn render_one_insight(ui: &mut egui::Ui, insight: &LoadedInsight) {
    let bg = insight_color(insight);
    let icon_size = 14.0 + (insight.tier as f32) * 2.5;
    egui::Frame::none()
        .fill(bg)
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(80)))
        .rounding(4.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(insight.icon.to_string()).size(icon_size));
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(&insight.headline).strong());
                    if !insight.detail.is_empty() {
                        ui.weak(&insight.detail);
                    }
                    if insight.confidence == "Low" {
                        ui.colored_label(
                            egui::Color32::from_rgb(180, 180, 90),
                            "(limited data)",
                        );
                    }
                });
            });
        });
    ui.add_space(4.0);
}

fn insight_color(insight: &LoadedInsight) -> egui::Color32 {
    // Tint by category. Tier brightness is implicit through opacity, but we
    // keep it subtle so the dashboard stays readable.
    match insight.category.as_str() {
        "YourSide" => egui::Color32::from_rgb(35, 55, 75),
        "TheirSide" => egui::Color32::from_rgb(55, 55, 35),
        "Shared" => egui::Color32::from_rgb(40, 50, 40),
        _ => egui::Color32::from_rgb(40, 40, 40),
    }
}

// ---------- Message Analysis ----------

fn render_message_analysis_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Message Analysis");
        side_by_side_grid(ui, "msg_analysis", &[
            ("Messages", format_with_commas(l.my_messages), format_with_commas(l.their_messages)),
            ("Words", format_with_commas(l.my_words), format_with_commas(l.their_words)),
            ("Unique words", format_with_commas(l.my_unique_words), format_with_commas(l.their_unique_words)),
            ("Characters", format_with_commas(l.my_chars), format_with_commas(l.their_chars)),
        ]);
    });
}

// ---------- Media Stats ----------

fn render_media_stats_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Media Stats");
        side_by_side_grid(ui, "media_stats", &[
            ("Images", format_with_commas(l.my_images), format_with_commas(l.their_images)),
            ("Videos", format_with_commas(l.my_videos), format_with_commas(l.their_videos)),
            ("Audios", format_with_commas(l.my_audios), format_with_commas(l.their_audios)),
            ("GIFs", format_with_commas(l.my_gifs), format_with_commas(l.their_gifs)),
            ("Links", format_with_commas(l.my_links), format_with_commas(l.their_links)),
        ]);
    });
}

// ---------- Conversation Analysis ----------

fn render_conversation_analysis_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Conversation Analysis");
        side_by_side_grid(ui, "conv_analysis", &[
            (
                "Convos started",
                format_with_commas(l.started_by_me),
                format_with_commas(l.started_by_them),
            ),
            (
                "Convos closed",
                format_with_commas(l.closed_by_me),
                format_with_commas(l.closed_by_them),
            ),
            (
                "Convos missed",
                format_with_commas(l.my_missed),
                format_with_commas(l.their_missed),
            ),
            (
                "Double messages",
                format_with_commas(l.my_doubles),
                format_with_commas(l.their_doubles),
            ),
        ]);
        ui.add_space(4.0);
        let top = match l.top_contributor {
            Some(1) => "you",
            Some(2) => "them",
            _ => "tied",
        };
        ui.label(format!("Top contributor: {}", top));
        ui.label(format!(
            "Avg convo points: {:.0} • Median convo length: {:.0} msgs",
            l.avg_convo_points, l.median_convo_messages
        ));
        ui.label(format!(
            "Reconnects (24h / 7d / 30d / 3× pair-median): {} / {} / {} / {}",
            l.reconnect_t1, l.reconnect_t2, l.reconnect_t3, l.reconnect_t4
        ));
    });
}

// ---------- Language Analysis ----------

fn render_language_analysis_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Language Analysis");
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Top emojis").strong());
        emoji_row(ui, "You", &l.my_top_emojis);
        emoji_row(ui, "Them", &l.their_top_emojis);
        ui.add_space(6.0);
        side_by_side_grid(ui, "language_grid", &[
            (
                "Emojis (total)",
                format_with_commas(l.my_emoji_total),
                format_with_commas(l.their_emoji_total),
            ),
            (
                "Laughs",
                format_with_commas(l.my_laughs),
                format_with_commas(l.their_laughs),
            ),
            (
                "Apologies",
                format_with_commas(l.my_apologies),
                format_with_commas(l.their_apologies),
            ),
            (
                "Questions",
                format_with_commas(l.my_questions),
                format_with_commas(l.their_questions),
            ),
            (
                "Encouragement",
                format_with_commas(l.my_encouragement),
                format_with_commas(l.their_encouragement),
            ),
        ]);
    });
}

fn emoji_row(ui: &mut egui::Ui, who: &str, items: &[EmojiCount]) {
    if items.is_empty() {
        ui.horizontal(|ui| {
            ui.label(format!("{}:", who));
            ui.weak("(none)");
        });
        return;
    }
    ui.horizontal(|ui| {
        ui.label(format!("{}:", who));
        for item in items.iter().take(5) {
            ui.label(
                egui::RichText::new(format!("{} {}", item.emoji, item.count))
                    .size(16.0),
            );
        }
    });
}

// ---------- Responding ----------

fn render_responding_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Responding");
        side_by_side_grid(ui, "responding_grid", &[
            (
                "Median response",
                fmt_ms_or_dash(l.my_median_resp_ms),
                fmt_ms_or_dash(l.their_median_resp_ms),
            ),
            (
                "Mean response",
                fmt_ms_or_dash(l.my_mean_resp_ms),
                fmt_ms_or_dash(l.their_mean_resp_ms),
            ),
            (
                "Rapid response",
                fmt_pct_or_dash(l.my_rapid_pct),
                fmt_pct_or_dash(l.their_rapid_pct),
            ),
            (
                "Median 1st response",
                fmt_ms_or_dash(l.my_first_median_ms),
                fmt_ms_or_dash(l.their_first_median_ms),
            ),
            (
                "Awake median",
                fmt_ms_or_dash(l.my_awake_median_ms),
                fmt_ms_or_dash(l.their_awake_median_ms),
            ),
            (
                "Overnight median",
                fmt_ms_or_dash(l.my_overnight_median_ms),
                fmt_ms_or_dash(l.their_overnight_median_ms),
            ),
        ]);
    });
}

// ---------- Summary of Writing ----------

fn render_summary_of_writing_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Summary of Writing");
        if let Some(m) = &l.writing_milestones {
            ui.label(format!(
                "Total characters typed: {}",
                format_with_commas(m.total_chars as i64)
            ));
            ui.label(format!(
                "Total words: {}",
                format_with_commas(m.total_words as i64)
            ));
            ui.label(format!(
                "Harry Potter book equivalents: {:.2}",
                m.harry_potter_equivalents
            ));
        } else {
            ui.weak("(no milestone data)");
        }
    });
}

// ===================================================================
// SHARED UI HELPERS
// ===================================================================

fn side_by_side_grid(ui: &mut egui::Ui, id: &str, rows: &[(&str, String, String)]) {
    egui::Grid::new(id)
        .num_columns(3)
        .spacing([20.0, 4.0])
        .min_col_width(120.0)
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Metric").strong());
            ui.label(egui::RichText::new("You").strong());
            ui.label(egui::RichText::new("Them").strong());
            ui.end_row();
            for (label, mine, theirs) in rows {
                ui.label(*label);
                ui.label(mine);
                ui.label(theirs);
                ui.end_row();
            }
        });
}

fn fmt_ms_or_dash(ms: Option<i64>) -> String {
    match ms {
        Some(m) => humanize_ms(m),
        None => "—".to_string(),
    }
}

fn fmt_pct_or_dash(p: Option<f64>) -> String {
    match p {
        Some(v) => format!("{:.1}%", v * 100.0),
        None => "—".to_string(),
    }
}

fn format_with_commas(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i != 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    let mut formatted: String = out.chars().rev().collect();
    if n < 0 {
        formatted.insert(0, '-');
    }
    formatted
}

fn humanize_ms(ms: i64) -> String {
    if ms < 60_000 {
        format!("{}s", ms / 1_000)
    } else if ms < 60 * 60_000 {
        format!("{}m", ms / 60_000)
    } else if ms < 24 * 60 * 60_000 {
        format!("{}h", ms / (60 * 60_000))
    } else {
        format!("{}d", ms / (24 * 60 * 60_000))
    }
}

fn current_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn humanize_seconds_ago(secs: i64) -> String {
    if secs < 60 {
        return "just now".into();
    }
    if secs < 60 * 60 {
        return format!("{} min ago", secs / 60);
    }
    if secs < 24 * 60 * 60 {
        return format!("{} hr ago", secs / 3_600);
    }
    let days = secs / 86_400;
    if days == 1 {
        return "1 day ago".into();
    }
    format!("{} days ago", days)
}

// ===================================================================
// SETTINGS — load + save against analytics_meta (Phase 3.5)
// ===================================================================

fn load_settings_from_db(db_path: &str) -> Result<AnalyticsSettings, String> {
    let db = Database::open(Path::new(db_path), ResourceProfile::detect())
        .map_err(|e| format!("open db: {}", e))?;
    let conn = db.connection();
    let mut stmt = conn
        .prepare("SELECT key, value FROM analytics_meta")
        .map_err(|e| format!("prepare: {}", e))?;
    let mut map = std::collections::HashMap::<String, String>::new();
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| format!("query: {}", e))?;
    for r in rows.flatten() {
        map.insert(r.0, r.1);
    }

    let mut s = AnalyticsSettings::default();
    macro_rules! get_f64 { ($field:ident, $key:literal) => {
        if let Some(v) = map.get($key).and_then(|x| x.parse::<f64>().ok()) {
            s.$field = v;
        }
    }; }
    macro_rules! get_i64 { ($field:ident, $key:literal) => {
        if let Some(v) = map.get($key).and_then(|x| x.parse::<i64>().ok()) {
            s.$field = v;
        }
    }; }
    macro_rules! get_u32 { ($field:ident, $key:literal) => {
        if let Some(v) = map.get($key).and_then(|x| x.parse::<u32>().ok()) {
            s.$field = v;
        }
    }; }
    macro_rules! get_u8  { ($field:ident, $key:literal) => {
        if let Some(v) = map.get($key).and_then(|x| x.parse::<u8>().ok()) {
            s.$field = v;
        }
    }; }

    get_i64!(conversation_timeout_secs, "conversation_timeout_secs");
    get_u32!(big_moment_threshold_static, "big_moment_threshold_static");
    get_u8!(big_moment_threshold_dynamic_pct, "big_moment_threshold_dynamic_pct");
    get_u32!(big_moment_threshold_dynamic_floor, "big_moment_threshold_dynamic_floor");
    get_i64!(reconnect_tier1_secs, "reconnect_tier1_secs");
    get_i64!(reconnect_tier2_secs, "reconnect_tier2_secs");
    get_i64!(reconnect_tier3_secs, "reconnect_tier3_secs");
    get_f64!(reconnect_tier4_multiplier, "reconnect_tier4_multiplier");

    get_i64!(rapid_response_threshold_secs, "rapid_response_threshold_secs");
    get_u8!(overnight_window_start_hour, "overnight_window_start_hour");
    get_u8!(overnight_window_end_hour, "overnight_window_end_hour");

    get_f64!(weight_text_message, "weight_text_message");
    get_f64!(weight_per_word_log, "weight_per_word_log");
    get_f64!(weight_emoji, "weight_emoji");
    get_f64!(weight_question, "weight_question");
    get_f64!(weight_image, "weight_image");
    get_f64!(weight_video, "weight_video");
    get_f64!(weight_audio, "weight_audio");
    get_f64!(weight_gif, "weight_gif");
    get_f64!(weight_link, "weight_link");
    get_f64!(weight_started_convo, "weight_started_convo");
    get_f64!(weight_rapid_response, "weight_rapid_response");
    get_f64!(weight_encouragement, "weight_encouragement");
    get_f64!(weight_apology, "weight_apology");

    get_f64!(rating_weight_responsiveness, "rating_weight_responsiveness");
    get_f64!(rating_weight_balance, "rating_weight_balance");
    get_f64!(rating_weight_engagement, "rating_weight_engagement");
    get_f64!(rating_weight_consistency, "rating_weight_consistency");
    get_f64!(rating_weight_reciprocity, "rating_weight_reciprocity");
    get_f64!(rating_weight_longevity, "rating_weight_longevity");
    get_f64!(rating_weight_mutual_effort, "rating_weight_mutual_effort");

    get_u32!(rating_hide_below_messages, "rating_hide_below_messages");
    get_u32!(rating_low_confidence_max_messages, "rating_low_confidence_max_messages");

    get_f64!(insight_tier1_ratio, "insight_tier1_ratio");
    get_f64!(insight_tier2_ratio, "insight_tier2_ratio");
    get_f64!(insight_tier3_ratio, "insight_tier3_ratio");
    get_f64!(insight_tier4_ratio, "insight_tier4_ratio");
    get_u32!(insight_min_sample_per_rule, "insight_min_sample_per_rule");
    get_u32!(insight_low_confidence_max_sample, "insight_low_confidence_max_sample");

    get_u32!(contact_picker_min_messages, "contact_picker_min_messages");

    Ok(s)
}

fn save_settings_to_db(db_path: &str, s: &AnalyticsSettings) -> Result<(), String> {
    let db = Database::open(Path::new(db_path), ResourceProfile::detect())
        .map_err(|e| format!("open db: {}", e))?;
    let conn = db.connection();

    let pairs: Vec<(&'static str, String)> = vec![
        ("conversation_timeout_secs", s.conversation_timeout_secs.to_string()),
        ("big_moment_threshold_static", s.big_moment_threshold_static.to_string()),
        ("big_moment_threshold_dynamic_pct", s.big_moment_threshold_dynamic_pct.to_string()),
        ("big_moment_threshold_dynamic_floor", s.big_moment_threshold_dynamic_floor.to_string()),
        ("reconnect_tier1_secs", s.reconnect_tier1_secs.to_string()),
        ("reconnect_tier2_secs", s.reconnect_tier2_secs.to_string()),
        ("reconnect_tier3_secs", s.reconnect_tier3_secs.to_string()),
        ("reconnect_tier4_multiplier", format!("{}", s.reconnect_tier4_multiplier)),
        ("rapid_response_threshold_secs", s.rapid_response_threshold_secs.to_string()),
        ("overnight_window_start_hour", s.overnight_window_start_hour.to_string()),
        ("overnight_window_end_hour", s.overnight_window_end_hour.to_string()),
        ("weight_text_message", format!("{}", s.weight_text_message)),
        ("weight_per_word_log", format!("{}", s.weight_per_word_log)),
        ("weight_emoji", format!("{}", s.weight_emoji)),
        ("weight_question", format!("{}", s.weight_question)),
        ("weight_image", format!("{}", s.weight_image)),
        ("weight_video", format!("{}", s.weight_video)),
        ("weight_audio", format!("{}", s.weight_audio)),
        ("weight_gif", format!("{}", s.weight_gif)),
        ("weight_link", format!("{}", s.weight_link)),
        ("weight_started_convo", format!("{}", s.weight_started_convo)),
        ("weight_rapid_response", format!("{}", s.weight_rapid_response)),
        ("weight_encouragement", format!("{}", s.weight_encouragement)),
        ("weight_apology", format!("{}", s.weight_apology)),
        ("rating_weight_responsiveness", format!("{}", s.rating_weight_responsiveness)),
        ("rating_weight_balance", format!("{}", s.rating_weight_balance)),
        ("rating_weight_engagement", format!("{}", s.rating_weight_engagement)),
        ("rating_weight_consistency", format!("{}", s.rating_weight_consistency)),
        ("rating_weight_reciprocity", format!("{}", s.rating_weight_reciprocity)),
        ("rating_weight_longevity", format!("{}", s.rating_weight_longevity)),
        ("rating_weight_mutual_effort", format!("{}", s.rating_weight_mutual_effort)),
        ("rating_hide_below_messages", s.rating_hide_below_messages.to_string()),
        ("rating_low_confidence_max_messages", s.rating_low_confidence_max_messages.to_string()),
        ("insight_tier1_ratio", format!("{}", s.insight_tier1_ratio)),
        ("insight_tier2_ratio", format!("{}", s.insight_tier2_ratio)),
        ("insight_tier3_ratio", format!("{}", s.insight_tier3_ratio)),
        ("insight_tier4_ratio", format!("{}", s.insight_tier4_ratio)),
        ("insight_min_sample_per_rule", s.insight_min_sample_per_rule.to_string()),
        ("insight_low_confidence_max_sample", s.insight_low_confidence_max_sample.to_string()),
        ("contact_picker_min_messages", s.contact_picker_min_messages.to_string()),
    ];

    conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")
        .map_err(|e| format!("begin: {}", e))?;
    let result: rusqlite::Result<()> = (|| {
        let mut stmt = conn.prepare_cached(
            "INSERT INTO analytics_meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )?;
        for (k, v) in &pairs {
            stmt.execute(params![k, v])?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => conn
            .execute_batch("COMMIT")
            .map_err(|e| format!("commit: {}", e))?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(format!("upsert: {}", e));
        }
    }
    Ok(())
}

/// Mark all contacts' analytics caches stale. Called when the user clicks
/// "Save & Recompute" so the next view of any contact will re-run the
/// pipeline with the new weights.
fn mark_all_caches_stale(db_path: &str) -> Result<(), String> {
    let db = Database::open(Path::new(db_path), ResourceProfile::detect())
        .map_err(|e| format!("open db: {}", e))?;
    let conn = db.connection();
    conn.execute(
        "UPDATE contact_analytics_status SET is_stale = 1",
        [],
    )
    .map_err(|e| format!("update: {}", e))?;
    Ok(())
}

fn render_settings_disclosure(app: &mut SmsArchiveApp, ui: &mut egui::Ui) {
    let header = if app.analytics.settings_open { "▼" } else { "▶" };
    let resp = ui.button(format!(
        "{}  ⚙ Analytics Settings (weights, thresholds, tunables)",
        header
    ));
    if resp.clicked() {
        app.analytics.settings_open = !app.analytics.settings_open;
    }
    if !app.analytics.settings_open {
        return;
    }

    if !app.analytics.settings_loaded {
        match load_settings_from_db(&app.db_path) {
            Ok(s) => {
                app.analytics.settings = s;
                app.analytics.settings_loaded = true;
                app.analytics.settings_status = "Loaded.".into();
            }
            Err(e) => {
                app.analytics.settings_status = format!("Load failed: {}", e);
            }
        }
    }

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("Settings live in the analytics_meta table. Changes apply on next Run Analysis. Save & Mark All Stale to force every contact to recompute.").italics());
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            if ui.button("💾 Save").clicked() {
                match save_settings_to_db(&app.db_path, &app.analytics.settings) {
                    Ok(()) => app.analytics.settings_status = "Saved.".into(),
                    Err(e) => app.analytics.settings_status = format!("Save failed: {}", e),
                }
            }
            if ui.button("💾 Save & Mark All Stale").clicked() {
                let save = save_settings_to_db(&app.db_path, &app.analytics.settings);
                let stale = mark_all_caches_stale(&app.db_path);
                app.analytics.settings_status = match (save, stale) {
                    (Ok(()), Ok(())) => "Saved + flagged all caches stale.".into(),
                    (Err(e), _) | (_, Err(e)) => format!("Failed: {}", e),
                };
            }
            if ui.button("↺ Reset to Defaults").clicked() {
                app.analytics.settings = AnalyticsSettings::default();
                app.analytics.settings_status = "Reset (not yet saved — click Save).".into();
            }
            if ui.button("⟳ Reload from DB").clicked() {
                match load_settings_from_db(&app.db_path) {
                    Ok(s) => {
                        app.analytics.settings = s;
                        app.analytics.settings_status = "Reloaded.".into();
                    }
                    Err(e) => app.analytics.settings_status = format!("Reload failed: {}", e),
                }
            }
            if !app.analytics.settings_status.is_empty() {
                ui.weak(app.analytics.settings_status.clone());
            }
        });

        ui.add_space(6.0);
        let s = &mut app.analytics.settings;

        egui::CollapsingHeader::new("Segmentation")
            .default_open(true)
            .show(ui, |ui| {
                drag_i64(ui, "Conversation timeout (sec)", &mut s.conversation_timeout_secs, 60, 86_400);
                drag_u32(ui, "Big moment threshold (static msgs)", &mut s.big_moment_threshold_static, 5, 200);
                drag_u8(ui, "Big moment dynamic percentile", &mut s.big_moment_threshold_dynamic_pct, 50, 99);
                drag_u32(ui, "Big moment dynamic floor", &mut s.big_moment_threshold_dynamic_floor, 1, 100);
                drag_i64(ui, "Reconnect tier 1 (sec)", &mut s.reconnect_tier1_secs, 3_600, 7 * 86_400);
                drag_i64(ui, "Reconnect tier 2 (sec)", &mut s.reconnect_tier2_secs, 86_400, 30 * 86_400);
                drag_i64(ui, "Reconnect tier 3 (sec)", &mut s.reconnect_tier3_secs, 7 * 86_400, 365 * 86_400);
                drag_f64(ui, "Reconnect tier 4 (× pair median)", &mut s.reconnect_tier4_multiplier, 1.5, 10.0);
            });

        egui::CollapsingHeader::new("Response classification")
            .show(ui, |ui| {
                drag_i64(ui, "Rapid response threshold (sec)", &mut s.rapid_response_threshold_secs, 5, 600);
                drag_u8(ui, "Overnight window — start hour (local)", &mut s.overnight_window_start_hour, 0, 23);
                drag_u8(ui, "Overnight window — end hour (local)", &mut s.overnight_window_end_hour, 0, 23);
            });

        egui::CollapsingHeader::new("Point weights (per-message scoring)")
            .show(ui, |ui| {
                drag_f64(ui, "Text message (base)", &mut s.weight_text_message, 0.0, 10.0);
                drag_f64(ui, "Per-word log scale", &mut s.weight_per_word_log, 0.0, 1.0);
                drag_f64(ui, "Emoji", &mut s.weight_emoji, 0.0, 5.0);
                drag_f64(ui, "Question", &mut s.weight_question, 0.0, 5.0);
                drag_f64(ui, "Apology", &mut s.weight_apology, 0.0, 5.0);
                drag_f64(ui, "Encouragement", &mut s.weight_encouragement, 0.0, 5.0);
                drag_f64(ui, "Link", &mut s.weight_link, 0.0, 5.0);
                drag_f64(ui, "Image", &mut s.weight_image, 0.0, 10.0);
                drag_f64(ui, "Video", &mut s.weight_video, 0.0, 10.0);
                drag_f64(ui, "Audio", &mut s.weight_audio, 0.0, 10.0);
                drag_f64(ui, "GIF", &mut s.weight_gif, 0.0, 10.0);
                drag_f64(ui, "Started conversation", &mut s.weight_started_convo, 0.0, 20.0);
                drag_f64(ui, "Rapid response", &mut s.weight_rapid_response, 0.0, 10.0);
            });

        egui::CollapsingHeader::new("Chat Rating components")
            .show(ui, |ui| {
                drag_f64(ui, "Responsiveness weight", &mut s.rating_weight_responsiveness, 0.0, 1.0);
                drag_f64(ui, "Balance weight", &mut s.rating_weight_balance, 0.0, 1.0);
                drag_f64(ui, "Engagement weight", &mut s.rating_weight_engagement, 0.0, 1.0);
                drag_f64(ui, "Consistency weight", &mut s.rating_weight_consistency, 0.0, 1.0);
                drag_f64(ui, "Reciprocity weight", &mut s.rating_weight_reciprocity, 0.0, 1.0);
                drag_f64(ui, "Longevity weight", &mut s.rating_weight_longevity, 0.0, 1.0);
                drag_f64(ui, "Mutual Effort weight", &mut s.rating_weight_mutual_effort, 0.0, 1.0);
                let total: f64 = s.rating_weight_responsiveness
                    + s.rating_weight_balance
                    + s.rating_weight_engagement
                    + s.rating_weight_consistency
                    + s.rating_weight_reciprocity
                    + s.rating_weight_longevity
                    + s.rating_weight_mutual_effort;
                ui.weak(format!(
                    "Sum: {:.3} (will be auto-normalized at compute time)",
                    total
                ));
            });

        egui::CollapsingHeader::new("Display thresholds")
            .show(ui, |ui| {
                drag_u32(ui, "Hide rating below N messages", &mut s.rating_hide_below_messages, 1, 1_000);
                drag_u32(ui, "Limited-data badge until N messages", &mut s.rating_low_confidence_max_messages, 1, 5_000);
                drag_u32(ui, "Contact picker min messages", &mut s.contact_picker_min_messages, 1, 1_000);
            });

        egui::CollapsingHeader::new("Insight tuning")
            .show(ui, |ui| {
                drag_f64(ui, "Tier 1 ratio (slightly more)", &mut s.insight_tier1_ratio, 1.0, 5.0);
                drag_f64(ui, "Tier 2 ratio (more)", &mut s.insight_tier2_ratio, 1.0, 5.0);
                drag_f64(ui, "Tier 3 ratio (much more)", &mut s.insight_tier3_ratio, 1.0, 5.0);
                drag_f64(ui, "Tier 4 ratio (far more)", &mut s.insight_tier4_ratio, 1.0, 10.0);
                drag_u32(ui, "Min sample per rule", &mut s.insight_min_sample_per_rule, 1, 5_000);
                drag_u32(ui, "Low-confidence max sample", &mut s.insight_low_confidence_max_sample, 1, 5_000);
                ui.weak("(Tier ratios feed the rule engine; full effect lands when the analytics crate reads them at runtime — currently they live as compile-time constants.)");
            });
    });
}

fn drag_f64(ui: &mut egui::Ui, label: &str, value: &mut f64, min: f64, max: f64) {
    ui.horizontal(|ui| {
        ui.add_sized([260.0, 18.0], egui::Label::new(label));
        ui.add(
            egui::DragValue::new(value)
                .clamp_range(min..=max)
                .speed((max - min) * 0.005),
        );
    });
}

fn drag_i64(ui: &mut egui::Ui, label: &str, value: &mut i64, min: i64, max: i64) {
    ui.horizontal(|ui| {
        ui.add_sized([260.0, 18.0], egui::Label::new(label));
        ui.add(
            egui::DragValue::new(value)
                .clamp_range(min..=max)
                .speed(((max - min) as f64) * 0.005),
        );
    });
}

fn drag_u32(ui: &mut egui::Ui, label: &str, value: &mut u32, min: u32, max: u32) {
    ui.horizontal(|ui| {
        ui.add_sized([260.0, 18.0], egui::Label::new(label));
        ui.add(
            egui::DragValue::new(value)
                .clamp_range(min..=max)
                .speed(((max - min) as f64) * 0.005),
        );
    });
}

fn drag_u8(ui: &mut egui::Ui, label: &str, value: &mut u8, min: u8, max: u8) {
    ui.horizontal(|ui| {
        ui.add_sized([260.0, 18.0], egui::Label::new(label));
        ui.add(egui::DragValue::new(value).clamp_range(min..=max).speed(0.5));
    });
}

// ===================================================================
// VISUAL WIDGETS (Phase 3.4b)
// ===================================================================

/// Colour ramp for ratings: red → orange → yellow → green.
fn rating_color(score: i64) -> egui::Color32 {
    match score {
        90..=100 => egui::Color32::from_rgb(80, 200, 120),
        75..=89 => egui::Color32::from_rgb(120, 200, 100),
        60..=74 => egui::Color32::from_rgb(200, 200, 80),
        45..=59 => egui::Color32::from_rgb(220, 160, 60),
        25..=44 => egui::Color32::from_rgb(220, 110, 60),
        _ => egui::Color32::from_rgb(220, 80, 80),
    }
}

/// Circular gauge: filled arc from 12 o'clock around to N% of the circle.
/// `size` is the bounding-box edge length in logical pixels.
fn paint_rating_gauge(ui: &mut egui::Ui, score: i64, size: f32) {
    let (response, painter) =
        ui.allocate_painter(egui::vec2(size, size), egui::Sense::hover());
    let rect = response.rect;
    let center = rect.center();
    let radius = size * 0.4;
    let stroke_w = size * 0.10;

    // Background ring.
    painter.circle_stroke(
        center,
        radius,
        egui::Stroke::new(stroke_w, egui::Color32::from_gray(55)),
    );

    // Score arc — clamp 0..100, map to 0..2π starting from straight up.
    let pct = (score.clamp(0, 100) as f32) / 100.0;
    let color = rating_color(score);
    let start = -std::f32::consts::FRAC_PI_2; // top of circle
    let end = start + 2.0 * std::f32::consts::PI * pct;
    if pct > 0.0 {
        let segments = ((pct * 96.0).max(8.0)) as usize;
        let mut points = Vec::with_capacity(segments + 1);
        for i in 0..=segments {
            let t = i as f32 / segments as f32;
            let angle = start + (end - start) * t;
            points.push(egui::pos2(
                center.x + radius * angle.cos(),
                center.y + radius * angle.sin(),
            ));
        }
        painter.add(egui::Shape::line(points, egui::Stroke::new(stroke_w, color)));
    }

    // Center label.
    painter.text(
        center,
        egui::Align2::CENTER_CENTER,
        format!("{}", score),
        egui::FontId::proportional(size * 0.32),
        egui::Color32::WHITE,
    );
    painter.text(
        egui::pos2(center.x, center.y + size * 0.18),
        egui::Align2::CENTER_CENTER,
        "/ 100",
        egui::FontId::proportional(size * 0.10),
        egui::Color32::from_gray(180),
    );
}

/// Horizontal stacked bar showing left vs right share. Labels are rendered
/// inside the segments if they have enough room.
fn paint_balance_bar(
    ui: &mut egui::Ui,
    left_count: i64,
    right_count: i64,
    left_label: &str,
    right_label: &str,
    height: f32,
) {
    let total = (left_count + right_count) as f32;
    let left_pct = if total > 0.0 {
        left_count as f32 / total
    } else {
        0.5
    };

    let width = ui.available_width().max(120.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter();

    let split = rect.left() + rect.width() * left_pct;
    let left_rect = egui::Rect::from_min_max(rect.min, egui::pos2(split, rect.max.y));
    let right_rect = egui::Rect::from_min_max(egui::pos2(split, rect.min.y), rect.max);

    painter.rect_filled(left_rect, 4.0, egui::Color32::from_rgb(80, 130, 200));
    painter.rect_filled(right_rect, 4.0, egui::Color32::from_rgb(80, 200, 160));

    let font = egui::FontId::proportional(height * 0.55);
    if left_rect.width() > 60.0 {
        painter.text(
            left_rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("{} {:.0}%", left_label, left_pct * 100.0),
            font.clone(),
            egui::Color32::WHITE,
        );
    }
    if right_rect.width() > 60.0 {
        painter.text(
            right_rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("{} {:.0}%", right_label, (1.0 - left_pct) * 100.0),
            font,
            egui::Color32::BLACK,
        );
    }
}

// ---------- Relationship Growth ----------

fn render_relationship_growth_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Relationship Growth");
        ui.weak("Cumulative chat points over time (you / them / total)");
        ui.add_space(6.0);
        if l.daily.is_empty() {
            ui.label("(no daily data yet)");
            return;
        }
        paint_relationship_growth_chart(ui, &l.daily, 720.0, 200.0);
    });
}

/// Cumulative line chart. Three series: my, their, total points by day.
fn paint_relationship_growth_chart(
    ui: &mut egui::Ui,
    daily: &[DailyActivityPoint],
    width: f32,
    height: f32,
) {
    let avail = ui.available_width().min(width).max(360.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(avail, height), egui::Sense::hover());
    let painter = ui.painter();
    let plot = rect.shrink2(egui::vec2(40.0, 16.0));

    // Build cumulative series.
    let mut my_cum = 0.0f32;
    let mut their_cum = 0.0f32;
    let mut my_pts: Vec<(f32, f32)> = Vec::with_capacity(daily.len());
    let mut their_pts: Vec<(f32, f32)> = Vec::with_capacity(daily.len());
    let mut total_pts: Vec<(f32, f32)> = Vec::with_capacity(daily.len());
    for (i, d) in daily.iter().enumerate() {
        my_cum += d.my_points as f32;
        their_cum += d.their_points as f32;
        let x = i as f32;
        my_pts.push((x, my_cum));
        their_pts.push((x, their_cum));
        total_pts.push((x, my_cum + their_cum));
    }
    let max_x = (daily.len().saturating_sub(1)) as f32;
    let max_y = total_pts.last().map(|(_, y)| *y).unwrap_or(1.0).max(1.0);

    // Axes background + frame.
    painter.rect_filled(rect, 4.0, egui::Color32::from_gray(28));
    painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, egui::Color32::from_gray(60)));

    // Y-axis labels (top, mid, bottom).
    let label_color = egui::Color32::from_gray(170);
    for (frac, label) in [(0.0, format!("{:.0}", max_y)), (0.5, format!("{:.0}", max_y / 2.0)), (1.0, "0".to_string())] {
        let y = plot.top() + plot.height() * frac;
        painter.text(
            egui::pos2(rect.left() + 4.0, y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(11.0),
            label_color,
        );
        painter.line_segment(
            [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
            egui::Stroke::new(0.5, egui::Color32::from_gray(50)),
        );
    }

    // First/last day markers on x-axis.
    if let (Some(first), Some(last)) = (daily.first(), daily.last()) {
        painter.text(
            egui::pos2(plot.left(), rect.bottom() - 2.0),
            egui::Align2::LEFT_BOTTOM,
            &first.day,
            egui::FontId::proportional(11.0),
            label_color,
        );
        painter.text(
            egui::pos2(plot.right(), rect.bottom() - 2.0),
            egui::Align2::RIGHT_BOTTOM,
            &last.day,
            egui::FontId::proportional(11.0),
            label_color,
        );
    }

    // Plotting helper: convert (x, y) data → pixel position.
    let to_px = |x: f32, y: f32| -> egui::Pos2 {
        let nx = if max_x > 0.0 { x / max_x } else { 0.5 };
        let ny = if max_y > 0.0 { 1.0 - y / max_y } else { 1.0 };
        egui::pos2(
            plot.left() + nx * plot.width(),
            plot.top() + ny * plot.height(),
        )
    };

    let draw_series = |pts: &[(f32, f32)], color: egui::Color32| {
        let line: Vec<egui::Pos2> = pts.iter().map(|(x, y)| to_px(*x, *y)).collect();
        painter.add(egui::Shape::line(line, egui::Stroke::new(1.5, color)));
    };
    draw_series(&total_pts, egui::Color32::from_rgb(180, 180, 220));
    draw_series(&my_pts, egui::Color32::from_rgb(80, 130, 200));
    draw_series(&their_pts, egui::Color32::from_rgb(80, 200, 160));

    // Legend in top-right corner.
    let legend_box = egui::Rect::from_min_size(
        egui::pos2(rect.right() - 110.0, rect.top() + 4.0),
        egui::vec2(106.0, 56.0),
    );
    painter.rect_filled(legend_box, 4.0, egui::Color32::from_rgba_unmultiplied(28, 28, 28, 200));
    let mut y = legend_box.top() + 6.0;
    for (color, label) in [
        (egui::Color32::from_rgb(180, 180, 220), "Total"),
        (egui::Color32::from_rgb(80, 130, 200), "You"),
        (egui::Color32::from_rgb(80, 200, 160), "Them"),
    ] {
        painter.line_segment(
            [
                egui::pos2(legend_box.left() + 6.0, y + 6.0),
                egui::pos2(legend_box.left() + 24.0, y + 6.0),
            ],
            egui::Stroke::new(2.0, color),
        );
        painter.text(
            egui::pos2(legend_box.left() + 28.0, y + 2.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(11.0),
            label_color,
        );
        y += 14.0;
    }
}

// ---------- Sentiment Timeline ----------

fn render_sentiment_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Sentiment Timeline");
        ui.weak("Daily average AFINN-style score per side. >0 = positive tone, <0 = negative.");
        ui.add_space(6.0);
        let Some(timeline) = l.sentiment_timeline.as_ref() else {
            ui.weak("(no sentiment data — refresh analytics)");
            return;
        };
        if timeline.days.is_empty() {
            ui.weak("(no message data on any day)");
            return;
        }

        ui.horizontal(|ui| {
            if let Some(my) = timeline.overall_my {
                ui.colored_label(
                    sentiment_text_color(my),
                    format!("Your overall tone: {:+.2}", my),
                );
            }
            ui.add_space(20.0);
            if let Some(t) = timeline.overall_their {
                ui.colored_label(
                    sentiment_text_color(t),
                    format!("Their overall tone: {:+.2}", t),
                );
            }
        });
        ui.add_space(6.0);

        paint_sentiment_chart(ui, &timeline.days, 720.0, 180.0);
    });
}

fn paint_sentiment_chart(
    ui: &mut egui::Ui,
    days: &[SentimentDayLoaded],
    width: f32,
    height: f32,
) {
    let avail = ui.available_width().min(width).max(360.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(avail, height), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, egui::Color32::from_gray(28));
    painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, egui::Color32::from_gray(60)));

    let plot = rect.shrink2(egui::vec2(40.0, 16.0));

    // Find y-range so the zero line stays visible.
    let mut min_y: f32 = -0.5;
    let mut max_y: f32 = 0.5;
    for d in days {
        for opt in [d.my_score, d.their_score] {
            if let Some(v) = opt {
                let v = v as f32;
                if v < min_y { min_y = v; }
                if v > max_y { max_y = v; }
            }
        }
    }
    // Symmetric padding around zero.
    let span = (max_y - min_y).max(1.0);
    let to_px = |i: usize, y: f32| -> egui::Pos2 {
        let nx = if days.len() > 1 { i as f32 / (days.len() - 1) as f32 } else { 0.5 };
        let ny = 1.0 - (y - min_y) / span;
        egui::pos2(plot.left() + nx * plot.width(), plot.top() + ny * plot.height())
    };

    // Zero line.
    let zero_y = plot.top() + (1.0 - (0.0 - min_y) / span) * plot.height();
    painter.line_segment(
        [egui::pos2(plot.left(), zero_y), egui::pos2(plot.right(), zero_y)],
        egui::Stroke::new(1.0, egui::Color32::from_gray(80)),
    );
    painter.text(
        egui::pos2(rect.left() + 4.0, zero_y),
        egui::Align2::LEFT_CENTER,
        "0",
        egui::FontId::proportional(10.0),
        egui::Color32::from_gray(170),
    );
    painter.text(
        egui::pos2(rect.left() + 4.0, plot.top()),
        egui::Align2::LEFT_TOP,
        format!("{:+.1}", max_y),
        egui::FontId::proportional(10.0),
        egui::Color32::from_gray(170),
    );
    painter.text(
        egui::pos2(rect.left() + 4.0, plot.bottom()),
        egui::Align2::LEFT_BOTTOM,
        format!("{:+.1}", min_y),
        egui::FontId::proportional(10.0),
        egui::Color32::from_gray(170),
    );

    // First/last day labels on x-axis.
    if let (Some(first), Some(last)) = (days.first(), days.last()) {
        painter.text(
            egui::pos2(plot.left(), rect.bottom() - 2.0),
            egui::Align2::LEFT_BOTTOM,
            &first.day,
            egui::FontId::proportional(11.0),
            egui::Color32::from_gray(170),
        );
        painter.text(
            egui::pos2(plot.right(), rect.bottom() - 2.0),
            egui::Align2::RIGHT_BOTTOM,
            &last.day,
            egui::FontId::proportional(11.0),
            egui::Color32::from_gray(170),
        );
    }

    // Plot two series — connect only adjacent points that BOTH have a score.
    let mut my_pts: Vec<egui::Pos2> = Vec::new();
    let mut their_pts: Vec<egui::Pos2> = Vec::new();
    for (i, d) in days.iter().enumerate() {
        if let Some(s) = d.my_score {
            my_pts.push(to_px(i, s as f32));
        }
        if let Some(s) = d.their_score {
            their_pts.push(to_px(i, s as f32));
        }
    }
    if my_pts.len() >= 2 {
        painter.add(egui::Shape::line(
            my_pts,
            egui::Stroke::new(1.5, egui::Color32::from_rgb(80, 130, 200)),
        ));
    }
    if their_pts.len() >= 2 {
        painter.add(egui::Shape::line(
            their_pts,
            egui::Stroke::new(1.5, egui::Color32::from_rgb(80, 200, 160)),
        ));
    }

    // Legend.
    let legend = egui::Rect::from_min_size(
        egui::pos2(rect.right() - 100.0, rect.top() + 4.0),
        egui::vec2(96.0, 36.0),
    );
    painter.rect_filled(legend, 4.0, egui::Color32::from_rgba_unmultiplied(28, 28, 28, 200));
    painter.line_segment(
        [
            egui::pos2(legend.left() + 6.0, legend.top() + 10.0),
            egui::pos2(legend.left() + 22.0, legend.top() + 10.0),
        ],
        egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 130, 200)),
    );
    painter.text(
        egui::pos2(legend.left() + 26.0, legend.top() + 6.0),
        egui::Align2::LEFT_TOP,
        "You",
        egui::FontId::proportional(11.0),
        egui::Color32::from_gray(200),
    );
    painter.line_segment(
        [
            egui::pos2(legend.left() + 6.0, legend.top() + 26.0),
            egui::pos2(legend.left() + 22.0, legend.top() + 26.0),
        ],
        egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 200, 160)),
    );
    painter.text(
        egui::pos2(legend.left() + 26.0, legend.top() + 22.0),
        egui::Align2::LEFT_TOP,
        "Them",
        egui::FontId::proportional(11.0),
        egui::Color32::from_gray(200),
    );
}

fn sentiment_text_color(score: f64) -> egui::Color32 {
    if score >= 1.0 {
        egui::Color32::from_rgb(80, 200, 120)
    } else if score >= 0.2 {
        egui::Color32::from_rgb(140, 200, 90)
    } else if score >= -0.2 {
        egui::Color32::from_gray(190)
    } else if score >= -1.0 {
        egui::Color32::from_rgb(220, 160, 60)
    } else {
        egui::Color32::from_rgb(220, 80, 80)
    }
}

// ---------- Topics (TF-IDF distinctive phrases) ----------

fn render_topics_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("What you talk about");
        ui.weak(
            "TF-IDF distinctive phrases — bigrams + trigrams this pair uses much more \
             than your other contacts do. Sampled against up to 50k other-contact messages.",
        );
        ui.add_space(6.0);
        if l.topics.is_empty() {
            ui.weak("(no topics detected — refresh analytics)");
            return;
        }
        let max_score = l
            .topics
            .iter()
            .map(|t| t.score)
            .fold(0.0_f64, f64::max)
            .max(1e-9);
        egui::Grid::new("topics_grid")
            .num_columns(4)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("#").strong());
                ui.label(egui::RichText::new("Phrase").strong());
                ui.label(egui::RichText::new("Score").strong());
                ui.label(egui::RichText::new("Pair count").strong());
                ui.end_row();
                for (i, t) in l.topics.iter().enumerate() {
                    ui.label(format!("{}", i + 1));
                    ui.label(&t.phrase);
                    let bar_len = ((t.score / max_score) as f32 * 120.0).max(2.0);
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(140.0, 14.0),
                        egui::Sense::hover(),
                    );
                    let bar_rect = egui::Rect::from_min_size(
                        rect.min,
                        egui::vec2(bar_len, rect.height()),
                    );
                    ui.painter().rect_filled(
                        bar_rect,
                        2.0,
                        egui::Color32::from_rgb(180, 140, 200),
                    );
                    ui.painter().text(
                        egui::pos2(rect.left() + bar_len + 4.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        format!("{:.1}", t.score),
                        egui::FontId::proportional(11.0),
                        egui::Color32::from_gray(220),
                    );
                    ui.label(format!("{}", t.pair_count));
                    ui.end_row();
                }
            });
    });
}

// ---------- Inside Jokes ----------

fn render_inside_jokes_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Inside Jokes / Recurring Phrases");
        ui.weak("2- and 3-word phrases this pair uses repeatedly (≥3 times, stopwords filtered).");
        ui.add_space(6.0);
        if l.inside_jokes.is_empty() {
            ui.weak("(no recurring phrases detected — refresh analytics)");
            return;
        }
        let max = l
            .inside_jokes
            .iter()
            .map(|j| j.count)
            .max()
            .unwrap_or(1)
            .max(1) as f32;
        egui::Grid::new("inside_jokes_grid")
            .num_columns(3)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("#").strong());
                ui.label(egui::RichText::new("Phrase").strong());
                ui.label(egui::RichText::new("Count").strong());
                ui.end_row();
                for (i, j) in l.inside_jokes.iter().enumerate() {
                    ui.label(format!("{}", i + 1));
                    ui.label(&j.phrase);
                    // Bar visualization.
                    let bar_len = (j.count as f32 / max) * 120.0;
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(140.0, 14.0),
                        egui::Sense::hover(),
                    );
                    let bar_rect = egui::Rect::from_min_size(
                        rect.min,
                        egui::vec2(bar_len.max(2.0), rect.height()),
                    );
                    ui.painter().rect_filled(
                        bar_rect,
                        2.0,
                        egui::Color32::from_rgb(120, 160, 200),
                    );
                    ui.painter().text(
                        egui::pos2(rect.left() + bar_len + 4.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        format!("{}", j.count),
                        egui::FontId::proportional(11.0),
                        egui::Color32::from_gray(220),
                    );
                    ui.end_row();
                }
            });
    });
}

// ---------- Streak tracker ----------

#[derive(Debug, Clone, Default)]
struct Streaks {
    longest_active_streak: i64,
    longest_silent_streak: i64,
    current_active_streak: i64,
    current_silent_streak: i64,
}

/// Compute longest/current active+silent streaks.
///
/// CRITICAL: `activity_daily` only stores rows for days that HAD messages
/// — silent days are NOT in the table. So we walk the full date range
/// from first → last entry day-by-day, treating any date not in our
/// active set as silent. This matches the "GitHub-style streak" intuition.
///
/// Current streaks are measured from "today" so a gap between the last
/// data day and today properly counts as ongoing silence.
fn compute_streaks(daily: &[DailyActivityPoint]) -> Streaks {
    use chrono::NaiveDate;
    let mut s = Streaks::default();
    if daily.is_empty() {
        return s;
    }

    // Build the set of dates that had any activity.
    let active_days: std::collections::HashSet<NaiveDate> = daily
        .iter()
        .filter_map(|d| {
            if d.my_msgs + d.their_msgs > 0 {
                NaiveDate::parse_from_str(&d.day, "%Y-%m-%d").ok()
            } else {
                None
            }
        })
        .collect();
    if active_days.is_empty() {
        return s;
    }

    let first = *active_days.iter().min().unwrap();
    let last = *active_days.iter().max().unwrap();
    let today = chrono::Local::now().date_naive();
    // Walk through to *today* (or `last`, whichever is later) so a tail of
    // recent silence inflates `longest_silent_streak` correctly.
    let walk_end = today.max(last);

    let mut active_run = 0i64;
    let mut silent_run = 0i64;
    let mut day = first;
    while day <= walk_end {
        if active_days.contains(&day) {
            active_run += 1;
            silent_run = 0;
        } else {
            silent_run += 1;
            active_run = 0;
        }
        if active_run > s.longest_active_streak {
            s.longest_active_streak = active_run;
        }
        if silent_run > s.longest_silent_streak {
            s.longest_silent_streak = silent_run;
        }
        day = match day.succ_opt() {
            Some(d) => d,
            None => break,
        };
    }

    // After the loop, `active_run` and `silent_run` are the streaks ending
    // on `walk_end`. If today >= last, that's exactly "current".
    s.current_active_streak = active_run;
    s.current_silent_streak = silent_run;
    s
}

fn render_streak_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Streaks");
        ui.weak("Day-by-day continuity (counts days, not messages)");
        ui.add_space(6.0);
        if l.daily.is_empty() {
            ui.weak("(no daily data yet)");
            return;
        }
        let s = compute_streaks(&l.daily);
        egui::Grid::new("streak_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .show(ui, |ui| {
                ui.label("Current active streak");
                ui.label(format!("{} days", s.current_active_streak));
                ui.end_row();
                ui.label("Longest active streak");
                ui.label(format!("{} days", s.longest_active_streak));
                ui.end_row();
                ui.label("Current silence");
                ui.label(format!("{} days", s.current_silent_streak));
                ui.end_row();
                ui.label("Longest silence");
                ui.label(format!("{} days", s.longest_silent_streak));
                ui.end_row();
            });
    });
}

// ---------- Direction of Conversation donut ----------

fn render_direction_donut_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Direction of Conversation");
        ui.weak("Where the talk focuses (you / them / others)");
        ui.add_space(6.0);

        let (me, them, other) = match (l.focus_me_pct, l.focus_them_pct, l.focus_other_pct) {
            (Some(m), Some(t), Some(o)) => (m, t, o),
            _ => {
                ui.label(
                    "Focus stats not yet computed. Click \"Refresh Analytics\" \
                     to populate (orchestrator was updated to compute these).",
                );
                return;
            }
        };

        ui.horizontal(|ui| {
            paint_direction_donut(ui, me, them, other, 160.0);
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(80, 130, 200),
                    format!("● You: {:.1}%", me * 100.0),
                );
                ui.colored_label(
                    egui::Color32::from_rgb(80, 200, 160),
                    format!("● Them: {:.1}%", them * 100.0),
                );
                ui.colored_label(
                    egui::Color32::from_rgb(180, 180, 180),
                    format!("● Others: {:.1}%", other * 100.0),
                );
                ui.add_space(8.0);
                ui.weak(
                    "Computed by scanning message bodies for first-person and \
                     second-person pronouns + mentions of other contact names.",
                );
            });
        });
    });
}

fn paint_direction_donut(
    ui: &mut egui::Ui,
    me_pct: f64,
    them_pct: f64,
    other_pct: f64,
    size: f32,
) {
    let total = (me_pct + them_pct + other_pct).max(1e-9);
    let me_frac = (me_pct / total).clamp(0.0, 1.0) as f32;
    let them_frac = (them_pct / total).clamp(0.0, 1.0) as f32;
    let other_frac = (other_pct / total).clamp(0.0, 1.0) as f32;

    let (response, painter) =
        ui.allocate_painter(egui::vec2(size, size), egui::Sense::hover());
    let rect = response.rect;
    let center = rect.center();
    let radius = size * 0.42;
    let stroke_w = size * 0.18;

    let mut start = -std::f32::consts::FRAC_PI_2;
    for (frac, color) in [
        (me_frac, egui::Color32::from_rgb(80, 130, 200)),
        (them_frac, egui::Color32::from_rgb(80, 200, 160)),
        (other_frac, egui::Color32::from_rgb(180, 180, 180)),
    ] {
        if frac <= 0.0 {
            continue;
        }
        let end = start + 2.0 * std::f32::consts::PI * frac;
        let segments = ((frac * 96.0).max(8.0)) as usize;
        let mut points = Vec::with_capacity(segments + 1);
        for i in 0..=segments {
            let t = i as f32 / segments as f32;
            let angle = start + (end - start) * t;
            points.push(egui::pos2(
                center.x + radius * angle.cos(),
                center.y + radius * angle.sin(),
            ));
        }
        painter.add(egui::Shape::line(points, egui::Stroke::new(stroke_w, color)));
        start = end;
    }
}

// ---------- Sankey conversation flow ----------

#[derive(Debug, Clone, serde::Deserialize)]
struct SankeyData {
    nodes: Vec<SankeyNode>,
    links: Vec<SankeyLink>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SankeyNode {
    id: String,
    label: String,
    value: u32,
    column: u8,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SankeyLink {
    source: String,
    target: String,
    value: u32,
}

fn render_sankey_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Conversation Flow");
        ui.weak("Started by → Big moment / Everyday / No reply → Major contributor → Final reply");
        ui.add_space(6.0);
        let raw = match l.conversation_flow_json.as_deref() {
            Some(s) if !s.trim().is_empty() && s.trim() != "{}" => s,
            _ => {
                ui.label("(no conversation flow data yet)");
                return;
            }
        };
        let data: SankeyData = match serde_json::from_str(raw) {
            Ok(d) => d,
            Err(e) => {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), format!("Sankey JSON error: {}", e));
                return;
            }
        };
        if data.nodes.is_empty() {
            ui.label("(empty Sankey)");
            return;
        }
        paint_sankey(ui, &data, 720.0, 320.0);
    });
}

fn paint_sankey(ui: &mut egui::Ui, data: &SankeyData, width: f32, height: f32) {
    let avail = ui.available_width().min(width).max(420.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(avail, height), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, egui::Color32::from_gray(28));
    painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, egui::Color32::from_gray(60)));

    // Group nodes by column.
    let max_col = data.nodes.iter().map(|n| n.column).max().unwrap_or(0) as usize + 1;
    let mut by_col: Vec<Vec<&SankeyNode>> = vec![Vec::new(); max_col];
    for node in &data.nodes {
        by_col[node.column as usize].push(node);
    }

    // Lay out: each column is a vertical strip. Within a column, stack nodes
    // by id (deterministic). Height ∝ value, scaled to fit.
    struct NodeBox {
        id: String,
        rect: egui::Rect,
        value: u32,
        column: u8,
        label: String,
    }
    let mut boxes: std::collections::HashMap<String, NodeBox> = std::collections::HashMap::new();

    let plot = rect.shrink2(egui::vec2(20.0, 20.0));
    let col_count = max_col.max(1);
    let col_step = if col_count > 1 {
        plot.width() / (col_count as f32 - 1.0)
    } else {
        plot.width()
    };
    let node_width = (col_step * 0.18).clamp(28.0, 70.0);

    for (ci, col) in by_col.iter().enumerate() {
        let total_value: u32 = col.iter().map(|n| n.value).sum();
        if total_value == 0 {
            continue;
        }
        let gap = 8.0;
        let usable_h = plot.height() - gap * (col.len().saturating_sub(1) as f32);
        let mut y = plot.top();
        let x_center = plot.left() + (ci as f32) * col_step;
        let x_left = x_center - node_width / 2.0;

        // Sort within column by id for deterministic stacking (matches the
        // analytics crate's flow.rs ordering).
        let mut sorted: Vec<&SankeyNode> = col.iter().copied().collect();
        sorted.sort_by(|a, b| a.id.cmp(&b.id));

        for node in sorted {
            let h = usable_h * (node.value as f32 / total_value as f32);
            let nb_rect = egui::Rect::from_min_size(
                egui::pos2(x_left, y),
                egui::vec2(node_width, h.max(2.0)),
            );
            boxes.insert(
                node.id.clone(),
                NodeBox {
                    id: node.id.clone(),
                    rect: nb_rect,
                    value: node.value,
                    column: node.column,
                    label: node.label.clone(),
                },
            );
            y += h + gap;
        }
    }

    // Draw links (under nodes so node boxes appear on top).
    // Track per-node consumed offsets for source-side and target-side stacking.
    let mut src_offsets: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
    let mut tgt_offsets: std::collections::HashMap<String, f32> = std::collections::HashMap::new();

    for link in &data.links {
        let src = match boxes.get(&link.source) {
            Some(b) => b,
            None => continue,
        };
        let tgt = match boxes.get(&link.target) {
            Some(b) => b,
            None => continue,
        };
        // Compute the strip height proportional to link value, sized against the source's full value.
        let src_h = src.rect.height() * (link.value as f32 / src.value.max(1) as f32);
        let tgt_h = tgt.rect.height() * (link.value as f32 / tgt.value.max(1) as f32);

        let src_top = *src_offsets.entry(link.source.clone()).or_insert(0.0);
        let tgt_top = *tgt_offsets.entry(link.target.clone()).or_insert(0.0);

        let src_y0 = src.rect.top() + src_top;
        let src_y1 = src_y0 + src_h;
        let tgt_y0 = tgt.rect.top() + tgt_top;
        let tgt_y1 = tgt_y0 + tgt_h;

        let src_x = src.rect.right();
        let tgt_x = tgt.rect.left();

        // Cubic bezier with control points at the midpoint between source and target.
        let cx_a = (src_x + tgt_x) / 2.0;
        let segs = 32;

        // Top edge (src_y0 → tgt_y0)
        let mut top_edge = Vec::with_capacity(segs + 1);
        for i in 0..=segs {
            let t = i as f32 / segs as f32;
            let pt = bezier_point(
                egui::pos2(src_x, src_y0),
                egui::pos2(cx_a, src_y0),
                egui::pos2(cx_a, tgt_y0),
                egui::pos2(tgt_x, tgt_y0),
                t,
            );
            top_edge.push(pt);
        }
        // Bottom edge in reverse (tgt_y1 → src_y1)
        let mut bot_edge = Vec::with_capacity(segs + 1);
        for i in 0..=segs {
            let t = i as f32 / segs as f32;
            let pt = bezier_point(
                egui::pos2(tgt_x, tgt_y1),
                egui::pos2(cx_a, tgt_y1),
                egui::pos2(cx_a, src_y1),
                egui::pos2(src_x, src_y1),
                t,
            );
            bot_edge.push(pt);
        }

        let mut polygon: Vec<egui::Pos2> = top_edge;
        polygon.extend(bot_edge);

        let color = sankey_link_color(&link.source);
        painter.add(egui::Shape::convex_polygon(
            polygon,
            color,
            egui::Stroke::NONE,
        ));

        *src_offsets.entry(link.source.clone()).or_insert(0.0) += src_h;
        *tgt_offsets.entry(link.target.clone()).or_insert(0.0) += tgt_h;
    }

    // Draw node rectangles + labels on top.
    for (_, b) in boxes.iter() {
        painter.rect_filled(b.rect, 2.0, sankey_node_color(b.column));
        // Label sits to the side appropriate for its column.
        let label_anchor = if b.column == 0 {
            egui::Align2::RIGHT_CENTER
        } else if b.column == max_col as u8 - 1 {
            egui::Align2::LEFT_CENTER
        } else {
            egui::Align2::CENTER_CENTER
        };
        let label_pos = match label_anchor {
            egui::Align2::RIGHT_CENTER => {
                egui::pos2(b.rect.left() - 4.0, b.rect.center().y)
            }
            egui::Align2::LEFT_CENTER => {
                egui::pos2(b.rect.right() + 4.0, b.rect.center().y)
            }
            _ => egui::pos2(b.rect.center().x, b.rect.center().y),
        };
        painter.text(
            label_pos,
            label_anchor,
            format!("{}\n{}", b.label, b.value),
            egui::FontId::proportional(11.0),
            egui::Color32::from_gray(220),
        );
    }
}

fn bezier_point(
    p0: egui::Pos2,
    p1: egui::Pos2,
    p2: egui::Pos2,
    p3: egui::Pos2,
    t: f32,
) -> egui::Pos2 {
    let u = 1.0 - t;
    let x = u.powi(3) * p0.x + 3.0 * u.powi(2) * t * p1.x + 3.0 * u * t.powi(2) * p2.x + t.powi(3) * p3.x;
    let y = u.powi(3) * p0.y + 3.0 * u.powi(2) * t * p1.y + 3.0 * u * t.powi(2) * p2.y + t.powi(3) * p3.y;
    egui::pos2(x, y)
}

fn sankey_link_color(source_id: &str) -> egui::Color32 {
    if source_id.starts_with("started_me") {
        egui::Color32::from_rgba_unmultiplied(80, 130, 200, 110)
    } else if source_id.starts_with("started_them") {
        egui::Color32::from_rgba_unmultiplied(80, 200, 160, 110)
    } else if source_id.starts_with("big_moment") {
        egui::Color32::from_rgba_unmultiplied(220, 160, 60, 110)
    } else if source_id.starts_with("everyday") {
        egui::Color32::from_rgba_unmultiplied(120, 160, 200, 100)
    } else if source_id.starts_with("no_reply") {
        egui::Color32::from_rgba_unmultiplied(220, 80, 80, 90)
    } else if source_id.starts_with("contrib_me") {
        egui::Color32::from_rgba_unmultiplied(80, 130, 200, 130)
    } else if source_id.starts_with("contrib_them") {
        egui::Color32::from_rgba_unmultiplied(80, 200, 160, 130)
    } else {
        egui::Color32::from_rgba_unmultiplied(150, 150, 150, 90)
    }
}

fn sankey_node_color(column: u8) -> egui::Color32 {
    match column {
        0 => egui::Color32::from_rgb(70, 110, 170),
        1 => egui::Color32::from_rgb(170, 130, 70),
        2 => egui::Color32::from_rgb(110, 160, 130),
        3 => egui::Color32::from_rgb(170, 110, 140),
        _ => egui::Color32::from_gray(120),
    }
}

// ---------- Daily Activity Heatmap (last 500 days) ----------

fn render_daily_activity_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Daily Chat Activity (last 500 days)");
        ui.add_space(6.0);
        if l.daily.is_empty() {
            ui.label("(no daily data yet)");
            return;
        }
        paint_daily_heatmap(ui, &l.daily);
    });
}

fn paint_daily_heatmap(ui: &mut egui::Ui, daily: &[DailyActivityPoint]) {
    use chrono::{Datelike, NaiveDate};

    let cell = 11.0;
    let gap = 2.0;

    let n = 500.min(daily.len());
    let recent: &[DailyActivityPoint] = &daily[daily.len().saturating_sub(n)..];

    if recent.is_empty() {
        ui.label("(no recent days)");
        return;
    }

    let max_msgs = recent
        .iter()
        .map(|d| d.my_msgs + d.their_msgs)
        .max()
        .unwrap_or(1)
        .max(1) as f32;

    // Parse dates and lay out by ISO week starting on Sunday.
    // Layout: columns = weeks ascending, rows = day-of-week (0=Sunday).
    struct Cell {
        col: usize,
        row: usize,
        intensity: f32,
    }
    let mut cells: Vec<Cell> = Vec::with_capacity(recent.len());
    let mut min_col = usize::MAX;
    let mut max_col = 0usize;

    let first_date = NaiveDate::parse_from_str(&recent[0].day, "%Y-%m-%d").unwrap_or_default();
    // anchor_sunday = the most recent Sunday on/before first_date
    let anchor_sunday = first_date
        - chrono::Duration::days(first_date.weekday().num_days_from_sunday() as i64);

    for d in recent {
        let Ok(date) = NaiveDate::parse_from_str(&d.day, "%Y-%m-%d") else {
            continue;
        };
        let days_since_anchor = (date - anchor_sunday).num_days();
        if days_since_anchor < 0 {
            continue;
        }
        let col = (days_since_anchor / 7) as usize;
        let row = date.weekday().num_days_from_sunday() as usize;
        let count = (d.my_msgs + d.their_msgs) as f32;
        // Log-scale intensity to compress hot spikes.
        let intensity = if count <= 0.0 {
            0.0
        } else {
            ((count + 1.0).ln() / (max_msgs + 1.0).ln()).clamp(0.0, 1.0)
        };
        if col < min_col {
            min_col = col;
        }
        if col > max_col {
            max_col = col;
        }
        cells.push(Cell { col, row, intensity });
    }

    if cells.is_empty() {
        ui.label("(no parseable dates)");
        return;
    }

    let cols = max_col - min_col + 1;
    let total_w = (cols as f32) * (cell + gap);
    let total_h = 7.0 * (cell + gap);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total_w, total_h), egui::Sense::hover());
    let painter = ui.painter();

    // Empty background grid (so missing days show as dim cells).
    for row in 0..7 {
        for col in 0..cols {
            let x = rect.left() + (col as f32) * (cell + gap);
            let y = rect.top() + (row as f32) * (cell + gap);
            painter.rect_filled(
                egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell, cell)),
                2.0,
                egui::Color32::from_gray(38),
            );
        }
    }

    // Active cells.
    for c in &cells {
        let col = c.col - min_col;
        let x = rect.left() + (col as f32) * (cell + gap);
        let y = rect.top() + (c.row as f32) * (cell + gap);
        let color = activity_color(c.intensity);
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell, cell)),
            2.0,
            color,
        );
    }
}

/// 0.0 → dim gray, 1.0 → bright green.
fn activity_color(intensity: f32) -> egui::Color32 {
    let t = intensity.clamp(0.0, 1.0);
    let r = lerp_u8(45, 80, t);
    let g = lerp_u8(60, 200, t);
    let b = lerp_u8(45, 120, t);
    egui::Color32::from_rgb(r, g, b)
}

/// IronBow thermal palette. Multi-stop interpolation:
/// black → dark purple → magenta → red → orange → yellow → white.
/// Standard "heat map" colorway used in thermal imaging — communicates
/// intensity intuitively because we see it everywhere (Predator vision,
/// FLIR cameras, etc.).
fn ironbow_color(intensity: f32) -> egui::Color32 {
    const STOPS: &[(f32, [u8; 3])] = &[
        (0.00, [0, 0, 0]),
        (0.15, [40, 0, 80]),
        (0.30, [130, 0, 130]),
        (0.50, [220, 40, 40]),
        (0.70, [250, 150, 40]),
        (0.85, [255, 230, 100]),
        (1.00, [255, 255, 255]),
    ];
    let t = intensity.clamp(0.0, 1.0);
    for window in STOPS.windows(2) {
        let (a_t, a_rgb) = window[0];
        let (b_t, b_rgb) = window[1];
        if t >= a_t && t <= b_t {
            let local = if b_t > a_t { (t - a_t) / (b_t - a_t) } else { 0.0 };
            return egui::Color32::from_rgb(
                lerp_u8(a_rgb[0], b_rgb[0], local),
                lerp_u8(a_rgb[1], b_rgb[1], local),
                lerp_u8(a_rgb[2], b_rgb[2], local),
            );
        }
    }
    egui::Color32::WHITE
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let af = a as f32;
    let bf = b as f32;
    (af + (bf - af) * t).round().clamp(0.0, 255.0) as u8
}

// ---------- Messaging Times Heatmap (DOW × hour) ----------

fn render_messaging_times_panel(ui: &mut egui::Ui, l: &LoadedAnalytics) {
    ui.group(|ui| {
        ui.heading("Messaging Times");
        ui.weak("Day of week × hour, in your local timezone");
        ui.add_space(6.0);
        if l.hourly.is_empty() {
            ui.label("(no hourly data yet)");
            return;
        }
        paint_hourly_heatmap(ui, &l.hourly);
    });
}

fn paint_hourly_heatmap(ui: &mut egui::Ui, hourly: &[HourlyActivityBucket]) {
    let cell_w = 14.0;
    let cell_h = 18.0;
    let gap = 2.0;
    let label_w = 36.0;

    // Find max for color scaling.
    let max_count = hourly.iter().map(|h| h.message_count).max().unwrap_or(1).max(1) as f32;

    // Build a (dow, hour) → count lookup. Indexes that have no row stay at 0.
    let mut grid = [[0i64; 24]; 7];
    for h in hourly {
        if (h.day_of_week as usize) < 7 && (h.hour as usize) < 24 {
            grid[h.day_of_week as usize][h.hour as usize] = h.message_count;
        }
    }

    let total_w = label_w + 24.0 * (cell_w + gap);
    let total_h = 16.0 + 7.0 * (cell_h + gap);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total_w, total_h), egui::Sense::hover());
    let painter = ui.painter();
    let label_color = egui::Color32::from_gray(170);
    let dow_labels = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

    // Hour-of-day labels along the top.
    for hour in 0..24 {
        if hour % 3 == 0 {
            let x = rect.left() + label_w + (hour as f32) * (cell_w + gap) + cell_w / 2.0;
            painter.text(
                egui::pos2(x, rect.top()),
                egui::Align2::CENTER_TOP,
                format!("{:02}", hour),
                egui::FontId::proportional(10.0),
                label_color,
            );
        }
    }

    // Grid cells.
    for dow in 0..7 {
        // Day-of-week label.
        let row_top = rect.top() + 16.0 + (dow as f32) * (cell_h + gap);
        painter.text(
            egui::pos2(rect.left() + label_w - 6.0, row_top + cell_h / 2.0),
            egui::Align2::RIGHT_CENTER,
            dow_labels[dow],
            egui::FontId::proportional(11.0),
            label_color,
        );

        for hour in 0..24 {
            let x = rect.left() + label_w + (hour as f32) * (cell_w + gap);
            let count = grid[dow][hour] as f32;
            let intensity = if count <= 0.0 {
                0.0
            } else {
                ((count + 1.0).ln() / (max_count + 1.0).ln()).clamp(0.0, 1.0)
            };
            // IronBow thermal palette — visually striking and the standard
            // for "more = hotter" data.
            let color = if count <= 0.0 {
                egui::Color32::from_gray(20)
            } else {
                ironbow_color(intensity)
            };
            painter.rect_filled(
                egui::Rect::from_min_size(egui::pos2(x, row_top), egui::vec2(cell_w, cell_h)),
                2.0,
                color,
            );
        }
    }
}

// ===================================================================
// DATA LOADING
// ===================================================================

fn load_contacts(app: &mut SmsArchiveApp) {
    app.analytics.loading = true;
    app.analytics.load_error = None;

    let result = (|| -> anyhow::Result<Vec<AnalyticsContactRow>> {
        let db = Database::open(Path::new(&app.db_path), ResourceProfile::detect())?;
        let conn = db.connection();
        let mut stmt = conn.prepare(
            "SELECT \
                c.id, \
                c.display_name, \
                COALESCE(c.source, 'unknown') AS source, \
                ca.address AS primary_address, \
                COUNT(m.id) AS msg_count \
             FROM contacts c \
             JOIN contact_addresses ca ON ca.contact_id = c.id \
             LEFT JOIN messages m ON m.address = ca.address \
                 AND m.message_direction IN (1, 2) \
                 AND m.address NOT LIKE '%~%' \
             GROUP BY c.id \
             ORDER BY msg_count DESC, c.display_name ASC",
        )?;
        let rows: Vec<AnalyticsContactRow> = stmt
            .query_map([], |r| {
                Ok(AnalyticsContactRow {
                    contact_id: r.get(0)?,
                    display_name: r.get(1)?,
                    source: r.get(2)?,
                    primary_address: r.get(3)?,
                    message_count: r.get(4)?,
                })
            })?
            .filter_map(|x| x.ok())
            .collect();
        Ok(rows)
    })();

    match result {
        Ok(rows) => {
            app.analytics.contacts = rows;
            app.analytics.contacts_loaded = true;
            app.analytics.load_error = None;
        }
        Err(err) => {
            app.analytics.load_error = Some(format!("Failed to load contacts: {}", err));
            app.analytics.contacts.clear();
            app.analytics.contacts_loaded = false;
        }
    }
    app.analytics.loading = false;
}

fn sync_loaded_for_selection(app: &mut SmsArchiveApp) {
    let Some(current) = app.analytics.selected_contact_id.clone() else {
        app.analytics.loaded = None;
        app.analytics.cache_loaded_for = None;
        return;
    };
    if app.analytics.cache_loaded_for.as_deref() == Some(&current) {
        return;
    }
    app.analytics.loaded = read_loaded_analytics(&app.db_path, &current);
    app.analytics.cache_loaded_for = Some(current);
}

fn read_loaded_analytics(db_path: &str, contact_id: &str) -> Option<LoadedAnalytics> {
    let db = Database::open(Path::new(db_path), ResourceProfile::detect()).ok()?;
    let conn = db.connection();

    let display_name: String = conn
        .query_row(
            "SELECT display_name FROM contacts WHERE id = ?1",
            params![contact_id],
            |r| r.get(0),
        )
        .ok()?;

    // status (may not exist if compute never ran)
    let (last_computed_at, is_stale, last_compute_ms, last_error): (i64, bool, Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT last_computed_at, is_stale, last_compute_ms, last_error \
             FROM contact_analytics_status WHERE contact_id = ?1",
            params![contact_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)? != 0,
                    r.get(2)?,
                    r.get(3)?,
                ))
            },
        )
        .unwrap_or((0, false, None, None));

    // contact_analytics row
    let ca_row = conn
        .query_row(
            "SELECT \
                my_message_count, their_message_count, \
                my_word_count, their_word_count, \
                my_unique_word_count, their_unique_word_count, \
                my_character_count, their_character_count, \
                my_image_count, their_image_count, \
                my_video_count, their_video_count, \
                my_audio_count, their_audio_count, \
                my_gif_count, their_gif_count, \
                my_link_count, their_link_count, \
                my_top_emojis, their_top_emojis, \
                my_emoji_total, their_emoji_total, \
                my_laugh_count, their_laugh_count, \
                my_apology_count, their_apology_count, \
                my_question_count, their_question_count, \
                my_encouragement_count, their_encouragement_count \
             FROM contact_analytics WHERE contact_id = ?1",
            params![contact_id],
            |r| {
                Ok(ContactAnalyticsRow {
                    my_messages: r.get(0)?,
                    their_messages: r.get(1)?,
                    my_words: r.get(2)?,
                    their_words: r.get(3)?,
                    my_unique_words: r.get(4)?,
                    their_unique_words: r.get(5)?,
                    my_chars: r.get(6)?,
                    their_chars: r.get(7)?,
                    my_images: r.get(8)?,
                    their_images: r.get(9)?,
                    my_videos: r.get(10)?,
                    their_videos: r.get(11)?,
                    my_audios: r.get(12)?,
                    their_audios: r.get(13)?,
                    my_gifs: r.get(14)?,
                    their_gifs: r.get(15)?,
                    my_links: r.get(16)?,
                    their_links: r.get(17)?,
                    my_top_emojis_json: r.get(18)?,
                    their_top_emojis_json: r.get(19)?,
                    my_emoji_total: r.get(20)?,
                    their_emoji_total: r.get(21)?,
                    my_laughs: r.get(22)?,
                    their_laughs: r.get(23)?,
                    my_apologies: r.get(24)?,
                    their_apologies: r.get(25)?,
                    my_questions: r.get(26)?,
                    their_questions: r.get(27)?,
                    my_encouragement: r.get(28)?,
                    their_encouragement: r.get(29)?,
                })
            },
        )
        .ok();

    // pair_analytics row
    let pa_row = conn
        .query_row(
            "SELECT \
                total_conversations, convos_started_by_me, convos_started_by_them, \
                convos_closed_by_me, convos_closed_by_them, \
                avg_convo_points, median_convo_messages, \
                my_double_messages, their_double_messages, \
                my_convos_missed, their_convos_missed, \
                reconnect_count_t1, reconnect_count_t2, reconnect_count_t3, reconnect_count_t4, \
                top_contributor, \
                my_median_response_ms, their_median_response_ms, \
                my_mean_response_ms, their_mean_response_ms, \
                my_rapid_response_pct, their_rapid_response_pct, \
                my_median_first_response_ms, their_median_first_response_ms, \
                my_median_response_awake_ms, their_median_response_awake_ms, \
                my_median_response_overnight_ms, their_median_response_overnight_ms, \
                my_points, their_points, \
                overall_score, score_responsiveness, score_balance, score_engagement, \
                score_consistency, score_reciprocity, score_longevity, score_mutual_effort, \
                insights_json, writing_milestones_json, \
                first_message_at, last_message_at, \
                conversation_flow_json, \
                focus_me_pct, focus_them_pct, focus_other_pct, \
                sentiment_timeline_json, inside_jokes_json, topics_json \
             FROM pair_analytics WHERE contact_id = ?1",
            params![contact_id],
            |r| {
                Ok(PairAnalyticsRow {
                    total_conversations: r.get(0)?,
                    started_by_me: r.get(1)?,
                    started_by_them: r.get(2)?,
                    closed_by_me: r.get(3)?,
                    closed_by_them: r.get(4)?,
                    avg_convo_points: r.get(5)?,
                    median_convo_messages: r.get(6)?,
                    my_doubles: r.get(7)?,
                    their_doubles: r.get(8)?,
                    my_missed: r.get(9)?,
                    their_missed: r.get(10)?,
                    reconnect_t1: r.get(11)?,
                    reconnect_t2: r.get(12)?,
                    reconnect_t3: r.get(13)?,
                    reconnect_t4: r.get(14)?,
                    top_contributor: r.get(15)?,
                    my_median_resp_ms: r.get(16)?,
                    their_median_resp_ms: r.get(17)?,
                    my_mean_resp_ms: r.get(18)?,
                    their_mean_resp_ms: r.get(19)?,
                    my_rapid_pct: r.get(20)?,
                    their_rapid_pct: r.get(21)?,
                    my_first_median_ms: r.get(22)?,
                    their_first_median_ms: r.get(23)?,
                    my_awake_median_ms: r.get(24)?,
                    their_awake_median_ms: r.get(25)?,
                    my_overnight_median_ms: r.get(26)?,
                    their_overnight_median_ms: r.get(27)?,
                    my_points: r.get(28)?,
                    their_points: r.get(29)?,
                    overall_score: r.get(30)?,
                    score_responsiveness: r.get(31)?,
                    score_balance: r.get(32)?,
                    score_engagement: r.get(33)?,
                    score_consistency: r.get(34)?,
                    score_reciprocity: r.get(35)?,
                    score_longevity: r.get(36)?,
                    score_mutual_effort: r.get(37)?,
                    insights_json: r.get::<_, Option<String>>(38)?.unwrap_or_default(),
                    writing_milestones_json: r.get::<_, Option<String>>(39)?.unwrap_or_default(),
                    first_message_at: r.get(40)?,
                    last_message_at: r.get(41)?,
                    conversation_flow_json: r.get(42)?,
                    focus_me_pct: r.get(43)?,
                    focus_them_pct: r.get(44)?,
                    focus_other_pct: r.get(45)?,
                    sentiment_timeline_json: r.get::<_, Option<String>>(46)?.unwrap_or_default(),
                    inside_jokes_json: r.get::<_, Option<String>>(47)?.unwrap_or_default(),
                    topics_json: r.get::<_, Option<String>>(48)?.unwrap_or_default(),
                })
            },
        )
        .ok();

    let ca = ca_row.unwrap_or_default();
    let pa = pa_row.unwrap_or_default();

    let my_top_emojis: Vec<EmojiCount> =
        serde_json::from_str(&ca.my_top_emojis_json).unwrap_or_default();
    let their_top_emojis: Vec<EmojiCount> =
        serde_json::from_str(&ca.their_top_emojis_json).unwrap_or_default();
    let insights: Vec<LoadedInsight> =
        serde_json::from_str(&pa.insights_json).unwrap_or_default();
    let writing_milestones: Option<WritingMilestonesJson> =
        serde_json::from_str(&pa.writing_milestones_json).ok();

    // activity_daily — chronological. Keep all rows; render layer can pick a window.
    let daily: Vec<DailyActivityPoint> = (|| -> rusqlite::Result<Vec<DailyActivityPoint>> {
        let mut stmt = conn.prepare(
            "SELECT day, my_messages, their_messages, my_points, their_points \
             FROM activity_daily WHERE contact_id = ?1 ORDER BY day ASC",
        )?;
        let rows: Vec<DailyActivityPoint> = stmt
            .query_map(params![contact_id], |r| {
                Ok(DailyActivityPoint {
                    day: r.get(0)?,
                    my_msgs: r.get::<_, i64>(1)?,
                    their_msgs: r.get::<_, i64>(2)?,
                    my_points: r.get::<_, f64>(3)?,
                    their_points: r.get::<_, f64>(4)?,
                })
            })?
            .filter_map(|x| x.ok())
            .collect();
        Ok(rows)
    })()
    .unwrap_or_default();

    // activity_hourly — sorted by (dow, hour) so the heatmap can index directly.
    let hourly: Vec<HourlyActivityBucket> = (|| -> rusqlite::Result<Vec<HourlyActivityBucket>> {
        let mut stmt = conn.prepare(
            "SELECT day_of_week, hour, message_count \
             FROM activity_hourly WHERE contact_id = ?1 ORDER BY day_of_week, hour",
        )?;
        let rows: Vec<HourlyActivityBucket> = stmt
            .query_map(params![contact_id], |r| {
                Ok(HourlyActivityBucket {
                    day_of_week: r.get::<_, i64>(0)? as u8,
                    hour: r.get::<_, i64>(1)? as u8,
                    message_count: r.get(2)?,
                })
            })?
            .filter_map(|x| x.ok())
            .collect();
        Ok(rows)
    })()
    .unwrap_or_default();

    let _ = display_name; // surfaced in the picker row + selection header — not duplicated in LoadedAnalytics

    Some(LoadedAnalytics {
        last_computed_at,
        is_stale,
        last_compute_ms,
        last_error,
        my_messages: ca.my_messages,
        their_messages: ca.their_messages,
        my_words: ca.my_words,
        their_words: ca.their_words,
        my_unique_words: ca.my_unique_words,
        their_unique_words: ca.their_unique_words,
        my_chars: ca.my_chars,
        their_chars: ca.their_chars,
        my_images: ca.my_images,
        their_images: ca.their_images,
        my_videos: ca.my_videos,
        their_videos: ca.their_videos,
        my_audios: ca.my_audios,
        their_audios: ca.their_audios,
        my_gifs: ca.my_gifs,
        their_gifs: ca.their_gifs,
        my_links: ca.my_links,
        their_links: ca.their_links,
        my_top_emojis,
        their_top_emojis,
        my_emoji_total: ca.my_emoji_total,
        their_emoji_total: ca.their_emoji_total,
        my_laughs: ca.my_laughs,
        their_laughs: ca.their_laughs,
        my_apologies: ca.my_apologies,
        their_apologies: ca.their_apologies,
        my_questions: ca.my_questions,
        their_questions: ca.their_questions,
        my_encouragement: ca.my_encouragement,
        their_encouragement: ca.their_encouragement,
        total_conversations: pa.total_conversations,
        started_by_me: pa.started_by_me,
        started_by_them: pa.started_by_them,
        closed_by_me: pa.closed_by_me,
        closed_by_them: pa.closed_by_them,
        avg_convo_points: pa.avg_convo_points,
        median_convo_messages: pa.median_convo_messages,
        my_doubles: pa.my_doubles,
        their_doubles: pa.their_doubles,
        my_missed: pa.my_missed,
        their_missed: pa.their_missed,
        reconnect_t1: pa.reconnect_t1,
        reconnect_t2: pa.reconnect_t2,
        reconnect_t3: pa.reconnect_t3,
        reconnect_t4: pa.reconnect_t4,
        top_contributor: pa.top_contributor,
        my_median_resp_ms: pa.my_median_resp_ms,
        their_median_resp_ms: pa.their_median_resp_ms,
        my_mean_resp_ms: pa.my_mean_resp_ms,
        their_mean_resp_ms: pa.their_mean_resp_ms,
        my_rapid_pct: pa.my_rapid_pct,
        their_rapid_pct: pa.their_rapid_pct,
        my_first_median_ms: pa.my_first_median_ms,
        their_first_median_ms: pa.their_first_median_ms,
        my_awake_median_ms: pa.my_awake_median_ms,
        their_awake_median_ms: pa.their_awake_median_ms,
        my_overnight_median_ms: pa.my_overnight_median_ms,
        their_overnight_median_ms: pa.their_overnight_median_ms,
        my_points: pa.my_points,
        their_points: pa.their_points,
        overall_score: pa.overall_score,
        score_responsiveness: pa.score_responsiveness,
        score_balance: pa.score_balance,
        score_engagement: pa.score_engagement,
        score_consistency: pa.score_consistency,
        score_reciprocity: pa.score_reciprocity,
        score_longevity: pa.score_longevity,
        score_mutual_effort: pa.score_mutual_effort,
        insights,
        writing_milestones,
        conversation_flow_json: pa.conversation_flow_json,
        focus_me_pct: pa.focus_me_pct,
        focus_them_pct: pa.focus_them_pct,
        focus_other_pct: pa.focus_other_pct,
        sentiment_timeline: serde_json::from_str(&pa.sentiment_timeline_json).ok(),
        inside_jokes: serde_json::from_str(&pa.inside_jokes_json).unwrap_or_default(),
        topics: serde_json::from_str(&pa.topics_json).unwrap_or_default(),
        first_message_ms: pa.first_message_at,
        last_message_ms: pa.last_message_at,
        daily,
        hourly,
    })
}

// Internal scratch structs used by the loader. Keeps the giant query_row
// closures from polluting LoadedAnalytics with private intermediate state.
#[derive(Default)]
struct ContactAnalyticsRow {
    my_messages: i64,
    their_messages: i64,
    my_words: i64,
    their_words: i64,
    my_unique_words: i64,
    their_unique_words: i64,
    my_chars: i64,
    their_chars: i64,
    my_images: i64,
    their_images: i64,
    my_videos: i64,
    their_videos: i64,
    my_audios: i64,
    their_audios: i64,
    my_gifs: i64,
    their_gifs: i64,
    my_links: i64,
    their_links: i64,
    my_top_emojis_json: String,
    their_top_emojis_json: String,
    my_emoji_total: i64,
    their_emoji_total: i64,
    my_laughs: i64,
    their_laughs: i64,
    my_apologies: i64,
    their_apologies: i64,
    my_questions: i64,
    their_questions: i64,
    my_encouragement: i64,
    their_encouragement: i64,
}

#[derive(Default)]
struct PairAnalyticsRow {
    total_conversations: i64,
    started_by_me: i64,
    started_by_them: i64,
    closed_by_me: i64,
    closed_by_them: i64,
    avg_convo_points: f64,
    median_convo_messages: f64,
    my_doubles: i64,
    their_doubles: i64,
    my_missed: i64,
    their_missed: i64,
    reconnect_t1: i64,
    reconnect_t2: i64,
    reconnect_t3: i64,
    reconnect_t4: i64,
    top_contributor: Option<i64>,
    my_median_resp_ms: Option<i64>,
    their_median_resp_ms: Option<i64>,
    my_mean_resp_ms: Option<i64>,
    their_mean_resp_ms: Option<i64>,
    my_rapid_pct: Option<f64>,
    their_rapid_pct: Option<f64>,
    my_first_median_ms: Option<i64>,
    their_first_median_ms: Option<i64>,
    my_awake_median_ms: Option<i64>,
    their_awake_median_ms: Option<i64>,
    my_overnight_median_ms: Option<i64>,
    their_overnight_median_ms: Option<i64>,
    my_points: f64,
    their_points: f64,
    overall_score: Option<i64>,
    score_responsiveness: Option<i64>,
    score_balance: Option<i64>,
    score_engagement: Option<i64>,
    score_consistency: Option<i64>,
    score_reciprocity: Option<i64>,
    score_longevity: Option<i64>,
    score_mutual_effort: Option<i64>,
    insights_json: String,
    writing_milestones_json: String,
    first_message_at: Option<i64>,
    last_message_at: Option<i64>,
    conversation_flow_json: Option<String>,
    focus_me_pct: Option<f64>,
    focus_them_pct: Option<f64>,
    focus_other_pct: Option<f64>,
    sentiment_timeline_json: String,
    inside_jokes_json: String,
    topics_json: String,
}

// ===================================================================
// COMPUTE THREAD
// ===================================================================

fn spawn_compute(app: &mut SmsArchiveApp, contact_id: &str) {
    let db_path = app.db_path.clone();
    let contact = contact_id.to_string();
    let tz_offset = app.analytics.tz_offset_secs;
    // Snapshot settings at button-click time so mid-compute edits don't change
    // what the worker uses. If settings haven't been loaded from DB yet, the
    // worker will load them itself.
    let settings_snapshot = if app.analytics.settings_loaded {
        Some(app.analytics.settings.clone())
    } else {
        None
    };
    let slot = Arc::new(Mutex::new(ComputeSlot::Running));
    let slot_for_thread = Arc::clone(&slot);

    std::thread::spawn(move || {
        let outcome: Result<sms_analytics::OrchestratorOutput, String> = (|| {
            let db = Database::open(Path::new(&db_path), ResourceProfile::detect())
                .map_err(|e| format!("open db: {}", e))?;
            let conn = db.connection();
            let settings = match settings_snapshot {
                Some(s) => s,
                None => load_settings_from_db(&db_path).unwrap_or_default(),
            };
            let config = settings.to_orchestrator_config(tz_offset);
            sms_analytics::compute_for_contact(conn, &contact, &config)
                .map_err(|e| format!("orchestrator: {}", e))
        })();
        if let Ok(mut guard) = slot_for_thread.lock() {
            *guard = match outcome {
                Ok(out) => ComputeSlot::Done(out),
                Err(e) => ComputeSlot::Failed(e),
            };
        }
    });

    app.analytics.compute_pending = Some(slot);
    app.analytics.compute_status_msg = "Running…".to_string();
}

fn poll_compute_slot(app: &mut SmsArchiveApp) {
    let Some(slot) = app.analytics.compute_pending.clone() else {
        return;
    };
    let outcome: Option<ComputeSlot> = {
        let Ok(mut guard) = slot.lock() else { return };
        match &*guard {
            ComputeSlot::Running => None,
            _ => {
                let taken = std::mem::replace(&mut *guard, ComputeSlot::Running);
                Some(taken)
            }
        }
    };
    let Some(outcome) = outcome else { return };

    match outcome {
        ComputeSlot::Done(out) => {
            app.analytics.compute_status_msg = format!(
                "Completed: {} messages, {} convos in {} ms",
                out.message_count, out.conversation_count, out.elapsed_ms
            );
            app.analytics.cache_loaded_for = None; // force reload
        }
        ComputeSlot::Failed(err) => {
            app.analytics.compute_status_msg = format!("Failed: {}", err);
        }
        ComputeSlot::Running => {}
    }
    app.analytics.compute_pending = None;
}
