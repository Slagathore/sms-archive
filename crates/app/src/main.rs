use anyhow::Result;
use base64::Engine;
use chrono::TimeZone;
use eframe::egui;
use egui::load::SizedTexture;
use lru::LruCache;
use nom_exif::{ExifIter, MediaParser, MediaSource};
use rfd::FileDialog;
use rusqlite::{named_params, params, OptionalExtension};
use sms_config::{
    available_disk_bytes, calculate_minimum_resources, detect_resource_limits, init_logging,
    ResourceProfile,
};
use sms_db::Database;
use sms_ingest::{ingest_file, IngestOptions, IngestProgress};
use sms_media::keyframes::{cleanup_temp_dir, extract_keyframes, Keyframe};
use sms_media_process::{process_media, MediaProcessOptions};
use sms_ml::{DevicePreference, EmbeddingConfig, EmbeddingService};
use sms_search::{semantic_search, Fts5Backend, SemanticHit};
use sms_types::Message;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
use walkdir::WalkDir;

mod analytics_tab;
mod theming;
mod thumbnails;
mod vcard_fmt;

use thumbnails::{thumb_placeholder, PreviewCache, ThumbJob, ThumbReady, ThumbnailLoader};
use vcard_fmt::{
    format_vcard_address, format_vcard_name, parse_vcard_property, unfold_vcard_lines,
    vcard_escape, vcard_phone_type_from_params, vcard_phone_type_label, vcard_unescape,
};

/// Result slot for the media grid query: (rows for the current page, total count).
type PendingMediaSlot = Arc<Mutex<Option<(Vec<AttachmentRow>, usize)>>>;

struct SmsArchiveApp {
    active_tab: AppTab,
    db_path: String,
    db_folder: Option<PathBuf>,
    media_root: Option<PathBuf>,
    new_db_name: String,
    status: String,
    message_count: i64,
    search_query: String,
    search_filters: SearchFilters,
    search_backend: Option<Fts5Backend>,
    results: Vec<Message>,
    selected: Option<Message>,
    selected_attachments: Vec<AttachmentRow>,
    page_size: usize,
    page_offset: usize,
    page_jump_input: String,
    search_in_flight: bool,
    pending_results: Arc<Mutex<Option<Vec<Message>>>>,
    search_max_results: String,
    search_unlimited: bool,
    context_window_size: String,
    show_perf: bool,
    semantic_query: String,
    semantic_hits: Vec<SemanticHit>,
    semantic_status: String,
    semantic_in_flight: bool,
    pending_semantic: Arc<Mutex<Option<Vec<SemanticHit>>>>,
    semantic_limit: usize,
    model_stats: Vec<ModelStat>,
    model_stats_total: i64,
    model_stats_status: String,
    model_stats_in_flight: bool,
    pending_model_stats: Arc<Mutex<Option<ModelStatsSnapshot>>>,
    selected_model_id: Option<String>,
    model_action_in_flight: bool,
    pending_model_action: Arc<Mutex<Option<String>>>,
    embed_generation: u64,
    import_input: String,
    import_status: String,
    import_job: Option<ImportJob>,
    checkpoint_info: Option<CheckpointInfo>,
    resume_from_checkpoint: bool,
    checkpoint_db_path: String,
    import_last_offset: u64,
    import_last_update: Instant,
    import_stuck_threshold: Duration,
    show_stuck_dialog: bool,
    import_pause_start: Option<Instant>,
    import_total_paused: Duration,
    thumbnail_cache: LruCache<String, egui::TextureHandle>,
    thumbnail_loader: ThumbnailLoader,
    last_frame: Instant,
    frame_ms_ema: f32,
    preview_cache: Option<PreviewCache>,
    preview_cache_db_path: String,
    embed_job: Option<EmbedJob>,
    embed_status: String,
    embed_model_path: String,
    embed_tokenizer_path: String,
    embed_model_name: String,
    embed_model_version: String,
    embed_dimensions: usize,
    embed_batch_size: usize,
    embed_max_length: usize,
    embed_normalize: bool,
    embed_device: DevicePreference,
    use_ollama: bool,
    ollama_base_url: String,
    ollama_models: Vec<OllamaModel>,
    ollama_selected: String,
    ollama_pull_name: String,
    ollama_status: String,
    ollama_log: Vec<String>,
    ollama_in_flight: bool,
    ollama_models_source: String,
    pending_ollama_models: Arc<Mutex<Option<Vec<OllamaModel>>>>,
    pending_ollama_log: Arc<Mutex<Vec<String>>>,
    /// Set by the model-pull/refresh workers when they finish; log lines
    /// alone must not clear `ollama_in_flight` (a pull streams lines for
    /// minutes while still running).
    ollama_job_done: Arc<AtomicBool>,
    assistant_base_url: String,
    assistant_model: String,
    assistant_model_status: String,
    assistant_model_check_in_flight: bool,
    pending_assistant_model_check: Arc<Mutex<Option<String>>>,
    vision_base_url: String,
    vision_model: String,
    vision_prompt: String,
    vision_model_status: String,
    vision_model_check_in_flight: bool,
    pending_vision_model_check: Arc<Mutex<Option<String>>>,
    tesseract_cmd: String,
    ocr_status: String,
    media_query: String,
    media_results: Vec<AttachmentRow>,
    media_nsfw_filter: MediaNsfwFilter,
    media_embed_prompt: String,
    media_embed_use_local: bool,
    media_nsfw_prompt: String,
    media_nsfw_threshold: f32,
    media_keyframe_max: usize,
    media_embed_status: String,
    pending_media_embed_status: Arc<Mutex<Vec<String>>>,
    pending_media_embed_done: Arc<Mutex<Vec<String>>>,
    ffmpeg_status: String,
    media_semantic_query: String,
    media_semantic_hits: Vec<MediaSemanticHit>,
    media_semantic_status: String,
    media_semantic_in_flight: bool,
    media_semantic_use_clip: bool,
    media_semantic_limit: usize,
    pending_media_semantic: Arc<Mutex<Option<Vec<MediaSemanticHit>>>>,
    pending_fts_rebuild: Arc<Mutex<Option<String>>>,
    media_page_size: usize,
    media_page_offset: usize,
    media_total_count: usize,
    media_in_flight: bool,
    pending_media: PendingMediaSlot,
    selected_media_ids: HashSet<String>,
    media_batch_status: String,
    pending_ocr: Arc<Mutex<Vec<OcrUpdate>>>,
    ocr_in_progress: HashSet<String>,
    pending_vision: Arc<Mutex<Vec<VisionUpdate>>>,
    vision_in_progress: HashSet<String>,
    nsfw_in_progress: HashSet<String>,
    pending_nsfw: Arc<Mutex<Vec<NsfwUpdate>>>,
    media_embed_in_progress: HashSet<String>,
    media_embed_inspect_target: Option<String>,
    media_embed_inspect_rows: Vec<MediaEmbedInspectRow>,
    media_embed_inspect_status: String,
    media_embed_inspect_in_flight: bool,
    pending_media_embed_inspect: Arc<Mutex<Option<Vec<MediaEmbedInspectRow>>>>,
    media_audit_status: String,
    media_audit_in_flight: bool,
    media_audit_snapshot: Option<MediaAuditSnapshot>,
    pending_media_audit: Arc<Mutex<Option<MediaAuditSnapshot>>>,
    media_backfill_in_flight: bool,
    media_backfill_status: String,
    pending_media_backfill: Arc<Mutex<Option<(String, bool)>>>, // (status, is_done)
    clip_model_path: String,
    clip_nsfw_weights_path: String,
    clip_batch_size: usize,
    clip_max_keyframes: usize,
    clip_workers: usize,
    clip_reprocess: bool,
    clip_auto_on_import: bool,
    clip_use_cuda: bool,
    clip_status: String,
    clip_cuda_status: String,
    clip_text_model_path: String,
    clip_text_tokenizer_path: String,
    clip_job: Option<MediaProcessJob>,
    pending_thread_results: Arc<Mutex<Option<Vec<Message>>>>,
    thread_results: Vec<Message>,
    thread_attachments: HashMap<uuid::Uuid, Vec<AttachmentRow>>,
    thread_in_flight: bool,
    thread_anchor: Option<uuid::Uuid>,
    thread_scroll_to_anchor: bool,
    thread_limit: usize,
    contact_search: String,
    self_addresses: Vec<String>,
    self_address_input: String,
    contacts: Vec<ContactSummary>,
    contacts_in_flight: bool,
    contact_status: String,
    pending_contacts: Arc<Mutex<Option<ContactSnapshot>>>,
    selected_contact_id: Option<String>,
    contact_detail: Option<ContactDetail>,
    pending_contact_detail: Arc<Mutex<Option<ContactDetail>>>,
    pending_contact_status: Arc<Mutex<Option<String>>>,
    contact_new_address: String,
    contact_merge_source: Option<String>,
    contact_merge_state: Option<ContactMergeState>,
    duplicate_groups: Vec<Vec<ContactSummary>>,
    duplicates_in_flight: bool,
    pending_duplicate_groups: Arc<Mutex<Option<Vec<Vec<ContactSummary>>>>>,
    contact_name_cache: HashMap<String, String>,
    timeline_stats: Option<TimelineStats>,
    timeline_filters: TimelineFilters,
    timeline_chart_mode: TimelineChartMode,
    timeline_name_query: String,
    timeline_selected_addresses: HashSet<String>,
    timeline_in_flight: bool,
    pending_timeline: Arc<Mutex<Option<TimelineStats>>>,
    assistant: sms_assistant::Assistant,
    assistant_input: String,
    assistant_waiting: bool,
    pending_assistant: Arc<Mutex<Option<Result<Vec<sms_assistant::ChatMessage>>>>>,
    /// Live-streaming assistant answer, appended by the worker as tokens arrive.
    assistant_stream: Arc<Mutex<String>>,
    /// Set by the "Stop" button to abort an in-flight assistant response.
    assistant_cancel: Arc<AtomicBool>,
    map_filters: MapFilters,
    map_points: Vec<MapPoint>,
    map_in_flight: bool,
    map_status: String,
    map_selected: Option<usize>,
    pending_map: Arc<Mutex<Option<Vec<MapPoint>>>>,
    map_tiles: HashMap<MapTileKey, egui::TextureHandle>,
    map_tiles_in_flight: HashSet<MapTileKey>,
    pending_map_tiles: Arc<Mutex<Vec<MapTileUpdate>>>,
    log_filter: String,
    log_files: Vec<PathBuf>,
    log_selected: Option<PathBuf>,
    log_lines: Vec<String>,
    log_status: String,
    log_max_lines: usize,
    log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
    ui_settings_snapshot: String,
    ui_settings_last_save: Instant,
    analytics: analytics_tab::AnalyticsTabState,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct GlobalSettings {
    clip_model_path: Option<String>,
    clip_nsfw_weights_path: Option<String>,
    clip_text_model_path: Option<String>,
    clip_text_tokenizer_path: Option<String>,
    media_embed_prompt: Option<String>,
    vision_prompt: Option<String>,
    tesseract_cmd: Option<String>,
    // #todo: persist more global defaults (embeddings model, base URLs, NSFW prompt).
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct UiSettings {
    version: u32,
    active_tab: AppTab,
    db_path: String,
    #[serde(default)]
    db_folder: Option<String>,
    new_db_name: String,
    search_query: String,
    search_filters: SearchFilters,
    search_max_results: String,
    search_unlimited: bool,
    context_window_size: String,
    show_perf: bool,
    page_size: usize,
    page_offset: usize,
    page_jump_input: String,
    semantic_query: String,
    semantic_limit: usize,
    import_input: String,
    resume_from_checkpoint: bool,
    checkpoint_db_path: String,
    embed_model_path: String,
    embed_tokenizer_path: String,
    embed_model_name: String,
    embed_model_version: String,
    embed_dimensions: usize,
    embed_batch_size: usize,
    embed_max_length: usize,
    embed_normalize: bool,
    embed_device: String,
    use_ollama: bool,
    ollama_base_url: String,
    ollama_selected: String,
    ollama_pull_name: String,
    ollama_models_source: String,
    assistant_base_url: String,
    assistant_model: String,
    #[serde(default)]
    assistant_input: String,
    vision_base_url: String,
    vision_model: String,
    vision_prompt: String,
    tesseract_cmd: String,
    media_query: String,
    media_nsfw_filter: MediaNsfwFilter,
    media_embed_prompt: String,
    media_embed_use_local: bool,
    media_nsfw_prompt: String,
    media_nsfw_threshold: f32,
    media_keyframe_max: usize,
    media_semantic_query: String,
    media_semantic_use_clip: bool,
    media_semantic_limit: usize,
    media_page_size: usize,
    media_page_offset: usize,
    clip_model_path: String,
    clip_nsfw_weights_path: String,
    clip_batch_size: usize,
    clip_max_keyframes: usize,
    clip_workers: usize,
    clip_reprocess: bool,
    clip_auto_on_import: bool,
    clip_use_cuda: bool,
    clip_text_model_path: String,
    clip_text_tokenizer_path: String,
    #[serde(default)]
    selected_model_id: Option<String>,
    thread_limit: usize,
    contact_search: String,
    #[serde(default)]
    self_addresses: Vec<String>,
    #[serde(default)]
    self_address_input: String,
    timeline_filters: TimelineFilters,
    timeline_chart_mode: TimelineChartMode,
    timeline_name_query: String,
    #[serde(default)]
    timeline_selected_addresses: Vec<String>,
    map_filters: MapFilters,
    log_filter: String,
    log_max_lines: usize,
    #[serde(default)]
    log_selected: Option<String>,
}

impl Default for SmsArchiveApp {
    fn default() -> Self {
        let assistant_base_url = "http://localhost:11434".to_string();
        let assistant_model = "llama2".to_string();
        let vision_base_url = assistant_base_url.clone();
        let vision_model = "llava".to_string();
        let vision_prompt = "Describe this image and extract any visible text.".to_string();
        let clip_workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8);
        let mut app = Self {
            active_tab: AppTab::Import,
            db_path: String::new(),
            db_folder: None,
            media_root: None,
            new_db_name: "sms_archive.db".to_string(),
            status: String::new(),
            message_count: 0,
            search_query: String::new(),
            search_filters: SearchFilters::default(),
            search_backend: None,
            results: Vec::new(),
            selected: None,
            selected_attachments: Vec::new(),
            page_size: 100,
            page_offset: 0,
            page_jump_input: String::new(),
            search_in_flight: false,
            pending_results: Arc::new(Mutex::new(None)),
            search_max_results: "1000".to_string(),
            search_unlimited: false,
            context_window_size: "25".to_string(),
            semantic_query: String::new(),
            semantic_hits: Vec::new(),
            semantic_status: String::new(),
            semantic_in_flight: false,
            pending_semantic: Arc::new(Mutex::new(None)),
            semantic_limit: 200,
            model_stats: Vec::new(),
            model_stats_total: 0,
            model_stats_status: String::new(),
            model_stats_in_flight: false,
            pending_model_stats: Arc::new(Mutex::new(None)),
            selected_model_id: None,
            model_action_in_flight: false,
            pending_model_action: Arc::new(Mutex::new(None)),
            embed_generation: 0,
            import_input: String::new(),
            import_status: String::new(),
            import_job: None,
            checkpoint_info: None,
            resume_from_checkpoint: false,
            checkpoint_db_path: String::new(),
            import_last_offset: 0,
            import_last_update: Instant::now(),
            import_stuck_threshold: Duration::from_secs(30),
            show_stuck_dialog: false,
            import_pause_start: None,
            import_total_paused: Duration::ZERO,
            thumbnail_cache: LruCache::new(NonZeroUsize::new(256).unwrap()),
            thumbnail_loader: ThumbnailLoader::default(),
            last_frame: Instant::now(),
            frame_ms_ema: 0.0,
            show_perf: false,
            preview_cache: None,
            preview_cache_db_path: String::new(),
            embed_job: None,
            embed_status: String::new(),
            embed_model_path: String::new(),
            embed_tokenizer_path: String::new(),
            embed_model_name: "hash-embed".to_string(),
            embed_model_version: "v1".to_string(),
            embed_dimensions: 384,
            embed_batch_size: 256,
            embed_max_length: 256,
            embed_normalize: true,
            embed_device: DevicePreference::Cpu,
            use_ollama: false,
            ollama_base_url: "http://localhost:11434".to_string(),
            ollama_models: Vec::new(),
            ollama_selected: String::new(),
            ollama_pull_name: String::new(),
            ollama_status: String::new(),
            ollama_log: Vec::new(),
            ollama_in_flight: false,
            ollama_models_source: String::new(),
            pending_ollama_models: Arc::new(Mutex::new(None)),
            pending_ollama_log: Arc::new(Mutex::new(Vec::new())),
            ollama_job_done: Arc::new(AtomicBool::new(false)),
            assistant_base_url: assistant_base_url.clone(),
            assistant_model: assistant_model.clone(),
            assistant_model_status: String::new(),
            assistant_model_check_in_flight: false,
            pending_assistant_model_check: Arc::new(Mutex::new(None)),
            vision_base_url,
            vision_model,
            vision_prompt,
            vision_model_status: String::new(),
            vision_model_check_in_flight: false,
            pending_vision_model_check: Arc::new(Mutex::new(None)),
            tesseract_cmd: String::new(),
            ocr_status: String::new(),
            media_query: String::new(),
            media_results: Vec::new(),
            media_nsfw_filter: MediaNsfwFilter::ShowAll,
            media_embed_prompt: "Summarize this image in one short sentence.".to_string(),
            media_embed_use_local: false,
            media_nsfw_prompt:
                "Return JSON with fields: label ('NSFW' or 'SAFE') and score (0-1) for this image."
                    .to_string(),
            media_nsfw_threshold: 0.5,
            media_keyframe_max: 6,
            media_embed_status: String::new(),
            pending_media_embed_status: Arc::new(Mutex::new(Vec::new())),
            pending_media_embed_done: Arc::new(Mutex::new(Vec::new())),
            ffmpeg_status: String::new(),
            media_semantic_query: String::new(),
            media_semantic_hits: Vec::new(),
            media_semantic_status: String::new(),
            media_semantic_in_flight: false,
            media_semantic_use_clip: false,
            media_semantic_limit: 50,
            pending_media_semantic: Arc::new(Mutex::new(None)),
            pending_fts_rebuild: Arc::new(Mutex::new(None)),
            media_page_size: 100,
            media_page_offset: 0,
            media_total_count: 0,
            media_in_flight: false,
            pending_media: Arc::new(Mutex::new(None)),
            selected_media_ids: HashSet::new(),
            media_batch_status: String::new(),
            pending_ocr: Arc::new(Mutex::new(Vec::new())),
            ocr_in_progress: HashSet::new(),
            pending_vision: Arc::new(Mutex::new(Vec::new())),
            vision_in_progress: HashSet::new(),
            nsfw_in_progress: HashSet::new(),
            pending_nsfw: Arc::new(Mutex::new(Vec::new())),
            media_embed_in_progress: HashSet::new(),
            media_embed_inspect_target: None,
            media_embed_inspect_rows: Vec::new(),
            media_embed_inspect_status: String::new(),
            media_embed_inspect_in_flight: false,
            pending_media_embed_inspect: Arc::new(Mutex::new(None)),
            media_audit_status: String::new(),
            media_audit_in_flight: false,
            media_audit_snapshot: None,
            pending_media_audit: Arc::new(Mutex::new(None)),
            media_backfill_in_flight: false,
            media_backfill_status: String::new(),
            pending_media_backfill: Arc::new(Mutex::new(None)), // (status, is_done)
            clip_model_path: String::new(),
            clip_nsfw_weights_path: String::new(),
            clip_batch_size: 32,
            clip_max_keyframes: 5,
            clip_workers,
            clip_reprocess: false,
            clip_auto_on_import: false,
            clip_use_cuda: true,
            clip_status: String::new(),
            clip_cuda_status: String::new(),
            clip_text_model_path: String::new(),
            clip_text_tokenizer_path: String::new(),
            clip_job: None,
            pending_thread_results: Arc::new(Mutex::new(None)),
            thread_results: Vec::new(),
            thread_attachments: HashMap::new(),
            thread_in_flight: false,
            thread_anchor: None,
            thread_scroll_to_anchor: false,
            thread_limit: 50,
            contact_search: String::new(),
            contacts: Vec::new(),
            contacts_in_flight: false,
            contact_status: String::new(),
            pending_contacts: Arc::new(Mutex::new(None)),
            selected_contact_id: None,
            contact_detail: None,
            pending_contact_detail: Arc::new(Mutex::new(None)),
            pending_contact_status: Arc::new(Mutex::new(None)),
            contact_new_address: String::new(),
            contact_merge_source: None,
            contact_merge_state: None,
            duplicate_groups: Vec::new(),
            duplicates_in_flight: false,
            pending_duplicate_groups: Arc::new(Mutex::new(None)),
            contact_name_cache: HashMap::new(),
            self_addresses: Vec::new(),
            self_address_input: String::new(),
            timeline_stats: None,
            timeline_filters: TimelineFilters::default(),
            timeline_chart_mode: TimelineChartMode::default(),
            timeline_name_query: String::new(),
            timeline_selected_addresses: HashSet::new(),
            timeline_in_flight: false,
            pending_timeline: Arc::new(Mutex::new(None)),
            assistant: sms_assistant::Assistant::new(assistant_base_url, assistant_model),
            assistant_input: String::new(),
            assistant_waiting: false,
            pending_assistant: Arc::new(Mutex::new(None)),
            assistant_stream: Arc::new(Mutex::new(String::new())),
            assistant_cancel: Arc::new(AtomicBool::new(false)),
            map_filters: MapFilters::default(),
            map_points: Vec::new(),
            map_in_flight: false,
            map_status: String::new(),
            map_selected: None,
            pending_map: Arc::new(Mutex::new(None)),
            map_tiles: HashMap::new(),
            map_tiles_in_flight: HashSet::new(),
            pending_map_tiles: Arc::new(Mutex::new(Vec::new())),
            log_filter: String::new(),
            log_files: Vec::new(),
            log_selected: None,
            log_lines: Vec::new(),
            log_status: String::new(),
            log_max_lines: 2000,
            log_guard: None,
            ui_settings_snapshot: String::new(),
            ui_settings_last_save: Instant::now(),
            analytics: analytics_tab::AnalyticsTabState::new(),
        };
        app.apply_global_settings(load_global_settings());
        app.apply_ui_settings(load_ui_settings());
        app
    }
}

#[derive(Debug, serde::Deserialize)]
struct CheckpointInfo {
    last_committed_offset: u64,
    messages_imported: u64,
    started_at: String,
}

struct ImportJob {
    progress: Arc<IngestProgress>,
    handle: JoinHandle<sms_errors::Result<sms_ingest::IngestStats>>,
    started_at: Instant,
    inputs: Vec<PathBuf>,
}

struct EmbedJob {
    progress: Arc<EmbedProgress>,
    handle: JoinHandle<sms_errors::Result<EmbedStats>>,
    started_at: Instant,
}

struct MediaProcessJob {
    progress: Arc<ClipProgress>,
    handle: JoinHandle<sms_errors::Result<sms_media_process::MediaProcessStats>>,
    started_at: Instant,
}

#[derive(Debug, Default)]
struct EmbedProgress {
    total: AtomicU64,
    done: AtomicU64,
    cancelled: AtomicBool,
}

#[derive(Debug, Clone)]
struct ClipProgress {
    paused: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    gps_in_progress: Arc<AtomicBool>,
    gps_tagged: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    done: Arc<AtomicU64>,
}

impl Default for ClipProgress {
    fn default() -> Self {
        Self {
            paused: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            gps_in_progress: Arc::new(AtomicBool::new(false)),
            gps_tagged: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(0)),
            done: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[derive(Debug)]
struct EmbedStats {
    embedded: u64,
    elapsed_ms: u128,
}

#[derive(Debug, Clone)]
struct AttachmentRow {
    id: String,
    mime_type: String,
    file_path: String,
    thumbnail_path: Option<String>,
    message_id: Option<String>,
    thread_id: Option<String>,
    timestamp: Option<i64>,
    address: Option<String>,
    ocr_text: Option<String>,
    ocr_model: Option<String>,
    ocr_timestamp: Option<i64>,
    vision_analysis: Option<String>,
    vision_model: Option<String>,
    vision_timestamp: Option<i64>,
    nsfw_label: Option<String>,
    nsfw_score: Option<f64>,
    nsfw_model: Option<String>,
    nsfw_timestamp: Option<i64>,
}

#[derive(Debug, Clone)]
struct MediaSemanticHit {
    score: f32,
    attachment: AttachmentRow,
    frame_index: i64,
    frame_time_ms: Option<i64>,
    caption: Option<String>,
    embedding_stats: Option<EmbeddingStats>,
}

#[derive(Debug, Clone)]
struct MediaEmbedInspectRow {
    model_name: String,
    model_version: String,
    frame_index: i64,
    frame_time_ms: Option<i64>,
    caption: Option<String>,
    stats: EmbeddingStats,
}

#[derive(Debug, Clone, Default)]
struct MediaAuditSnapshot {
    media_root: Option<PathBuf>,
    media_files_total: usize,
    db_attachments_total: usize,
    db_image_video_total: usize,
    db_missing_files: usize,
    db_missing_samples: Vec<String>,
    db_mime_counts: Vec<(String, usize)>,
    fs_unlinked_total: usize,
    fs_unlinked_samples: Vec<String>,
}

#[derive(Debug, Clone)]
struct EmbeddingStats {
    dims: usize,
    min: f32,
    max: f32,
    mean: f32,
    norm: f32,
    head: Vec<f32>,
}

#[derive(Debug, Clone)]
struct OcrPayload {
    text: String,
    model: String,
    timestamp: i64,
}

struct OcrUpdate {
    attachment_id: String,
    result: Result<OcrPayload>,
}

#[derive(Debug, Clone)]
struct VisionPayload {
    analysis: String,
    model: String,
    timestamp: i64,
}

struct VisionUpdate {
    attachment_id: String,
    result: Result<VisionPayload>,
}

#[derive(Debug, Clone)]
struct NsfwPayload {
    label: String,
    score: f64,
    model: String,
    timestamp: i64,
}

struct NsfwUpdate {
    attachment_id: String,
    result: Result<NsfwPayload>,
}

#[derive(Debug, Clone)]
struct ModelStat {
    id: String,
    name: String,
    version: String,
    sha256: Option<String>,
    created_at: i64,
    embedding_count: i64,
    dims: Option<i64>,
    max_length: Option<i64>,
    normalize: Option<i64>,
    tokenizer_path: Option<String>,
    input_ids_name: Option<String>,
    attention_mask_name: Option<String>,
    token_type_ids_name: Option<String>,
    output_name: Option<String>,
}

#[derive(Debug, Default)]
struct ModelStatsSnapshot {
    total_messages: i64,
    models: Vec<ModelStat>,
}

#[derive(Debug, Clone)]
struct ContactSummary {
    id: String,
    display_name: String,
    primary: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ContactDetail {
    id: String,
    display_name: String,
    nickname: String,
    company: String,
    notes: String,
    email: String,
    phone_primary: String,
    phone_secondary: String,
    phone_primary_type: String,   // Phase 3: NEW
    phone_secondary_type: String, // Phase 3: NEW
    website: String,              // Phase 3: NEW
    social_media: String,         // Phase 3: NEW
    address: String,
    birthday: String,
    avatar_path: String,
    last_contacted: Option<i64>, // Phase 3: NEW
    favorite: bool,              // Phase 3: NEW
    addresses: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct ContactSnapshot {
    contacts: Vec<ContactSummary>,
    address_map: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeChoice {
    Target,
    Source,
}

#[derive(Debug, Clone)]
struct ContactMergeState {
    target: ContactDetail,
    source: ContactDetail,
    name: MergeChoice,
    nickname: MergeChoice,
    company: MergeChoice,
    notes: MergeChoice,
    email: MergeChoice,
    phone_primary: MergeChoice,
    phone_secondary: MergeChoice,
    phone_primary_type: MergeChoice,   // Phase 3: NEW
    phone_secondary_type: MergeChoice, // Phase 3: NEW
    website: MergeChoice,              // Phase 3: NEW
    social_media: MergeChoice,         // Phase 3: NEW
    address: MergeChoice,
    birthday: MergeChoice,
    avatar_path: MergeChoice,
    last_contacted: MergeChoice, // Phase 3: NEW
    favorite: MergeChoice,       // Phase 3: NEW
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
enum TimelineGranularity {
    Day,
    Week,
    #[default]
    Month,
}

impl TimelineGranularity {
    fn label(self) -> &'static str {
        match self {
            TimelineGranularity::Day => "Day",
            TimelineGranularity::Week => "Week",
            TimelineGranularity::Month => "Month",
        }
    }

    fn strftime(self) -> &'static str {
        match self {
            TimelineGranularity::Day => "%Y-%m-%d",
            TimelineGranularity::Week => "%Y-W%W",
            TimelineGranularity::Month => "%Y-%m",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
enum TimelineChartMode {
    #[default]
    Bar,
    Line,
}

impl TimelineChartMode {
    fn label(self) -> &'static str {
        match self {
            TimelineChartMode::Bar => "Bar",
            TimelineChartMode::Line => "Line",
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct TimelineFilters {
    since: String,
    until: String,
    address: String,
    granularity: TimelineGranularity,
}

#[derive(Debug, Clone, Default)]
struct TimelineStats {
    total_messages: i64,
    sent_messages: i64,
    received_messages: i64,
    total_attachments: i64,
    total_threads: i64,
    busiest_hour: Option<(i64, i64)>,
    busiest_bucket: Option<(String, i64)>,
    series_text: Vec<(String, i64)>,
    series_media: Vec<(String, i64)>,
    top_contacts: Vec<(String, i64)>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct MapFilters {
    since: String,
    until: String,
    address: String,
    mime_prefix: String,
}

#[derive(Debug, Clone)]
struct MapPoint {
    lat: f64,
    lon: f64,
    file_path: String,
    mime_type: String,
    message_id: String,
    thread_id: Option<String>,
    timestamp: i64,
    address: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MapTileKey {
    z: u8,
    x: i32,
    y: i32,
}

struct MapTileUpdate {
    key: MapTileKey,
    image: Option<egui::ColorImage>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SearchFilters {
    address: String,
    thread_id: String,
    message_type: MessageTypeFilter,
    since: String,
    until: String,
}

impl Default for SearchFilters {
    fn default() -> Self {
        Self {
            address: String::new(),
            thread_id: String::new(),
            message_type: MessageTypeFilter::All,
            since: String::new(),
            until: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum MessageTypeFilter {
    All,
    Sms,
    Mms,
    Rcs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum MediaNsfwFilter {
    ShowAll,
    OnlyNsfw,
    HideNsfw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum AppTab {
    Import,
    Search,
    Embeddings,
    Media,
    Contacts,
    Timeline,
    Analytics,
    Assistant,
    Map,
    Logs,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaModel>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct OllamaModel {
    name: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct OllamaEmbedResponse {
    #[serde(default)]
    embedding: Option<Vec<f32>>,
    #[serde(default)]
    data: Option<Vec<OllamaEmbedData>>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct OllamaEmbedData {
    embedding: Vec<f32>,
}

impl eframe::App for SmsArchiveApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        let frame_ms = now.duration_since(self.last_frame).as_secs_f32() * 1000.0;
        self.last_frame = now;
        if self.frame_ms_ema == 0.0 {
            self.frame_ms_ema = frame_ms;
        } else {
            self.frame_ms_ema = (self.frame_ms_ema * 0.9) + (frame_ms * 0.1);
        }

        self.pump_thumbnails(ctx);

        if let Some(results) = self.take_pending_results() {
            self.results = results;
            self.search_in_flight = false;
            self.status = "Search complete".to_string();
            self.selected = None;
            self.selected_attachments.clear();
            if self.results.is_empty() {
                self.page_jump_input.clear();
            } else {
                let current = (self.page_offset / self.page_size) + 1;
                self.page_jump_input = current.to_string();
            }
        }
        if let Some(results) = self.take_pending_thread_results() {
            self.thread_attachments = load_attachments_for_messages(&self.db_path, &results);
            self.thread_results = results;
            self.thread_in_flight = false;
            self.status = "Thread loaded".to_string();
            if let Some(anchor) = self.thread_anchor {
                if let Some(msg) = self.thread_results.iter().find(|m| m.id == anchor).cloned() {
                    self.selected = Some(msg);
                }
            }
            self.thread_scroll_to_anchor = self.thread_anchor.is_some();
        }
        if let Some(hits) = self.take_pending_semantic() {
            self.semantic_hits = hits;
            self.semantic_in_flight = false;
            self.semantic_status = "Semantic search complete".to_string();
        }
        if let Some(stats) = self.take_pending_model_stats() {
            self.model_stats = stats.models;
            self.model_stats_total = stats.total_messages;
            self.model_stats_in_flight = false;
            self.model_stats_status = "Model stats updated".to_string();
        }
        if let Some(snapshot) = self.take_pending_contacts() {
            self.contacts = snapshot.contacts;
            self.contact_name_cache = snapshot.address_map;
            self.contacts_in_flight = false;
        }
        if let Some(detail) = self.take_pending_contact_detail() {
            // Detail loads race when contacts are clicked in quick succession
            // (and saves share this slot) — only apply a result that still
            // matches the current selection.
            if self.selected_contact_id.as_deref() == Some(detail.id.as_str()) {
                self.contact_detail = Some(detail);
                self.contact_status = "Contact updated".to_string();
            }
        }
        if let Some(status) = self.take_pending_contact_status() {
            self.contact_status = status;
        }
        if let Some(groups) = self.take_pending_duplicate_groups() {
            self.duplicate_groups = groups;
            self.duplicates_in_flight = false;
            self.contact_status =
                format!("Found {} duplicate group(s)", self.duplicate_groups.len());
        }
        if let Some(stats) = self.take_pending_timeline() {
            self.timeline_stats = Some(stats);
            self.timeline_in_flight = false;
        }
        if let Some(points) = self.take_pending_map() {
            let count = points.len();
            self.map_points = points;
            self.map_in_flight = false;
            self.map_status = format!("Loaded {} geotagged items", count);
        }
        for update in self.take_pending_map_tiles() {
            self.map_tiles_in_flight.remove(&update.key);
            if let Some(image) = update.image {
                let key = format!(
                    "map_tile_{}_{}_{}",
                    update.key.z, update.key.x, update.key.y
                );
                let texture = ctx.load_texture(key, image, egui::TextureOptions::LINEAR);
                self.map_tiles.insert(update.key, texture);
            }
        }
        if let Some((media, total)) = self.take_pending_media() {
            self.media_results = media;
            self.media_total_count = total;
            self.media_in_flight = false;
        }
        if let Some(hits) = self.take_pending_media_semantic() {
            self.media_semantic_hits = hits;
            self.media_semantic_in_flight = false;
            self.media_semantic_status = "Media semantic search complete".to_string();
        }
        if let Some(status) = self.take_pending_fts_rebuild() {
            self.status = status;
        }
        for update in self.take_pending_ocr() {
            self.ocr_in_progress.remove(&update.attachment_id);
            match update.result {
                Ok(payload) => {
                    apply_ocr_update(&mut self.media_results, &payload, &update.attachment_id);
                    apply_ocr_update(
                        &mut self.selected_attachments,
                        &payload,
                        &update.attachment_id,
                    );
                }
                Err(err) => {
                    self.status = format!("OCR error: {}", err);
                }
            }
        }
        for update in self.take_pending_vision() {
            self.vision_in_progress.remove(&update.attachment_id);
            match update.result {
                Ok(payload) => {
                    apply_vision_update(&mut self.media_results, &payload, &update.attachment_id);
                    apply_vision_update(
                        &mut self.selected_attachments,
                        &payload,
                        &update.attachment_id,
                    );
                }
                Err(err) => {
                    self.status = format!("Vision error: {}", err);
                }
            }
        }
        for update in self.take_pending_nsfw() {
            self.nsfw_in_progress.remove(&update.attachment_id);
            match update.result {
                Ok(payload) => {
                    apply_nsfw_update(&mut self.media_results, &payload, &update.attachment_id);
                    apply_nsfw_update(
                        &mut self.selected_attachments,
                        &payload,
                        &update.attachment_id,
                    );
                }
                Err(err) => {
                    self.status = format!("NSFW error: {}", err);
                }
            }
        }
        if let Some(msg) = self.take_pending_model_action() {
            self.model_action_in_flight = false;
            self.model_stats_status = msg;
            if !self.model_stats_in_flight {
                self.refresh_model_stats();
            }
            let current = self.embed_generation;
            if current != 0 && self.embed_job.is_none() {
                self.embed_generation = 0;
                self.start_embeddings();
            }
        }
        if let Some(models) = self.take_pending_ollama_models() {
            self.ollama_models = models;
            if self.ollama_selected.is_empty() {
                if let Some(first) = self.ollama_models.first() {
                    self.ollama_selected = first.name.clone();
                }
            }
            self.ollama_in_flight = false;
            self.ollama_status = "Ollama models updated".to_string();
        }
        let pending_lines = self.take_pending_ollama_log();
        if !pending_lines.is_empty() {
            self.append_ollama_log(pending_lines);
        }
        // Only worker completion clears the in-flight flag — a pull streams
        // progress lines for minutes, and clearing on the first batch let
        // concurrent pulls/refreshes interleave.
        if self.ollama_job_done.swap(false, Ordering::Relaxed) {
            self.ollama_in_flight = false;
        }
        let embed_lines = self.take_pending_media_embed_status();
        if !embed_lines.is_empty() {
            self.media_embed_status = embed_lines.join("\n");
        }
        for id in self.take_pending_media_embed_done() {
            self.media_embed_in_progress.remove(&id);
        }
        if let Some(rows) = self.take_pending_media_embed_inspect() {
            self.media_embed_inspect_rows = rows;
            self.media_embed_inspect_in_flight = false;
            if self.media_embed_inspect_rows.is_empty() {
                self.media_embed_inspect_status = "No embeddings found for selection".to_string();
            } else {
                self.media_embed_inspect_status = format!(
                    "Embedding inspector: {} row(s)",
                    self.media_embed_inspect_rows.len()
                );
            }
        }
        if let Some(snapshot) = self.take_pending_media_audit() {
            self.media_audit_snapshot = Some(snapshot);
            self.media_audit_in_flight = false;
            self.media_audit_status = "Media audit complete".to_string();
        }
        let backfill_update = self
            .pending_media_backfill
            .lock()
            .ok()
            .and_then(|mut l| l.take());
        if let Some((status, done)) = backfill_update {
            self.media_backfill_status = status;
            if done {
                self.media_backfill_in_flight = false;
                self.load_media_page();
            }
        }
        if let Some(status) = self.take_pending_assistant_model_check() {
            self.assistant_model_check_in_flight = false;
            self.assistant_model_status = status;
        }
        if let Some(status) = self.take_pending_vision_model_check() {
            self.vision_model_check_in_flight = false;
            self.vision_model_status = status;
        }
        if let Some(result) = self.take_pending_assistant() {
            self.assistant_waiting = false;
            if let Ok(mut buf) = self.assistant_stream.lock() {
                buf.clear();
            }
            match result {
                Ok(messages) => {
                    self.assistant.messages = messages;
                }
                Err(err) => {
                    self.status = format!("Assistant error: {}", err);
                }
            }
        }
        // Keep repainting while a streamed answer is arriving so tokens render.
        if self.assistant_waiting {
            ctx.request_repaint();
        }

        if self
            .import_job
            .as_ref()
            .is_some_and(|job| job.handle.is_finished())
        {
            if let Some(job) = self.import_job.take() {
                let total = job.progress.total_bytes.load(Ordering::Relaxed);
                let read = job.progress.bytes_read.load(Ordering::Relaxed);
                let input_count = job.inputs.len();
                match job.handle.join() {
                    Ok(Ok(stats)) => {
                        let elapsed_s = (stats.elapsed_ms as f64 / 1000.0).max(0.001);
                        let msg_rate = stats.messages_seen as f64 / elapsed_s;
                        let mb = stats.bytes_read as f64 / (1024.0 * 1024.0);
                        let file_label = if input_count > 1 {
                            format!(" | {} files", input_count)
                        } else {
                            String::new()
                        };
                        self.import_status = format!(
                            "Import complete: {} inserted ({} seen, {} errors, {} attachments) | {:.1} MB read | {:.0} msg/s | {:.1}s{}",
                            stats.messages_inserted,
                            stats.messages_seen,
                            stats.parse_errors,
                            stats.attachments_written,
                            mb,
                            msg_rate,
                            elapsed_s,
                            file_label
                        );
                        if input_count == 1 {
                            if let Some(xml_path) = job.inputs.first() {
                                if xml_path.exists() {
                                    self.import_contacts_from_xml_path(xml_path.clone());
                                }
                            }
                        }
                        self.start_gps_tagging_after_import();
                        self.maybe_start_clip_after_import();
                    }
                    Ok(Err(err)) => {
                        let msg = err.to_string();
                        if msg.contains("Cancelled") && total > 0 && read >= total {
                            self.import_status = "Import complete (auto-stopped)".to_string();
                            if input_count == 1 {
                                if let Some(xml_path) = job.inputs.first() {
                                    if xml_path.exists() {
                                        self.import_contacts_from_xml_path(xml_path.clone());
                                    }
                                }
                            }
                            self.start_gps_tagging_after_import();
                            self.maybe_start_clip_after_import();
                        } else {
                            self.import_status = format!("Import failed: {}", err);
                        }
                    }
                    Err(_) => {
                        self.import_status = "Import thread panicked".to_string();
                    }
                }
            }
        }
        if self
            .embed_job
            .as_ref()
            .is_some_and(|job| job.handle.is_finished())
        {
            if let Some(job) = self.embed_job.take() {
                match job.handle.join() {
                    Ok(Ok(stats)) => {
                        self.embed_status = format!(
                            "Embeddings complete: {} embedded in {} ms",
                            stats.embedded, stats.elapsed_ms
                        );
                    }
                    Ok(Err(err)) => {
                        self.embed_status = format!("Embedding failed: {}", err);
                    }
                    Err(_) => {
                        self.embed_status = "Embedding thread panicked".to_string();
                    }
                }
            }
        }
        if self
            .clip_job
            .as_ref()
            .is_some_and(|job| job.handle.is_finished())
        {
            if let Some(job) = self.clip_job.take() {
                match job.handle.join() {
                    Ok(Ok(stats)) => {
                        self.clip_status = format!(
                            "CLIP complete: {} tasks, {} frames, {} NSFW updates in {} ms",
                            stats.processed_tasks,
                            stats.embedded_frames,
                            stats.nsfw_updated,
                            stats.elapsed_ms
                        );
                    }
                    Ok(Err(err)) => {
                        self.clip_status = format!("CLIP processing failed: {}", err);
                    }
                    Err(_) => {
                        self.clip_status = "CLIP processing thread panicked".to_string();
                    }
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("SMS Archive");
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.show_perf, "Show perf");
                if self.show_perf {
                    let fps = if self.frame_ms_ema > 0.0 {
                        1000.0 / self.frame_ms_ema
                    } else {
                        0.0
                    };
                    ui.label(format!(
                        "frame {:.2} ms ({:.1} fps)",
                        self.frame_ms_ema, fps
                    ));
                }
            });
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.active_tab, AppTab::Import, "Import");
                ui.selectable_value(&mut self.active_tab, AppTab::Search, "Search");
                ui.selectable_value(&mut self.active_tab, AppTab::Embeddings, "Embeddings");
                ui.selectable_value(&mut self.active_tab, AppTab::Media, "Media");
                ui.selectable_value(&mut self.active_tab, AppTab::Contacts, "Contacts");
                ui.selectable_value(&mut self.active_tab, AppTab::Timeline, "Timeline");
                ui.selectable_value(&mut self.active_tab, AppTab::Analytics, "Analytics");
                ui.selectable_value(&mut self.active_tab, AppTab::Assistant, "Assistant");
                ui.selectable_value(&mut self.active_tab, AppTab::Map, "Map");
                ui.selectable_value(&mut self.active_tab, AppTab::Logs, "Logs");
            });
            ui.add_space(8.0);

            if matches!(self.active_tab, AppTab::Import) {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.group(|ui| {
                            ui.label("Import");
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.label("XML:")
                                    .on_hover_text("Select the SMS XML export to ingest.");
                                ui.text_edit_singleline(&mut self.import_input);
                                if ui.button("Browse").clicked() {
                                    if let Some(path) = FileDialog::new()
                                        .add_filter("XML", &["xml"])
                                        .pick_file()
                                    {
                                        self.import_input = path.display().to_string();
                                    }
                                }
                            });
                            ui.label("Tip: separate multiple XML files with ';' to queue them.");
                            ui.horizontal(|ui| {
                                ui.label("DB:")
                                    .on_hover_text("Choose an existing database file, or pick a folder to create a new one.");
                                ui.text_edit_singleline(&mut self.db_path);
                                if ui.button("Browse DB").clicked() {
                                    if let Some(path) = FileDialog::new()
                                        .add_filter("SQLite DB", &["db", "sqlite", "sqlite3"])
                                        .pick_file()
                                    {
                                        self.db_folder = path.parent().map(PathBuf::from);
                                        self.db_path = path.display().to_string();
                                    }
                                }
                                if ui.button("Choose folder").clicked() {
                                    if let Some(folder) = FileDialog::new().pick_folder() {
                                        self.db_folder = Some(folder.clone());
                                        self.db_path = folder.display().to_string();
                                    }
                                }
                            });
                            if self.db_folder.is_some() {
                                ui.horizontal(|ui| {
                                    ui.label("New DB name:")
                                        .on_hover_text("Create a new database in the selected folder.");
                                    ui.text_edit_singleline(&mut self.new_db_name);
                                    if ui.button("Create new DB").clicked() {
                                        self.create_new_db();
                                    }
                                });
                            }
                            ui.horizontal(|ui| {
                                if ui.button("Open DB").clicked() {
                                    self.open_db();
                                }
                            });
                            self.refresh_checkpoint_if_needed();
                            if let Some(info) = &self.checkpoint_info {
                                ui.label(format!(
                                    "Checkpoint: offset {} | imported {} | started {}",
                                    info.last_committed_offset, info.messages_imported, info.started_at
                                ));
                                ui.checkbox(&mut self.resume_from_checkpoint, "Resume from checkpoint");
                                if ui.button("Restart (delete checkpoint)").clicked() {
                                    let _ = fs::remove_file(checkpoint_path_for(&self.db_path));
                                    self.checkpoint_info = None;
                                    self.resume_from_checkpoint = false;
                                    self.import_status = "Checkpoint removed".to_string();
                                }
                            }
                            let mut paused = false;
                            let mut cancelled = false;
                            if let Some(job) = &self.import_job {
                                paused = job.progress.paused.load(Ordering::Relaxed);
                                cancelled = job.progress.cancelled.load(Ordering::Relaxed);
                            }
                            if paused {
                                let current_pause_duration = self.import_pause_start
                                    .map(|s| s.elapsed())
                                    .unwrap_or(Duration::ZERO);
                                ui.colored_label(
                                    egui::Color32::YELLOW,
                                    format!("⚠ IMPORT PAUSED for {:?}", current_pause_duration)
                                );
                            } else if cancelled {
                                ui.colored_label(egui::Color32::LIGHT_RED, "CANCELLING...");
                            }
                            ui.horizontal(|ui| {
                                if ui
                                    .add_enabled(self.import_job.is_none(), egui::Button::new("Start Import"))
                                    .clicked()
                                {
                                    self.start_import();
                                    self.import_last_offset = 0;
                                    self.import_last_update = Instant::now();
                                    self.import_total_paused = Duration::ZERO;
                                    self.import_pause_start = None;
                                }
                                if let Some(job) = &self.import_job {
                                    let pause_label = if paused { "▶ Resume" } else { "⏸ Pause" };
                                    if ui.add_enabled(!cancelled, egui::Button::new(pause_label)).clicked() {
                                        let new_paused = !paused;
                                        job.progress.paused.store(new_paused, Ordering::Relaxed);

                                        // Track pause duration
                                        if new_paused {
                                            // Starting pause
                                            self.import_pause_start = Some(Instant::now());
                                        } else {
                                            // Resuming - accumulate pause time
                                            if let Some(start) = self.import_pause_start {
                                                self.import_total_paused += start.elapsed();
                                            }
                                            self.import_pause_start = None;
                                            self.import_last_update = Instant::now(); // Reset stuck detection
                                        }
                                    }
                                    if ui.add_enabled(!cancelled, egui::Button::new("Cancel")).clicked() {
                                        job.progress.cancelled.store(true, Ordering::Relaxed);
                                    }
                                    if cancelled && ui.button("Force reset").clicked() {
                                        self.force_reset_import();
                                    }
                                }
                            });

                            if let Some(job) = &self.import_job {
                                let total = job.progress.total_bytes.load(Ordering::Relaxed);
                                let read = job.progress.bytes_read.load(Ordering::Relaxed);
                                let seen = job.progress.messages_seen.load(Ordering::Relaxed);
                                let inserted = job.progress.messages_inserted.load(Ordering::Relaxed);
                                let errors = job.progress.parse_errors.load(Ordering::Relaxed);
                                let skipped_attachments =
                                    job.progress.attachments_skipped.load(Ordering::Relaxed);
                                let paused = job.progress.paused.load(Ordering::Relaxed);
                                let cancelled = job.progress.cancelled.load(Ordering::Relaxed);

                                // Stuck detection - check if import is making progress
                                if !paused && !cancelled {
                                    if read != self.import_last_offset {
                                        self.import_last_offset = read;
                                        self.import_last_update = Instant::now();
                                    } else if self.import_last_update.elapsed() > self.import_stuck_threshold {
                                        self.show_stuck_dialog = true;
                                    }
                                }
                                let pct = if total > 0 {
                                    (read as f32 / total as f32).min(1.0)
                                } else {
                                    0.0
                                };
                                let elapsed = job.started_at.elapsed().as_secs_f32().max(0.001);
                                let msg_rate = (seen as f32 / elapsed).max(0.0);
                                let skipped = seen.saturating_sub(inserted + errors);
                                let state = if cancelled {
                                    "CANCELLING"
                                } else if paused {
                                    "PAUSED"
                                } else {
                                    ""
                                };
                                if total > 0
                                    && read >= total
                                    && !paused
                                    && !cancelled
                                    && self.import_last_update.elapsed()
                                        > Duration::from_secs(8)
                                {
                                    job.progress.cancelled.store(true, Ordering::Relaxed);
                                    job.progress
                                        .skip_current_file
                                        .store(true, Ordering::Relaxed);
                                    self.import_status =
                                        "Auto-stopping import after 100% (finalizing)".to_string();
                                }
                                ui.add(
                                    egui::ProgressBar::new(pct).text(format!(
                                        "{:.1}% | {} seen | {} inserted | {} errors | {} skipped | {:.0} msg/s {}",
                                        pct * 100.0,
                                        seen,
                                        inserted,
                                        errors,
                                        skipped,
                                        msg_rate,
                                        state
                                    )),
                                );
                                if let Ok(lock) = job.progress.current_file.lock() {
                                    if let Some(path) = lock.as_ref() {
                                        ui.label(format!("Current file: {}", path));
                                    }
                                }
                                if skipped_attachments > 0 {
                                    ui.label(format!(
                                        "Skipped attachments: {}",
                                        skipped_attachments
                                    ));
                                }

                                // Show total paused time if any
                                if self.import_total_paused > Duration::ZERO {
                                    ui.label(format!("Total paused time: {:?}", self.import_total_paused));
                                }

                                ui.separator();
                                ui.group(|ui| {
                                    ui.label("Recent import errors");
                                    ui.horizontal(|ui| {
                                        if ui.button("Clear errors").clicked() {
                                            if let Ok(mut lock) = job.progress.error_samples.lock() {
                                                lock.clear();
                                            }
                                        }
                                    });
                                    let errors = job
                                        .progress
                                        .error_samples
                                        .lock()
                                        .map(|lock| lock.clone())
                                        .unwrap_or_default();
                                    if errors.is_empty() {
                                        ui.label("No parse errors recorded.");
                                    } else {
                                        egui::ScrollArea::vertical()
                                            .max_height(120.0)
                                            .show(ui, |ui| {
                                                for entry in errors {
                                                    ui.label(entry);
                                                }
                                            });
                                    }
                                });
                                ui.group(|ui| {
                                    ui.label("Skipped items");
                                    ui.horizontal(|ui| {
                                        if ui.button("Clear skipped").clicked() {
                                            job.progress
                                                .attachments_skipped
                                                .store(0, Ordering::Relaxed);
                                            if let Ok(mut lock) = job.progress.skipped_samples.lock() {
                                                lock.clear();
                                            }
                                        }
                                    });
                                    let skipped = job
                                        .progress
                                        .skipped_samples
                                        .lock()
                                        .map(|lock| lock.clone())
                                        .unwrap_or_default();
                                    if skipped.is_empty() {
                                        ui.label("No skipped items recorded.");
                                    } else {
                                        egui::ScrollArea::vertical()
                                            .max_height(120.0)
                                            .show(ui, |ui| {
                                                for entry in skipped {
                                                    ui.label(entry);
                                                }
                                            });
                                    }
                                    // #todo: add export-to-file for skipped items.
                                });

                                ctx.request_repaint_after(Duration::from_millis(200));
                            }
                            if !self.import_status.is_empty() {
                                ui.label(&self.import_status);
                            }
                        });
                    });
            }

            if matches!(self.active_tab, AppTab::Embeddings) {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.group(|ui| {
                ui.label("Embeddings");
                ui.add_space(4.0);
                ui.label("Embeddings turn text into vectors for semantic search. Choose either ONNX (local) or Ollama (server).");
                ui.horizontal(|ui| {
                    ui.label("Database:")
                        .on_hover_text("Database to write embeddings into.");
                    ui.text_edit_singleline(&mut self.db_path);
                    if ui.button("Browse DB").clicked() {
                        if let Some(path) = FileDialog::new()
                            .add_filter("SQLite DB", &["db", "sqlite", "sqlite3"])
                            .pick_file()
                        {
                            self.db_folder = path.parent().map(PathBuf::from);
                            self.db_path = path.display().to_string();
                        }
                    }
                    if ui.button("Open").clicked() {
                        self.open_db();
                    }
                });
                ui.separator();
                ui.label("ONNX (local embeddings)");
                ui.horizontal(|ui| {
                    ui.label("Model path:")
                        .on_hover_text("ONNX model file for embeddings.");
                    ui.text_edit_singleline(&mut self.embed_model_path);
                    if ui.button("Browse").clicked() {
                        if let Some(path) = FileDialog::new().pick_file() {
                            self.embed_model_path = path.display().to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Tokenizer:")
                        .on_hover_text("Tokenizer JSON for the ONNX embedding model.");
                    ui.text_edit_singleline(&mut self.embed_tokenizer_path);
                    if ui.button("Browse").clicked() {
                        if let Some(path) = FileDialog::new()
                            .add_filter("JSON", &["json"])
                            .pick_file()
                        {
                            self.embed_tokenizer_path = path.display().to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Name:")
                        .on_hover_text("Logical model name stored in the DB.");
                    ui.text_edit_singleline(&mut self.embed_model_name);
                    ui.label("Version:")
                        .on_hover_text("Model version or tag (used for tracking).");
                    ui.text_edit_singleline(&mut self.embed_model_version);
                });
                ui.horizontal(|ui| {
                    ui.label("Dims:")
                        .on_hover_text("Embedding vector size. Use model output dimension.");
                    ui.add(egui::DragValue::new(&mut self.embed_dimensions).clamp_range(8..=4096));
                    egui::ComboBox::from_id_source("dims_presets")
                        .selected_text("Dims presets")
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.embed_dimensions, 256, "256");
                            ui.selectable_value(&mut self.embed_dimensions, 384, "384");
                            ui.selectable_value(&mut self.embed_dimensions, 512, "512");
                            ui.selectable_value(&mut self.embed_dimensions, 768, "768");
                            ui.selectable_value(&mut self.embed_dimensions, 1024, "1024");
                        });
                    ui.label("Batch:")
                        .on_hover_text("Number of messages embedded per batch.");
                    ui.add(egui::DragValue::new(&mut self.embed_batch_size).clamp_range(1..=4096));
                });
                ui.horizontal(|ui| {
                    ui.label("Max len:")
                        .on_hover_text("Tokenizer max sequence length.");
                    ui.add(egui::DragValue::new(&mut self.embed_max_length).clamp_range(8..=4096));
                    egui::ComboBox::from_id_source("maxlen_presets")
                        .selected_text("Len presets")
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.embed_max_length, 128, "128");
                            ui.selectable_value(&mut self.embed_max_length, 256, "256");
                            ui.selectable_value(&mut self.embed_max_length, 384, "384");
                            ui.selectable_value(&mut self.embed_max_length, 512, "512");
                            ui.selectable_value(&mut self.embed_max_length, 1024, "1024");
                        });
                    ui.checkbox(&mut self.embed_normalize, "L2 normalize")
                        .on_hover_text("Normalize embeddings to unit length.");
                });
                ui.horizontal(|ui| {
                    ui.label("Device:")
                        .on_hover_text("Use GPU if ONNX Runtime CUDA is available.");
                    egui::ComboBox::from_id_source("embed_device")
                        .selected_text(match self.embed_device {
                            DevicePreference::Cpu => "CPU",
                            DevicePreference::Gpu => "GPU (CUDA)",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.embed_device, DevicePreference::Cpu, "CPU");
                            ui.selectable_value(
                                &mut self.embed_device,
                                DevicePreference::Gpu,
                                "GPU (CUDA)",
                            );
                        });
                    if ui.button("Test local model").clicked() {
                        let model_path = self.embed_model_path.trim();
                        let tokenizer_path = self.embed_tokenizer_path.trim();
                        if model_path.is_empty() || tokenizer_path.is_empty() {
                            self.embed_status =
                                "Set local model + tokenizer before testing".to_string();
                        } else {
                            let result: sms_errors::Result<usize> = (|| {
                                let mut service = EmbeddingService::new(EmbeddingConfig {
                                    model_path: Some(PathBuf::from(model_path)),
                                    tokenizer_path: Some(PathBuf::from(tokenizer_path)),
                                    model_name: self.embed_model_name.trim().to_string(),
                                    model_version: self.embed_model_version.trim().to_string(),
                                    dimensions: self.embed_dimensions,
                                    device: self.embed_device,
                                    max_length: self.embed_max_length,
                                    normalize: self.embed_normalize,
                                    input_ids_name: None,
                                    attention_mask_name: None,
                                    token_type_ids_name: None,
                                    output_name: None,
                                })?;
                                let vec = service.embed("test embedding")?;
                                Ok(vec.len())
                            })();
                            self.embed_status = match result {
                                Ok(size) => format!("Local model OK (dims={})", size),
                                Err(err) => format!("Local model failed: {}", err),
                            };
                        }
                    }
                });
                ui.separator();
                ui.label("Ollama (local server embeddings)");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.use_ollama, "Use Ollama")
                        .on_hover_text("Use local Ollama embeddings instead of ONNX.");
                    ui.label("Base URL:")
                        .on_hover_text("Ollama server URL (default http://localhost:11434).");
                    ui.text_edit_singleline(&mut self.ollama_base_url);
                    if ui.button("Refresh models").clicked() {
                        self.refresh_ollama_models();
                    }
                });
                if !self.ollama_models_source.is_empty() {
                    ui.label(format!("Models source: {}", self.ollama_models_source));
                }
                if self.use_ollama {
                    ui.horizontal(|ui| {
                        ui.label("Model:")
                            .on_hover_text("Select an installed Ollama model.");
                        egui::ComboBox::from_id_source("ollama_models")
                            .selected_text(
                                if self.ollama_selected.is_empty() {
                                    "Select model"
                                } else {
                                    &self.ollama_selected
                                },
                            )
                            .show_ui(ui, |ui| {
                                for model in &self.ollama_models {
                                    ui.selectable_value(
                                        &mut self.ollama_selected,
                                        model.name.clone(),
                                        &model.name,
                                    );
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Pull model:")
                            .on_hover_text("Download a model via Ollama if missing.");
                        ui.text_edit_singleline(&mut self.ollama_pull_name);
                        if ui.button("Pull").clicked() {
                            self.pull_ollama_model();
                        }
                    });
                    if !self.ollama_log.is_empty() {
                        let mut text = self.ollama_log.join("\n");
                        ui.add(
                            egui::TextEdit::multiline(&mut text)
                                .desired_rows(4)
                                .interactive(false),
                        );
                    } else if !self.ollama_status.is_empty() {
                        ui.label(&self.ollama_status);
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("Start Embeddings").clicked() {
                        self.start_embeddings();
                    }
                    if let Some(job) = &self.embed_job {
                        if ui.button("Cancel").clicked() {
                            job.progress.cancelled.store(true, Ordering::Relaxed);
                        }
                    }
                });
                if let Some(job) = &self.embed_job {
                    let total = job.progress.total.load(Ordering::Relaxed);
                    let done = job.progress.done.load(Ordering::Relaxed);
                    let pct = if total > 0 {
                        (done as f32 / total as f32).min(1.0)
                    } else {
                        0.0
                    };
                    let elapsed = job.started_at.elapsed().as_secs_f32().max(0.001);
                    let rate = (done as f32 / elapsed).max(0.0);
                    ui.add(
                        egui::ProgressBar::new(pct).text(format!(
                            "{:.1}% | {} / {} | {:.0} msg/s",
                            pct * 100.0,
                            done,
                            total,
                            rate
                        )),
                    );
                    ctx.request_repaint_after(Duration::from_millis(200));
                }
                if !self.embed_status.is_empty() {
                    ui.label(&self.embed_status);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Refresh model stats").clicked() {
                        self.refresh_model_stats();
                    }
                    if self.model_stats_in_flight {
                        ui.label("Loading...");
                    }
                });
                if !self.model_stats_status.is_empty() {
                    ui.label(&self.model_stats_status);
                }
                if self.model_stats_total > 0 {
                    ui.label(format!(
                        "Embeddable messages: {}",
                        self.model_stats_total
                    ));
                }
                if !self.model_stats.is_empty() {
                    for model in &self.model_stats {
                        let pct = if self.model_stats_total > 0 {
                            (model.embedding_count as f32 / self.model_stats_total as f32) * 100.0
                        } else {
                            0.0
                        };
                        let dims = model
                            .dims
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        let max_len = model
                            .max_length
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        let norm = match model.normalize {
                            Some(v) => {
                                if v != 0 {
                                    "Y"
                                } else {
                                    "N"
                                }
                            }
                            None => "?",
                        };
                        let sha = model
                            .sha256
                            .as_deref()
                            .map(|s| s.chars().take(8).collect::<String>())
                            .unwrap_or_else(|| "none".to_string());
                        let id_short = model
                            .id
                            .chars()
                            .take(8)
                            .collect::<String>();
                        let tokenizer_short = model
                            .tokenizer_path
                            .as_ref()
                            .and_then(|path| Path::new(path).file_name())
                            .and_then(|name| name.to_str())
                            .unwrap_or("?");
                        let selected = self
                            .selected_model_id
                            .as_ref()
                            .map(|id| id == &model.id)
                            .unwrap_or(false);
                        let label = format!(
                            "{} {} | {} embeddings ({:.1}%) | dims {} | max {} | norm {} | tok {} | sha {} | id {} | created {}",
                            model.name,
                            model.version,
                            model.embedding_count,
                            pct,
                            dims,
                            max_len,
                            norm,
                            tokenizer_short,
                            sha,
                            id_short,
                            model.created_at
                        );
                        let mut details = Vec::new();
                        if let Some(path) = &model.tokenizer_path {
                            details.push(format!("tokenizer: {}", path));
                        }
                        if let Some(name) = &model.input_ids_name {
                            details.push(format!("input_ids: {}", name));
                        }
                        if let Some(name) = &model.attention_mask_name {
                            details.push(format!("attention_mask: {}", name));
                        }
                        if let Some(name) = &model.token_type_ids_name {
                            details.push(format!("token_type_ids: {}", name));
                        }
                        if let Some(name) = &model.output_name {
                            details.push(format!("output: {}", name));
                        }
                        let mut row = ui.selectable_label(selected, label);
                        if !details.is_empty() {
                            row = row.on_hover_text(details.join("\n"));
                        }
                        if row.clicked() {
                            self.selected_model_id = Some(model.id.clone());
                            self.embed_model_name = model.name.clone();
                            self.embed_model_version = model.version.clone();
                            if let Some(dims) = model.dims {
                                self.embed_dimensions = dims as usize;
                            }
                            if let Some(max_len) = model.max_length {
                                self.embed_max_length = max_len as usize;
                            }
                            if let Some(norm) = model.normalize {
                                self.embed_normalize = norm != 0;
                            }
                            if let Some(tokenizer) = &model.tokenizer_path {
                                self.embed_tokenizer_path = tokenizer.clone();
                            }
                        }
                    }
                    ui.horizontal(|ui| {
                        let has_selection = self.selected_model_id.is_some();
                        if ui
                            .add_enabled(has_selection && !self.model_action_in_flight, egui::Button::new("Purge embeddings"))
                            .clicked()
                        {
                            self.start_model_purge();
                        }
                        if ui
                            .add_enabled(has_selection && !self.model_action_in_flight, egui::Button::new("Delete model"))
                            .clicked()
                        {
                            self.start_model_delete();
                        }
                        if ui
                            .add_enabled(!self.model_action_in_flight, egui::Button::new("Re-embed"))
                            .clicked()
                        {
                            self.start_reembed();
                        }
                    });
                }
            });
        });
            }

            if matches!(self.active_tab, AppTab::Search) {
                ui.push_id("search_tab", |ui| {
                    egui::ScrollArea::both()
                        .id_source("timeline_main_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                        ui.horizontal(|ui| {
                    ui.label("Database:")
                        .on_hover_text("Database file to search against.");
                    ui.text_edit_singleline(&mut self.db_path);
                    if ui.button("Browse DB").clicked() {
                        if let Some(path) = FileDialog::new()
                            .add_filter("SQLite DB", &["db", "sqlite", "sqlite3"])
                            .pick_file()
                        {
                            self.db_folder = path.parent().map(PathBuf::from);
                            self.db_path = path.display().to_string();
                        }
                    }
                    if ui.button("Open").clicked() {
                        self.open_db();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Search:")
                        .on_hover_text("Full-text search query.");
                    ui.text_edit_singleline(&mut self.search_query);
                    if ui.button("Search").clicked() {
                        self.run_search();
                    }
                    if ui.button("Rebuild FTS").clicked() {
                        self.rebuild_fts_index();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Name/Sender:")
                        .on_hover_text("Filter by sender/recipient name or address.");
                    ui.text_edit_singleline(&mut self.search_filters.address);
                    ui.label("Thread:")
                        .on_hover_text("Filter by thread id.");
                    ui.text_edit_singleline(&mut self.search_filters.thread_id);
                    ui.label("Type:")
                        .on_hover_text("Filter by message type.");
                    egui::ComboBox::from_id_source("type_filter")
                        .selected_text(self.search_filters.message_type.label())
                        .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.search_filters.message_type,
                            MessageTypeFilter::All,
                            "All",
                        );
                        ui.selectable_value(
                            &mut self.search_filters.message_type,
                            MessageTypeFilter::Sms,
                            "SMS",
                        );
                        ui.selectable_value(
                            &mut self.search_filters.message_type,
                            MessageTypeFilter::Mms,
                            "MMS",
                        );
                        ui.selectable_value(
                            &mut self.search_filters.message_type,
                            MessageTypeFilter::Rcs,
                            "RCS",
                        );
                    });
            });

                ui.horizontal(|ui| {
                    ui.label("Since:").on_hover_text(
                        "Only include messages on/after this date. Accepts YYYY-MM-DD or epoch ms.",
                    );
                    ui.text_edit_singleline(&mut self.search_filters.since);
                    ui.label("Until:").on_hover_text(
                        "Only include messages on/before this date. Accepts YYYY-MM-DD or epoch ms.",
                    );
                    ui.text_edit_singleline(&mut self.search_filters.until);
                    ui.label("Thread limit:")
                        .on_hover_text("Maximum messages to load for full thread view.");
                    ui.add(egui::DragValue::new(&mut self.thread_limit).clamp_range(0..=10000));
                    ui.label("Context window:")
                        .on_hover_text("Messages to load before/after when jumping to a specific message (±N).");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.context_window_size)
                            .desired_width(60.0)
                    );
                    if ui.button("Clear filters").clicked() {
                        self.search_filters = SearchFilters::default();
                    }
                });

                ui.group(|ui| {
                    ui.label("Semantic Search");
                    ui.horizontal(|ui| {
                        ui.label("Query:")
                            .on_hover_text("Semantic query (embeddings).");
                        ui.text_edit_singleline(&mut self.semantic_query);
                        ui.label("Max hits:")
                            .on_hover_text("How many semantic hits to return (0 = 5000 cap).");
                        ui.add(egui::DragValue::new(&mut self.semantic_limit).clamp_range(0..=5000));
                        if ui.button("Search").clicked() {
                            self.run_semantic_search();
                        }
                    });
                if self.semantic_in_flight {
                    ui.label("Searching...");
                }
                if !self.semantic_status.is_empty() {
                    ui.label(&self.semantic_status);
                }
                if !self.semantic_hits.is_empty() {
                    ui.label(format!("Hits: {}", self.semantic_hits.len()));
                    let selected_id = self.selected.as_ref().map(|m| m.id);
                    let mut select_hit: Option<Message> = None;
                    egui::ScrollArea::vertical()
                        .max_height(140.0)
                        .show(ui, |ui| {
                            for hit in self.semantic_hits.iter().take(50) {
                                let label = format!(
                                    "{:.4} | {} | {} | {}",
                                    hit.score,
                                    format_timestamp(hit.message.timestamp),
                                    self.sender_label(&hit.message),
                                    summarize_body(&hit.message.body, 80)
                                );
                                let is_selected = selected_id
                                    .map(|id| id == hit.message.id)
                                    .unwrap_or(false);
                                ui.push_id(hit.message.id, |ui| {
                                    if ui.selectable_label(is_selected, label).clicked() {
                                        select_hit = Some(hit.message.clone());
                                    }
                                });
                            }
                        });
                    if let Some(msg) = select_hit {
                        self.selected = Some(msg.clone());
                        self.selected_attachments =
                            load_attachments(&self.db_path, &msg.id);
                    }
                }
            });

                ui.label(format!("Messages: {}", self.message_count));
                ui.label(format!("Status: {}", self.status));
                if !self.thread_results.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label("Thread view active");
                        if ui.button("Back to search results").clicked() {
                            self.clear_thread_view();
                        }
                    });
                } else if self.thread_in_flight {
                    ui.label("Loading thread...");
                }

                ui.separator();

                // Search limit controls
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.search_unlimited, "Unlimited results");
                    if !self.search_unlimited {
                        ui.label("Max results:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.search_max_results)
                                .desired_width(60.0)
                        )
                        .on_hover_text("Maximum number of results to return");
                    }
                });

                // Enhanced pagination controls
                ui.horizontal(|ui| {
                    ui.label("Page size:")
                        .on_hover_text("Number of messages per page.");
                    ui.add(egui::DragValue::new(&mut self.page_size).clamp_range(10..=500));

                    // `self.results` holds only the CURRENT page, so the true
                    // total is unknown without a COUNT query. The old code
                    // derived "total pages" from this page's row count, which
                    // permanently disabled Next whenever a page came back
                    // full. A full page now means "there may be more".
                    let shown = self.results.len();
                    let page_size = self.page_size.max(1);
                    let current_page = (self.page_offset / page_size) + 1;
                    let may_have_more = shown == page_size;

                    ui.separator();

                    if ui.button("◀ Prev").clicked() && self.page_offset > 0 {
                        self.page_offset = self.page_offset.saturating_sub(page_size);
                        self.run_search();
                        self.page_jump_input = (self.page_offset / page_size + 1).to_string();
                    }

                    ui.label(format!("Page {} ({} shown)", current_page, shown));

                    // Jump to page
                    if self.page_jump_input.is_empty() {
                        self.page_jump_input = current_page.to_string();
                    }
                    ui.label("Jump:");
                    let page_input = ui.add(
                        egui::TextEdit::singleline(&mut self.page_jump_input)
                            .desired_width(40.0)
                    );
                    if page_input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if let Ok(page) = self.page_jump_input.trim().parse::<usize>() {
                            if page > 0 {
                                self.page_offset = (page - 1) * page_size;
                                self.run_search();
                                self.page_jump_input = page.to_string();
                            }
                        }
                    }

                    if ui.button("Next ▶").clicked() && may_have_more {
                        self.page_offset = self.page_offset.saturating_add(page_size);
                        self.run_search();
                        self.page_jump_input = (self.page_offset / page_size + 1).to_string();
                    }

                    if self.search_in_flight {
                        ui.spinner();
                        ui.label("Searching...");
                    }
                });

                if !self.thread_results.is_empty() {
                    ui.add_space(4.0);
                    ui.group(|ui| {
                        ui.label("Thread View");
                        let messages = self.thread_results.clone();
                        let attachments_map = self.thread_attachments.clone();
                        let selected_id = self.selected.as_ref().map(|m| m.id);
                        let anchor_id = self.thread_anchor;
                        let mut select_msg: Option<Message> = None;
                        let mut did_scroll = false;
                        ui.push_id("thread_view_scroll", |ui| {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                for msg in messages {
                                    let is_self = self.is_self_message(&msg);
                                    let sender = self.sender_label(&msg);
                                    let ts = format_timestamp(msg.timestamp);
                                    let bubble_max = ui.available_width() * 0.70;
                                    let is_anchor =
                                        anchor_id.map(|id| id == msg.id).unwrap_or(false);
                                    let bubble_fill = if is_self {
                                        egui::Color32::from_rgb(199, 239, 207)
                                    } else {
                                        egui::Color32::from_rgb(235, 235, 245)
                                    };
                                    let bubble_stroke = if is_anchor {
                                        egui::Stroke::new(1.5, egui::Color32::from_rgb(0, 120, 215))
                                    } else {
                                        egui::Stroke::NONE
                                    };
                                    let layout = if is_self {
                                        egui::Layout::right_to_left(egui::Align::Min)
                                    } else {
                                        egui::Layout::left_to_right(egui::Align::Min)
                                    };
                                    ui.with_layout(layout, |ui| {
                                        ui.add_space(4.0);
                                        let response = egui::Frame::none()
                                            .fill(bubble_fill)
                                            .rounding(egui::Rounding::same(10.0))
                                            .stroke(bubble_stroke)
                                            .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                                            .show(ui, |ui| {
                                                ui.set_max_width(bubble_max);
                                                ui.label(
                                                    egui::RichText::new(format!("{} · {}", sender, ts))
                                                        .small()
                                                        .color(egui::Color32::DARK_GRAY),
                                                );
                                                let atts = attachments_map.get(&msg.id).cloned();
                                                let has_amr = atts
                                                    .as_ref()
                                                    .map(|items| items.iter().any(is_amr_attachment))
                                                    .unwrap_or(false);
                                                let body_text = if msg.body.trim().is_empty() && has_amr {
                                                    "[audio recording]".to_string()
                                                } else if msg.body.trim_start().starts_with("<smil") {
                                                    // Strip SMIL XML from legacy imported messages
                                                    String::new()
                                                } else {
                                                    msg.body.clone()
                                                };
                                                if !body_text.is_empty() {
                                                    ui.add(egui::Label::new(&body_text).wrap(true));
                                                }
                                                if let Some(atts) = atts {
                                                    for att in atts {
                                                        let file_path =
                                                            self.resolve_media_path(&att.file_path);
                                                        let thumb_path = att
                                                            .thumbnail_path
                                                            .as_ref()
                                                            .and_then(|rel| self.resolve_media_path(rel))
                                                            .filter(|p| p.exists());
                                                        if let Some(path) = file_path.as_ref() {
                                                            ui.horizontal(|ui| {
                                                                ui.label(&att.mime_type);
                                                                if ui.button("Open").clicked() {
                                                                    open_file(path);
                                                                }
                                                            });
                                                        }
                                                        if let Some(texture) = self.thumbnail_for(
                                                            file_path.as_ref(),
                                                            thumb_path.as_ref(),
                                                            &att.mime_type,
                                                        ) {
                                                            let size = texture.size_vec2();
                                                            let max_edge = 160.0;
                                                            let scale =
                                                                (max_edge / size.x).min(max_edge / size.y).min(1.0);
                                                            let draw_size =
                                                                egui::vec2(size.x * scale, size.y * scale);
                                                            ui.add(egui::Image::new(SizedTexture::new(
                                                                texture.id(),
                                                                draw_size,
                                                            )));
                                                        } else {
                                                            thumb_placeholder(ui, 160.0);
                                                        }
                                                    }
                                                }
                                                if let Some(id) = selected_id {
                                                    if id == msg.id {
                                                        ui.separator();
                                                        ui.label(
                                                            egui::RichText::new("Selected")
                                                                .small()
                                                                .color(egui::Color32::DARK_GRAY),
                                                        );
                                                    }
                                                }
                                            });
                                        if is_anchor && self.thread_scroll_to_anchor && !did_scroll {
                                            ui.scroll_to_rect(
                                                response.response.rect,
                                                Some(egui::Align::Center),
                                            );
                                            did_scroll = true;
                                        }
                                        if response.response.clicked() {
                                            select_msg = Some(msg.clone());
                                        }
                                    });
                                    ui.add_space(6.0);
                                }
                                });
                            });
                        if did_scroll {
                            self.thread_scroll_to_anchor = false;
                        }
                        if let Some(msg) = select_msg {
                            self.selected = Some(msg.clone());
                            self.selected_attachments =
                                load_attachments(&self.db_path, &msg.id);
                        }
                    });
                } else {
                    ui.columns(2, |columns| {
                        let list = self.results.clone();
                        let selected_id = self.selected.as_ref().map(|m| m.id);
                        let mut select_msg: Option<Message> = None;
                        columns[0].heading(format!("Results: {}", list.len()));
                        columns[0].push_id("search_results", |ui| {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show_rows(ui, 22.0, list.len(), |ui, range| {
                                for idx in range {
                                    if let Some(msg) = list.get(idx) {
                                        let sender = self.sender_label(msg);
                                        let body_clean = if msg.body.trim_start().starts_with("<smil") {
                                            "[MMS media]"
                                        } else {
                                            &msg.body
                                        };
                                        let snippet = summarize_body(body_clean, 48);
                                        let label = format!(
                                            "{} | {} | {}",
                                            format_timestamp(msg.timestamp),
                                            sender,
                                            snippet
                                        );
                                        let is_selected =
                                            selected_id.map(|id| id == msg.id).unwrap_or(false);
                                        ui.push_id(msg.id, |ui| {
                                            ui.horizontal(|ui| {
                                                if ui.selectable_label(is_selected, label).clicked() {
                                                    select_msg = Some(msg.clone());
                                                }
                                                if ui.small_button("Thread").clicked() {
                                                    let thread = msg.thread_id.as_ref();
                                                    let addr = Some(&msg.address);
                                                    self.open_thread_from_parts(thread, addr, Some(msg.id));
                                                }
                                            });
                                        });
                                    }
                                }
                                });
                        });
                        if let Some(msg) = select_msg {
                            self.selected = Some(msg.clone());
                            self.selected_attachments =
                                load_attachments(&self.db_path, &msg.id);
                        }

                        columns[1].heading("Details");
                        if let Some(msg) = self.selected.clone() {
                            let thread_id = msg.thread_id.clone();
                            let address = msg.address.clone();
                            columns[1].label(format!("Time: {}", format_timestamp(msg.timestamp)));
                            columns[1].label(format!("Sender: {}", self.sender_label(&msg)));
                            columns[1].label(format!("Type: {:?}", msg.message_type));
                            if let Some(thread_id) = thread_id.as_ref() {
                                columns[1].label(format!("Thread: {}", thread_id));
                            }
                            columns[1].horizontal(|ui| {
                                if ui.button("Open thread").clicked() {
                                    let thread = thread_id.as_ref();
                                    let addr = Some(&address);
                                    self.open_thread_from_parts(thread, addr, Some(msg.id));
                                }
                                if ui.button("Open contact").clicked() {
                                    self.open_contact_for_address(&address);
                                }
                            });
                            columns[1].separator();
                            let has_amr = self
                                .selected_attachments
                                .iter()
                                .any(is_amr_attachment);
                            let body_label = if msg.body.trim().is_empty() && has_amr {
                                "[audio recording]".to_string()
                            } else if msg.body.trim_start().starts_with("<smil") {
                                "[MMS media]".to_string()
                            } else {
                                msg.body.clone()
                            };
                            columns[1].add(egui::Label::new(&body_label).wrap(true));
                            if !self.selected_attachments.is_empty() {
                                columns[1].separator();
                                columns[1].label("Attachments:");
                                let attachments = self.selected_attachments.clone();
                                for att in attachments {
                                    let file_path =
                                        self.resolve_media_path(&att.file_path);
                                    let thumb_path = att
                                        .thumbnail_path
                                        .as_ref()
                                        .and_then(|rel| self.resolve_media_path(rel))
                                        .filter(|p| p.exists());

                                    columns[1].horizontal(|ui| {
                                        ui.label(format!("{} | {}", att.mime_type, att.file_path));
                                        if let Some(path) = file_path.as_ref() {
                                            if ui.button("Open file").clicked() {
                                                open_file(path);
                                            }
                                            if ui.button("Open location").clicked() {
                                                open_file_location(path);
                                            }
                                        }
                                    });

                                    if let Some(texture) = self.thumbnail_for(
                                        file_path.as_ref(),
                                        thumb_path.as_ref(),
                                        &att.mime_type,
                                    ) {
                                        let size = texture.size_vec2();
                                        let max_edge = 180.0;
                                        let scale = (max_edge / size.x)
                                            .min(max_edge / size.y)
                                            .min(1.0);
                                        let draw_size = egui::vec2(size.x * scale, size.y * scale);
                                        columns[1].add(egui::Image::new(SizedTexture::new(
                                            texture.id(),
                                            draw_size,
                                        )));
                                    } else {
                                        thumb_placeholder(&mut columns[1], 180.0);
                                    }
                                }
                            }
                        } else {
                            columns[1].label("Select a message");
                        }
                    });
                }
                        });
                });
            }

            if matches!(self.active_tab, AppTab::Media) {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label("Media Library");
                        ui.add_space(4.0);
                        egui::CollapsingHeader::new("Media Controls")
                            .default_open(true)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label("Database:")
                                        .on_hover_text("Database file containing attachments.");
                                    ui.text_edit_singleline(&mut self.db_path);
                                    if ui.button("Browse DB").clicked() {
                                        if let Some(path) = FileDialog::new()
                                            .add_filter("SQLite DB", &["db", "sqlite", "sqlite3"])
                                            .pick_file()
                                        {
                                            self.db_folder = path.parent().map(PathBuf::from);
                                            self.db_path = path.display().to_string();
                                        }
                                    }
                                    if ui.button("Open").clicked() {
                                        self.open_db();
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Filter:")
                                        .on_hover_text("Filter by MIME type or file name.");
                                    ui.text_edit_singleline(&mut self.media_query);
                                    if ui.button("Refresh").clicked() {
                                        self.load_media_page();
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("NSFW:");
                                    if ui
                                        .selectable_label(
                                            self.media_nsfw_filter == MediaNsfwFilter::ShowAll,
                                            "Show all NSFW",
                                        )
                                        .clicked()
                                    {
                                        self.media_nsfw_filter = MediaNsfwFilter::ShowAll;
                                        self.load_media_page();
                                    }
                                    if ui
                                        .selectable_label(
                                            self.media_nsfw_filter == MediaNsfwFilter::OnlyNsfw,
                                            "Show only NSFW",
                                        )
                                        .clicked()
                                    {
                                        self.media_nsfw_filter = MediaNsfwFilter::OnlyNsfw;
                                        self.load_media_page();
                                    }
                                    if ui
                                        .selectable_label(
                                            self.media_nsfw_filter == MediaNsfwFilter::HideNsfw,
                                            "Hide NSFW",
                                        )
                                        .clicked()
                                    {
                                        self.media_nsfw_filter = MediaNsfwFilter::HideNsfw;
                                        self.load_media_page();
                                    }
                                });
                                ui.group(|ui| {
                                    ui.label("OCR / Vision");
                                    ui.horizontal(|ui| {
                                        ui.label("Tesseract:");
                                        ui.label("Uses system tesseract binary");
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("FFmpeg:");
                                        if ui.button("Check FFmpeg").clicked() {
                                            self.ffmpeg_status = check_ffmpeg_cli();
                                        }
                                        if !self.ffmpeg_status.is_empty() {
                                            ui.label(&self.ffmpeg_status);
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Tesseract cmd:");
                                        ui.text_edit_singleline(&mut self.tesseract_cmd)
                                            .on_hover_text("Path or command name for tesseract.exe");
                                        if ui.button("Check OCR").clicked() {
                                            self.ocr_status =
                                                check_tesseract(Some(self.tesseract_cmd.as_str()));
                                        }
                                    });
                                    if !self.ocr_status.is_empty() {
                                        ui.label(&self.ocr_status);
                                    }
                                    ui.horizontal(|ui| {
                                        ui.label("Vision base URL:");
                                        ui.text_edit_singleline(&mut self.vision_base_url);
                                        if ui.button("Refresh models").clicked() {
                                            let base = self.vision_base_url.trim().to_string();
                                            self.refresh_ollama_models_for(&base);
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Vision model:");
                                        ui.text_edit_singleline(&mut self.vision_model);
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Pick:");
                                        let mut changed = false;
                                        egui::ComboBox::from_id_source("vision_models")
                                            .selected_text(if self.vision_model.is_empty() {
                                                "Select model"
                                            } else {
                                                &self.vision_model
                                            })
                                            .show_ui(ui, |ui| {
                                                for model in &self.ollama_models {
                                                    if ui
                                                        .selectable_value(
                                                            &mut self.vision_model,
                                                            model.name.clone(),
                                                            &model.name,
                                                        )
                                                        .clicked()
                                                    {
                                                        changed = true;
                                                    }
                                                }
                                            });
                                        if ui.button("Check model").clicked() {
                                            self.check_vision_model();
                                        }
                                        if changed {
                                            self.check_vision_model();
                                        }
                                    });
                                    if self.vision_model_check_in_flight {
                                        ui.label("Checking model...");
                                    } else if !self.vision_model_status.is_empty() {
                                        ui.label(&self.vision_model_status);
                                    }
                                    ui.horizontal(|ui| {
                                        ui.label("Vision prompt:");
                                        ui.text_edit_singleline(&mut self.vision_prompt);
                                    });
                                    ui.horizontal(|ui| {
                                        if ui.button("Save OCR/Vision settings").clicked() {
                                            self.save_llm_settings();
                                        }
                                        if ui.button("Save global settings").clicked() {
                                            self.save_global_settings();
                                        }
                                    });
                                });
                                ui.group(|ui| {
                                    ui.label("Media Embeddings / NSFW");
                                    ui.horizontal(|ui| {
                                        ui.label("Embedding prompt:");
                                        ui.text_edit_singleline(&mut self.media_embed_prompt);
                                    });
                                    ui.horizontal(|ui| {
                                        ui.checkbox(
                                            &mut self.media_embed_use_local,
                                            "Use local embeddings",
                                        )
                                        .on_hover_text(
                                            "Use the Embeddings tab ONNX model instead of Ollama for media captions.",
                                        );
                                        if self.media_embed_use_local {
                                            if self.embed_model_path.trim().is_empty()
                                                || self.embed_tokenizer_path.trim().is_empty()
                                            {
                                                ui.label("(set model + tokenizer in Embeddings tab)");
                                            } else {
                                                ui.label(format!(
                                                    "{} | {}",
                                                    self.embed_model_name, self.embed_model_version
                                                ));
                                            }
                                        } else if self.ollama_selected.is_empty() {
                                            ui.label("(set in Embeddings tab)");
                                        } else {
                                            ui.label(self.ollama_selected.clone());
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Keyframes max:");
                                        ui.add(
                                            egui::DragValue::new(&mut self.media_keyframe_max)
                                                .clamp_range(1..=50),
                                        );
                                        ui.label("Embeddings model:");
                                        if self.media_embed_use_local {
                                            ui.label(if self.embed_model_name.trim().is_empty() {
                                                "(local model)".to_string()
                                            } else {
                                                format!(
                                                    "{} | {}",
                                                    self.embed_model_name, self.embed_model_version
                                                )
                                            });
                                        } else if self.ollama_selected.is_empty() {
                                            ui.label("(set in Embeddings tab)");
                                        } else {
                                            ui.label(self.ollama_selected.clone());
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("NSFW prompt:");
                                        ui.text_edit_singleline(&mut self.media_nsfw_prompt);
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("NSFW threshold:");
                                        ui.add(
                                            egui::DragValue::new(&mut self.media_nsfw_threshold)
                                                .clamp_range(0.0..=1.0)
                                                .speed(0.01),
                                        );
                                        ui.label(format!("{:.2}", self.media_nsfw_threshold));
                                        if ui.button("Save global settings").clicked() {
                                            self.save_global_settings();
                                        }
                                    });
                                    if !self.media_embed_status.is_empty() {
                                        ui.label(&self.media_embed_status);
                                    }
                                });
                                ui.group(|ui| {
                                    ui.label("CLIP Media Processing (LAION)");
                            ui.horizontal(|ui| {
                                ui.label("Media root:")
                                    .on_hover_text("Base directory for attachment paths.");
                                let root_label = self
                                    .media_root
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| "(not set)".to_string());
                                ui.label(root_label);
                                if ui.button("Pick root").clicked() {
                                    if let Some(folder) = FileDialog::new().pick_folder() {
                                        self.media_root = Some(folder.clone());
                                        self.save_media_root(&folder);
                                    }
                                }
                                if ui.button("Reset to default").clicked() {
                                    let default_dir = resolve_media_dir(&self.db_path);
                                    self.media_root = Some(default_dir.clone());
                                    if default_dir.exists() {
                                        self.save_media_root(&default_dir);
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("CLIP model:")
                                    .on_hover_text("CLIP image encoder ONNX file.");
                                ui.text_edit_singleline(&mut self.clip_model_path);
                                if ui.button("Browse").clicked() {
                                    if let Some(path) = FileDialog::new().pick_file() {
                                        self.clip_model_path = path.display().to_string();
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("NSFW weights:")
                                    .on_hover_text("LAION NSFW linear probe weights (.npz).");
                                ui.text_edit_singleline(&mut self.clip_nsfw_weights_path);
                                if ui.button("Browse").clicked() {
                                    if let Some(path) = FileDialog::new().pick_file() {
                                        self.clip_nsfw_weights_path = path.display().to_string();
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("Batch:")
                                    .on_hover_text("Attachments processed per batch.");
                                ui.add(
                                    egui::DragValue::new(&mut self.clip_batch_size)
                                        .clamp_range(1..=512),
                                );
                                ui.label("Keyframes:")
                                    .on_hover_text("Max keyframes extracted per attachment.");
                                ui.add(
                                    egui::DragValue::new(&mut self.clip_max_keyframes)
                                        .clamp_range(1..=50),
                                );
                                ui.label("Workers:")
                                    .on_hover_text("Parallel keyframe extraction workers.");
                                ui.add(
                                    egui::DragValue::new(&mut self.clip_workers)
                                        .clamp_range(1..=32),
                                );
                            });
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut self.clip_reprocess, "Reprocess existing")
                                    .on_hover_text("Recompute embeddings even if already present.");
                                ui.checkbox(&mut self.clip_auto_on_import, "Auto-run after import")
                                    .on_hover_text("Run CLIP processing automatically after imports.");
                                ui.checkbox(&mut self.clip_use_cuda, "Use CUDA")
                                    .on_hover_text("Enable CUDA if ONNX Runtime CUDA EP is available.");
                                if ui.button("Save CLIP settings").clicked() {
                                    self.save_clip_settings();
                                }
                                if ui.button("Save global settings").clicked() {
                                    self.save_global_settings();
                                }
                                if ui.button("Check CUDA").clicked() {
                                    let model_path = self.clip_model_path.trim();
                                    if model_path.is_empty() {
                                        self.clip_cuda_status =
                                            "CUDA probe: set CLIP model path first".to_string();
                                    } else {
                                        self.clip_cuda_status =
                                            match sms_clip::probe_cuda_support(Path::new(model_path))
                                            {
                                                Ok(true) => "CUDA probe: available".to_string(),
                                                Ok(false) => "CUDA probe: unavailable".to_string(),
                                                Err(err) => {
                                                    format!("CUDA probe failed: {}", err)
                                                }
                                            };
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                if ui
                                    .add_enabled(self.clip_job.is_none(), egui::Button::new("Start CLIP"))
                                    .clicked()
                                {
                                    self.start_clip_processing();
                                }
                                if let Some(job) = &self.clip_job {
                                    let paused = job.progress.paused.load(Ordering::Relaxed);
                                    let cancelled = job.progress.cancelled.load(Ordering::Relaxed);
                                    let pause_label = if paused { "▶ Resume" } else { "⏸ Pause" };
                                    if ui
                                        .add_enabled(!cancelled, egui::Button::new(pause_label))
                                        .clicked()
                                    {
                                        job.progress
                                            .paused
                                            .store(!paused, Ordering::Relaxed);
                                    }
                                    if ui
                                        .add_enabled(!cancelled, egui::Button::new("Cancel"))
                                        .clicked()
                                    {
                                        job.progress
                                            .cancelled
                                            .store(true, Ordering::Relaxed);
                                    }
                                    if cancelled {
                                        ui.label("CANCELLING...");
                                    } else if paused {
                                        ui.label("PAUSED");
                                    } else {
                                        ui.label("Processing...");
                                    }
                                }
                            });
                            if let Some(job) = &self.clip_job {
                                let total = job.progress.total.load(Ordering::Relaxed);
                                let done = job.progress.done.load(Ordering::Relaxed);
                                if total > 0 {
                                    let pct = (done as f32 / total as f32).clamp(0.0, 1.0);
                                    ui.add(egui::ProgressBar::new(pct).text(format!(
                                        "{}/{} attachment(s)",
                                        done, total
                                    )));
                                } else {
                                    ui.label("Scanning attachments...");
                                }
                                if job.progress.gps_in_progress.load(Ordering::Relaxed) {
                                    ui.label("GPS tagging...");
                                } else {
                                    let tagged = job.progress.gps_tagged.load(Ordering::Relaxed);
                                    if tagged > 0 {
                                        ui.label(format!("GPS tagged: {} attachment(s)", tagged));
                                    }
                                }
                                ui.label(format!(
                                    "Elapsed: {:.1}s",
                                    job.started_at.elapsed().as_secs_f32()
                                ));
                                ctx.request_repaint_after(Duration::from_millis(200));
                            }
                            if !self.clip_cuda_status.is_empty() {
                                ui.label(&self.clip_cuda_status);
                            }
                            if !self.clip_status.is_empty() {
                                ui.label(&self.clip_status);
                            }
                            // #todo: surface per-frame progress and GPU name for CLIP processing.
                        });
                                ui.group(|ui| {
                                    ui.label("Media Semantic Search");
                                    ui.horizontal(|ui| {
                                        ui.label("Query:");
                                        ui.text_edit_singleline(&mut self.media_semantic_query);
                                        ui.checkbox(
                                            &mut self.media_semantic_use_clip,
                                            "Use CLIP embeddings",
                                        )
                                        .on_hover_text(
                                            "Uses CLIP text encoder to query CLIP media embeddings.",
                                        );
                                        ui.label("Max hits:");
                                        ui.add(
                                            egui::DragValue::new(&mut self.media_semantic_limit)
                                                .clamp_range(1..=500),
                                        );
                                        if ui.button("Search media").clicked() {
                                            self.run_media_semantic_search();
                                        }
                                        if self.media_semantic_in_flight {
                                            ui.spinner();
                                            ui.label("Searching...");
                                        }
                                    });
                                    if self.media_semantic_use_clip {
                                        ui.horizontal(|ui| {
                                            ui.label("CLIP text model:");
                                            ui.text_edit_singleline(&mut self.clip_text_model_path);
                                            if ui.button("Browse").clicked() {
                                                if let Some(path) = FileDialog::new().pick_file() {
                                                    self.clip_text_model_path =
                                                        path.display().to_string();
                                                }
                                            }
                                        });
                                        ui.horizontal(|ui| {
                                            ui.label("Tokenizer:");
                                            ui.text_edit_singleline(
                                                &mut self.clip_text_tokenizer_path,
                                            );
                                            if ui.button("Browse").clicked() {
                                                if let Some(path) = FileDialog::new().pick_file() {
                                                    self.clip_text_tokenizer_path =
                                                        path.display().to_string();
                                                }
                                            }
                                        });
                                        ui.horizontal(|ui| {
                                            if ui.button("Save global settings").clicked() {
                                                self.save_global_settings();
                                            }
                                        });
                                    }
                                    if !self.media_semantic_status.is_empty() {
                                        ui.label(&self.media_semantic_status);
                                    }
                                    if !self.media_semantic_hits.is_empty() {
                                        let hits = self.media_semantic_hits.clone();
                                        ui.label(format!("Hits: {}", hits.len()));
                                        egui::ScrollArea::vertical()
                                            .max_height(140.0)
                                            .show(ui, |ui| {
                                            for hit in hits.iter().take(50) {
                                                        let file_label = truncate_filename(&hit.attachment.file_path);
                                                        let label = format!(
                                                            "{:.4} | {} | {}",
                                                            hit.score,
                                                            hit.attachment.mime_type,
                                                            file_label
                                                        );
                                                        ui.label(label)
                                                            .on_hover_text(hit.attachment.file_path.clone());
                                                    let frame_label = if let Some(ts) = hit.frame_time_ms {
                                                        format!("frame {} @ {} ms", hit.frame_index, ts)
                                                    } else {
                                                        format!("frame {}", hit.frame_index)
                                                    };
                                                    ui.label(frame_label);
                                                        if let Some(stats) = hit.embedding_stats.as_ref() {
                                                            ui.label(format!(
                                                                "Embedding: dims {} | norm {:.3} | min {:.3} | max {:.3} | mean {:.3}",
                                                                stats.dims,
                                                                stats.norm,
                                                                stats.min,
                                                                stats.max,
                                                                stats.mean
                                                            ));
                                                            if !stats.head.is_empty() {
                                                                let head = stats
                                                                    .head
                                                                    .iter()
                                                                    .map(|v| format!("{:.3}", v))
                                                                    .collect::<Vec<_>>()
                                                                    .join(", ");
                                                                ui.label(format!("head: [{}]", head));
                                                            }
                                                            // #todo: add a sparkline or histogram for embedding distributions.
                                                        }
                                                    if let Some(caption) = hit.caption.as_ref() {
                                                        ui.label(caption);
                                                    }
                                                        let file_path =
                                                            self.resolve_media_path(&hit.attachment.file_path);
                                                        let thumb_path = hit
                                                            .attachment
                                                            .thumbnail_path
                                                            .as_ref()
                                                            .and_then(|rel| self.resolve_media_path(rel))
                                                            .filter(|p| p.exists());
                                                        if let Some(texture) = self.thumbnail_for(
                                                            file_path.as_ref(),
                                                            thumb_path.as_ref(),
                                                            &hit.attachment.mime_type,
                                                        ) {
                                                            let size = texture.size_vec2();
                                                            let max_edge = 72.0;
                                                            let scale = (max_edge / size.x)
                                                                .min(max_edge / size.y)
                                                                .min(1.0);
                                                            let draw_size =
                                                                egui::vec2(size.x * scale, size.y * scale);
                                                            ui.add(egui::Image::new(SizedTexture::new(
                                                                texture.id(),
                                                                draw_size,
                                                            )));
                                                        } else {
                                                            thumb_placeholder(ui, 72.0);
                                                        }
                                                    if let Some(path) =
                                                        self.resolve_media_path(&hit.attachment.file_path)
                                                    {
                                                        ui.horizontal(|ui| {
                                                            if ui.button("Open file").clicked() {
                                                                open_file(&path);
                                                            }
                                                            if ui.button("Open location").clicked() {
                                                                open_file_location(&path);
                                                            }
                                                            if ui.button("Inspect embeddings").clicked() {
                                                                self.start_media_embedding_inspect(&hit.attachment);
                                                            }
                                                        });
                                                    }
                                                    ui.separator();
                                                }
                                            });
                                    }
                                });
                            });
                        ui.horizontal(|ui| {
                            if ui.button("Select all on page").clicked() {
                                self.selected_media_ids = self
                                    .media_results
                                    .iter()
                                    .map(|att| att.id.clone())
                                    .collect();
                                self.media_batch_status = format!(
                                    "Selected {} item(s)",
                                    self.selected_media_ids.len()
                                );
                            }
                            if ui.button("Select none").clicked() {
                                self.selected_media_ids.clear();
                                self.media_batch_status = "Selection cleared".to_string();
                            }
                            if ui.button("Run Vision on selected").clicked() {
                                let targets: Vec<AttachmentRow> = self
                                    .media_results
                                    .iter()
                                    .filter(|att| self.selected_media_ids.contains(&att.id))
                                    .cloned()
                                    .collect();
                                if targets.is_empty() {
                                    self.media_batch_status = "No items selected".to_string();
                                } else {
                                    for att in targets {
                                        self.start_vision_for_attachment(&att);
                                    }
                                    self.media_batch_status = format!(
                                        "Queued vision for {} item(s)",
                                        self.selected_media_ids.len()
                                    );
                                }
                            }
                            if ui.button("Run NSFW on selected").clicked() {
                                let targets: Vec<AttachmentRow> = self
                                    .media_results
                                    .iter()
                                    .filter(|att| self.selected_media_ids.contains(&att.id))
                                    .cloned()
                                    .collect();
                                if targets.is_empty() {
                                    self.media_batch_status = "No items selected".to_string();
                                } else {
                                    for att in targets {
                                        self.start_nsfw_for_attachment(&att);
                                    }
                                    self.media_batch_status = format!(
                                        "Queued NSFW for {} item(s)",
                                        self.selected_media_ids.len()
                                    );
                                }
                            }
                            if ui.button("Embed selected (keyframes)").clicked() {
                                let targets: Vec<AttachmentRow> = self
                                    .media_results
                                    .iter()
                                    .filter(|att| self.selected_media_ids.contains(&att.id))
                                    .cloned()
                                    .collect();
                                if targets.is_empty() {
                                    self.media_embed_status = "No items selected".to_string();
                                } else {
                                    for att in targets {
                                        self.start_media_embedding_for_attachment(&att);
                                    }
                                    self.media_embed_status = format!(
                                        "Queued embeddings for {} item(s)",
                                        self.selected_media_ids.len()
                                    );
                                }
                            }
                        });
                        if !self.media_batch_status.is_empty() {
                            ui.label(&self.media_batch_status);
                        }
                        ui.group(|ui| {
                            ui.label("Embedding Inspector");
                            ui.horizontal(|ui| {
                                if let Some(target) = self.media_embed_inspect_target.as_ref() {
                                    ui.label(format!("Target: {}", target));
                                } else {
                                    ui.label("Target: none");
                                }
                                if ui.button("Clear").clicked() {
                                    self.media_embed_inspect_target = None;
                                    self.media_embed_inspect_rows.clear();
                                    self.media_embed_inspect_status.clear();
                                }
                                if self.media_embed_inspect_in_flight {
                                    ui.spinner();
                                    ui.label("Inspecting...");
                                }
                            });
                            if !self.media_embed_inspect_status.is_empty() {
                                ui.label(&self.media_embed_inspect_status);
                            }
                            if !self.media_embed_inspect_rows.is_empty() {
                                egui::ScrollArea::vertical()
                                    .max_height(160.0)
                                    .show(ui, |ui| {
                                        for row in &self.media_embed_inspect_rows {
                                            let frame_label = if let Some(ts) = row.frame_time_ms {
                                                format!("frame {} @ {} ms", row.frame_index, ts)
                                            } else {
                                                format!("frame {}", row.frame_index)
                                            };
                                            ui.label(format!(
                                                "{} | {} | {}",
                                                row.model_name, row.model_version, frame_label
                                            ));
                                            ui.label(format!(
                                                "dims {} | norm {:.3} | min {:.3} | max {:.3} | mean {:.3}",
                                                row.stats.dims,
                                                row.stats.norm,
                                                row.stats.min,
                                                row.stats.max,
                                                row.stats.mean
                                            ));
                                            if !row.stats.head.is_empty() {
                                                let head = row
                                                    .stats
                                                    .head
                                                    .iter()
                                                    .map(|v| format!("{:.3}", v))
                                                    .collect::<Vec<_>>()
                                                    .join(", ");
                                                ui.label(format!("head: [{}]", head));
                                            }
                                            if let Some(caption) = row.caption.as_ref() {
                                                ui.label(caption);
                                            }
                                            ui.separator();
                                        }
                                    });
                            }
                            // #todo: add PCA/UMAP projection preview for embeddings.
                        });
                        ui.group(|ui| {
                            ui.label("Media Audit");
                            ui.horizontal(|ui| {
                                if ui.button("Run audit").clicked() {
                                    self.run_media_audit();
                                }
                                if self.media_audit_in_flight {
                                    ui.spinner();
                                    ui.label("Auditing...");
                                }
                            });
                            if !self.media_audit_status.is_empty() {
                                ui.label(&self.media_audit_status);
                            }
                            if let Some(snapshot) = &self.media_audit_snapshot {
                                if let Some(root) = snapshot.media_root.as_ref() {
                                    ui.label(format!("Media root: {}", root.display()));
                                }
                                ui.label(format!(
                                    "Filesystem media files: {}",
                                    snapshot.media_files_total
                                ));
                                ui.label(format!(
                                    "DB attachments: {} (image/video: {})",
                                    snapshot.db_attachments_total,
                                    snapshot.db_image_video_total
                                ));
                                ui.label(format!(
                                    "Filesystem files not in DB: {}",
                                    snapshot.fs_unlinked_total
                                ));
                                ui.label(format!(
                                    "Missing files referenced by DB: {}",
                                    snapshot.db_missing_files
                                ));
                                if !snapshot.db_mime_counts.is_empty() {
                                    ui.label("Mime breakdown:");
                                    for (mime, count) in &snapshot.db_mime_counts {
                                        ui.label(format!("  {}: {}", mime, count));
                                    }
                                }
                                if !snapshot.db_missing_samples.is_empty() {
                                    ui.label("Missing samples:");
                                    for sample in &snapshot.db_missing_samples {
                                        ui.label(sample);
                                    }
                                }
                                if !snapshot.fs_unlinked_samples.is_empty() {
                                    ui.label("Unlinked file samples:");
                                    for sample in &snapshot.fs_unlinked_samples {
                                        ui.label(sample);
                                    }
                                }
                            }
                            // #todo: add export button for missing media report.
                        });
                        ui.group(|ui| {
                            ui.label("Media Backfill");
                            ui.label("Scan media directory and create DB records for files not yet tracked.");
                            ui.horizontal(|ui| {
                                if ui.button("Run backfill").clicked() {
                                    self.run_media_backfill();
                                }
                                if self.media_backfill_in_flight {
                                    ui.spinner();
                                    ui.label("Backfilling...");
                                    ctx.request_repaint_after(std::time::Duration::from_millis(250));
                                }
                            });
                            if !self.media_backfill_status.is_empty() {
                                ui.label(&self.media_backfill_status);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Page size:");
                            ui.add(egui::DragValue::new(&mut self.media_page_size).clamp_range(10..=10000));
                            if ui.button("Prev").clicked() {
                                self.media_page_offset =
                                    self.media_page_offset.saturating_sub(self.media_page_size);
                                self.load_media_page();
                            }
                            if ui.button("Next").clicked() {
                                // Clamp to the last page-aligned offset, not the
                                // last row index, so the final page isn't a
                                // single-item stub.
                                let page = self.media_page_size.max(1);
                                let last_page_offset = self
                                    .media_total_count
                                    .saturating_sub(1)
                                    .div_euclid(page)
                                    * page;
                                self.media_page_offset =
                                    (self.media_page_offset + page).min(last_page_offset);
                                self.load_media_page();
                            }
                            if ui.button("Load All").clicked() {
                                self.media_page_size = self.media_total_count.max(1);
                                self.media_page_offset = 0;
                                self.load_media_page();
                            }
                            if self.media_in_flight {
                                ui.label("Loading...");
                            }
                            let end = (self.media_page_offset + self.media_results.len()).min(self.media_total_count);
                            ui.label(format!(
                                "Showing {}-{} of {}",
                                self.media_page_offset + 1,
                                end,
                                self.media_total_count
                            ));
                        });
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let media = self.media_results.clone();
                                let split = media.len().div_ceil(2);
                                let (left, right) = media.split_at(split);
                                ui.columns(2, |cols| {
                                    let mut render = |ui: &mut egui::Ui, items: &[AttachmentRow]| {
                                        for att in items {
                                            let file_path =
                                                self.resolve_media_path(&att.file_path);
                                            let thumb_path = att
                                                .thumbnail_path
                                                .as_ref()
                                                .and_then(|rel| self.resolve_media_path(rel))
                                                .filter(|p| p.exists());
                                            let mut meta_line = String::new();
                                            if let Some(path) = file_path.as_ref() {
                                                if let Ok(meta) = fs::metadata(path) {
                                                    let size_kb =
                                                        (meta.len() as f64 / 1024.0).round() as u64;
                                                    meta_line = format!("{} KB", size_kb);
                                                }
                                            }
                                            if let Some(ts) = att.timestamp {
                                                let msg_time = format_timestamp(ts);
                                                if meta_line.is_empty() {
                                                    meta_line = format!("message {}", msg_time);
                                                } else {
                                                    meta_line.push_str(&format!(" | message {}", msg_time));
                                                }
                                            }
                                            if let Some(address) = att.address.as_ref() {
                                                let name = self.contact_name_cache.get(address);
                                                let sender_label = if let Some(name) = name {
                                                    format!("sender {} ({})", name, address)
                                                } else {
                                                    format!("sender {}", address)
                                                };
                                                if meta_line.is_empty() {
                                                    meta_line = sender_label;
                                                } else {
                                                    meta_line.push_str(&format!(" | {}", sender_label));
                                                }
                                            }
                                            ui.horizontal(|ui| {
                                                let mut selected =
                                                    self.selected_media_ids.contains(&att.id);
                                                if ui
                                                    .checkbox(&mut selected, "")
                                                    .on_hover_text("Select for batch actions")
                                                    .clicked()
                                                {
                                                    if selected {
                                                        self.selected_media_ids
                                                            .insert(att.id.clone());
                                                    } else {
                                                        self.selected_media_ids
                                                            .remove(&att.id);
                                                    }
                                                }
                                                let file_label = truncate_filename(&att.file_path);
                                                let label = format!("{} | {}", att.mime_type, file_label);
                                                let label_text = if let Some(nsfw) = att.nsfw_label.as_ref() {
                                                    format!("{} | NSFW:{}", label, nsfw)
                                                } else {
                                                    label
                                                };
                                                let hover = if meta_line.is_empty() {
                                                    att.file_path.clone()
                                                } else {
                                                    format!("{}\n{}", att.file_path, meta_line)
                                                };
                                                ui.label(label_text).on_hover_text(hover);
                                            });
                                            ui.horizontal(|ui| {
                                                if let Some(path) = file_path.as_ref() {
                                                    if ui.button("Open file").clicked() {
                                                        open_file(path);
                                                    }
                                                    if ui.button("Open location").clicked() {
                                                        open_file_location(path);
                                                    }
                                                }
                                        if ui.button("Open thread").clicked() {
                                            let anchor = att
                                                .message_id
                                                .as_ref()
                                                .and_then(|id| uuid::Uuid::parse_str(id).ok());
                                            self.open_thread_from_parts(
                                                att.thread_id.as_ref(),
                                                att.address.as_ref(),
                                                anchor,
                                            );
                                            self.active_tab = AppTab::Search;
                                        }
                                        if ui.button("Inspect embeddings").clicked() {
                                            self.start_media_embedding_inspect(att);
                                        }
                                            });
                                            if let Some(texture) = self.thumbnail_for(
                                                file_path.as_ref(),
                                                thumb_path.as_ref(),
                                                &att.mime_type,
                                            ) {
                                                let size = texture.size_vec2();
                                                let max_edge = 180.0;
                                                let scale =
                                                    (max_edge / size.x).min(max_edge / size.y).min(1.0);
                                                let draw_size = egui::vec2(size.x * scale, size.y * scale);
                                                ui.add(egui::Image::new(SizedTexture::new(
                                                    texture.id(),
                                                    draw_size,
                                                )));
                                            } else {
                                                thumb_placeholder(ui, 180.0);
                                            }
                                            if let Some(text) = att.ocr_text.as_ref() {
                                                ui.group(|ui| {
                                                    ui.label("📝 Extracted Text");
                                                    ui.label(text);
                                                    if let Some(model) = att.ocr_model.as_ref() {
                                                        ui.label(format!("Model: {}", model));
                                                    }
                                                    if let Some(ts) = att.ocr_timestamp {
                                                        ui.label(format!("OCR at {}", format_timestamp(ts)));
                                                    }
                                                });
                                            } else if self.ocr_in_progress.contains(&att.id) {
                                                ui.horizontal(|ui| {
                                                    ui.spinner();
                                                    ui.label("Running OCR...");
                                                });
                                            } else if ui.button("🔍 Run OCR").clicked() {
                                                self.start_ocr_for_attachment(att);
                                            }
                                            if let Some(analysis) = att.vision_analysis.as_ref() {
                                                ui.group(|ui| {
                                                    ui.label("🔎 Vision Analysis");
                                                    let (duration, body) = split_vision_analysis(analysis);
                                                    if let Some(label) = duration {
                                                        ui.label(label);
                                                    }
                                                    ui.label(body);
                                                    if let Some(model) = att.vision_model.as_ref() {
                                                        ui.label(format!("Model: {}", model));
                                                    }
                                                    if let Some(ts) = att.vision_timestamp {
                                                        ui.label(format!("Vision at {}", format_timestamp(ts)));
                                                    }
                                                    if self.vision_in_progress.contains(&att.id) {
                                                        ui.horizontal(|ui| {
                                                            ui.spinner();
                                                            ui.label("Rerunning vision...");
                                                        });
                                                    } else if ui.button("Rerun Vision").clicked() {
                                                        self.start_vision_for_attachment(att);
                                                    }
                                                });
                                            } else if self.vision_in_progress.contains(&att.id) {
                                                ui.horizontal(|ui| {
                                                    ui.spinner();
                                                    ui.label("Running vision...");
                                                });
                                            } else if ui.button("🧠 Run Vision").clicked() {
                                                self.start_vision_for_attachment(att);
                                            }
                                            if let Some(label) = att.nsfw_label.as_ref() {
                                                ui.group(|ui| {
                                                    let score_text = att
                                                        .nsfw_score
                                                        .map(|s| format!("{:.2}", s))
                                                        .unwrap_or_else(|| "?".to_string());
                                                    ui.label(format!("🚫 NSFW: {} ({})", label, score_text));
                                                    if let Some(model) = att.nsfw_model.as_ref() {
                                                        ui.label(format!("Model: {}", model));
                                                    }
                                                    if let Some(ts) = att.nsfw_timestamp {
                                                        ui.label(format!(
                                                            "NSFW at {}",
                                                            format_timestamp(ts)
                                                        ));
                                                    }
                                                });
                                            } else if self.nsfw_in_progress.contains(&att.id) {
                                                ui.horizontal(|ui| {
                                                    ui.spinner();
                                                    ui.label("Running NSFW...");
                                                });
                                            } else if ui.button("🚫 Run NSFW").clicked() {
                                                self.start_nsfw_for_attachment(att);
                                            }
                                            if self.media_embed_in_progress.contains(&att.id) {
                                                ui.horizontal(|ui| {
                                                    ui.spinner();
                                                    ui.label("Embedding media...");
                                                });
                                            } else if ui.button("🧬 Embed (keyframes)").clicked() {
                                                self.start_media_embedding_for_attachment(att);
                                            }
                                            ui.separator();
                                        }
                                    };
                                    render(&mut cols[0], left);
                                    render(&mut cols[1], right);
                                });
                            });
                    });
            }

            if matches!(self.active_tab, AppTab::Contacts) {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label("Contacts");
                        ui.add_space(4.0);
                ui.group(|ui| {
                    ui.label("My Addresses (for iMessage alignment)");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.self_address_input);
                        if ui.button("Add").clicked() {
                            let addr = self.self_address_input.trim().to_string();
                            if !addr.is_empty()
                                && !self.self_addresses.iter().any(|a| a == &addr)
                            {
                                self.self_addresses.push(addr);
                                self.self_addresses.sort();
                                self.save_self_addresses();
                            }
                            self.self_address_input.clear();
                        }
                        if ui.button("Save").clicked() {
                            self.save_self_addresses();
                        }
                    });
                    if !self.self_addresses.is_empty() {
                        for addr in self.self_addresses.clone() {
                            ui.horizontal(|ui| {
                                ui.label(addr.clone());
                                if ui.button("Remove").clicked() {
                                    self.self_addresses.retain(|a| a != &addr);
                                    self.save_self_addresses();
                                }
                            });
                        }
                    } else {
                        ui.label("Add your own phone numbers or handles.");
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Search:");
                    ui.text_edit_singleline(&mut self.contact_search);
                    if ui.button("Refresh").clicked() {
                        self.refresh_contacts();
                    }
                    if ui.button("Import from messages").clicked() {
                        self.import_contacts_from_messages();
                    }
                    if ui.button("Sync names from XML").clicked() {
                        self.import_contacts_from_xml();
                    }
                    // Developer-only diagnostic; hidden unless SMS_DEBUG_TOOLS=1.
                    if debug_tools_enabled()
                        && ui.button("Test XML sync (workspace)").clicked()
                    {
                        self.test_contact_name_sync_workspace_xml();
                    }
                    if ui.button("Find duplicates").clicked() {
                        self.find_contact_duplicates();
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Import vCard").clicked() {
                        self.import_contacts_vcf();
                    }
                    if ui.button("Export vCard").clicked() {
                        self.export_contacts_vcf();
                    }
                    if ui.button("Import CSV").clicked() {
                        self.import_contacts_csv();
                    }
                    if ui.button("Export CSV").clicked() {
                        self.export_contacts_csv();
                    }
                });
                if self.contacts_in_flight {
                    ui.label("Loading contacts...");
                }
                if !self.contact_status.is_empty() {
                    ui.label(&self.contact_status);
                }
                let contacts = self.contacts.clone();
                let mut pending_select: Option<String> = None;
                let mut pending_new = false;
                let mut request_merge_preview = false;
                let mut pending_duplicate_merge: Option<(String, String)> = None;
                let mut action_save = false;
                let mut action_delete = false;
                let mut action_preview_merge = false;
                let mut action_merge = false;
                let mut action_show_media = false;
                let mut action_open_thread = false;
                let mut open_thread_addr: Option<String> = None;
                ui.columns(2, |columns| {
                    columns[0].heading(format!("Contacts: {}", contacts.len()));
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(&mut columns[0], |ui| {
                            for contact in &contacts {
                                let label = if let Some(primary) = &contact.primary {
                                    format!("{} | {}", contact.display_name, primary)
                                } else {
                                    contact.display_name.clone()
                                };
                                let selected = self
                                    .selected_contact_id
                                    .as_ref()
                                    .map(|id| id == &contact.id)
                                    .unwrap_or(false);
                                if ui.selectable_label(selected, label).clicked() {
                                    pending_select = Some(contact.id.clone());
                                }
                            }
                        });

                    columns[1].heading("Details");
                    if let Some(detail) = &mut self.contact_detail {
                        columns[1].horizontal(|ui| {
                            ui.label("Name:");
                            ui.text_edit_singleline(&mut detail.display_name);
                            ui.checkbox(&mut detail.favorite, "⭐ Favorite");
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Nickname:");
                            ui.text_edit_singleline(&mut detail.nickname);
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Company:");
                            ui.text_edit_singleline(&mut detail.company);
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Email:");
                            ui.text_edit_singleline(&mut detail.email);
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Primary phone:");
                            ui.text_edit_singleline(&mut detail.phone_primary);
                            let primary_label = match detail.phone_primary_type.as_str() {
                                "mobile" => "Mobile",
                                "home" => "Home",
                                "work" => "Work",
                                "" => "Type",
                                other => other,
                            };
                            egui::ComboBox::from_id_source("primary_phone_type")
                                .selected_text(primary_label)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut detail.phone_primary_type,
                                        "mobile".to_string(),
                                        "Mobile",
                                    );
                                    ui.selectable_value(
                                        &mut detail.phone_primary_type,
                                        "home".to_string(),
                                        "Home",
                                    );
                                    ui.selectable_value(
                                        &mut detail.phone_primary_type,
                                        "work".to_string(),
                                        "Work",
                                    );
                                });
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Secondary phone:");
                            ui.text_edit_singleline(&mut detail.phone_secondary);
                            let secondary_label = match detail.phone_secondary_type.as_str() {
                                "mobile" => "Mobile",
                                "home" => "Home",
                                "work" => "Work",
                                "" => "Type",
                                other => other,
                            };
                            egui::ComboBox::from_id_source("secondary_phone_type")
                                .selected_text(secondary_label)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut detail.phone_secondary_type,
                                        "mobile".to_string(),
                                        "Mobile",
                                    );
                                    ui.selectable_value(
                                        &mut detail.phone_secondary_type,
                                        "home".to_string(),
                                        "Home",
                                    );
                                    ui.selectable_value(
                                        &mut detail.phone_secondary_type,
                                        "work".to_string(),
                                        "Work",
                                    );
                                });
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Website:");
                            ui.text_edit_singleline(&mut detail.website);
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Social:");
                            ui.text_edit_singleline(&mut detail.social_media);
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Address:");
                            ui.text_edit_singleline(&mut detail.address);
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Birthday:");
                            ui.text_edit_singleline(&mut detail.birthday);
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Last contacted:");
                            let last_contacted = detail
                                .last_contacted
                                .map(format_timestamp)
                                .unwrap_or_else(|| "—".to_string());
                            ui.label(last_contacted);
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Avatar:");
                            ui.text_edit_singleline(&mut detail.avatar_path);
                        });
                        columns[1].label("Notes:");
                        columns[1].add(
                            egui::TextEdit::multiline(&mut detail.notes).desired_rows(3),
                        );
                        columns[1].separator();
                        columns[1].label("Addresses:");
                        for addr in &detail.addresses {
                            columns[1].label(addr);
                        }
                        columns[1].horizontal(|ui| {
                            ui.label("Add address:");
                            ui.text_edit_singleline(&mut self.contact_new_address);
                            if ui.button("Add").clicked() {
                                let addr = self.contact_new_address.trim().to_string();
                                if !addr.is_empty()
                                    && !detail.addresses.iter().any(|a| a == &addr)
                                {
                                    detail.addresses.push(addr);
                                }
                                self.contact_new_address.clear();
                            }
                        });
                        columns[1].separator();
                        columns[1].horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                action_save = true;
                            }
                            if ui.button("Delete").clicked() {
                                action_delete = true;
                            }
                        });
                        columns[1].horizontal(|ui| {
                            ui.label("Merge from:");
                            egui::ComboBox::from_id_source("merge_contact")
                                .selected_text(
                                    self.contact_merge_source
                                        .as_deref()
                                        .unwrap_or("Select contact"),
                                )
                                .show_ui(ui, |ui| {
                                    for c in &self.contacts {
                                        if Some(&c.id) != self.selected_contact_id.as_ref() {
                                            ui.selectable_value(
                                                &mut self.contact_merge_source,
                                                Some(c.id.clone()),
                                                &c.display_name,
                                            );
                                        }
                                    }
                                });
                            if ui.button("Preview merge").clicked() {
                                action_preview_merge = true;
                            }
                            if ui.button("Merge into selected").clicked() {
                                action_merge = true;
                            }
                        });
                        columns[1].horizontal(|ui| {
                            if ui.button("Show media").clicked() {
                                action_show_media = true;
                            }
                            if ui.button("Open thread").clicked() {
                                action_open_thread = true;
                                open_thread_addr = primary_contact_address(detail);
                            }
                        });
                    } else {
                        if columns[1].button("New contact").clicked() {
                            pending_new = true;
                        }
                        columns[1].label("Select a contact or create a new one.");
                    }
                });
                ui.separator();
                ui.label(format!(
                    "Duplicate groups: {}",
                    self.duplicate_groups.len()
                ));
                if self.duplicates_in_flight {
                    ui.label("Scanning for duplicates...");
                } else if self.duplicate_groups.is_empty() {
                    ui.label("No duplicates found yet.");
                } else {
                    for (idx, group) in self.duplicate_groups.iter().enumerate() {
                        ui.group(|ui| {
                            ui.label(format!("Group {}: {} contacts", idx + 1, group.len()));
                            for contact in group {
                                let selected = self
                                    .selected_contact_id
                                    .as_ref()
                                    .map(|id| id == &contact.id)
                                    .unwrap_or(false);
                                ui.horizontal(|ui| {
                                    ui.label(&contact.display_name);
                                    if ui.button("Set target").clicked() {
                                        pending_select = Some(contact.id.clone());
                                    }
                                    if selected {
                                        ui.label("Target");
                                    } else {
                                        let target_id = self
                                            .selected_contact_id
                                            .clone()
                                            .or_else(|| group.first().map(|c| c.id.clone()));
                                        if let Some(target) = target_id {
                                            if target != contact.id
                                                && ui.button("Preview merge").clicked()
                                            {
                                                pending_duplicate_merge =
                                                    Some((target, contact.id.clone()));
                                            }
                                        }
                                    }
                                });
                            }
                        });
                    }
                }
                if let Some(id) = pending_select {
                    self.selected_contact_id = Some(id.clone());
                    self.load_contact_detail(&id);
                }
                if pending_new {
                    self.new_contact();
                }
                    if action_save {
                        self.save_contact();
                    }
                    if action_delete {
                        self.delete_contact();
                    }
                    if action_preview_merge {
                        request_merge_preview = true;
                    }
                    if action_merge {
                        self.merge_contacts();
                    }
                    if action_show_media {
                        self.show_contact_media();
                    }
                    if action_open_thread {
                        if let Some(addr) = open_thread_addr {
                            self.open_thread_from_parts(None, Some(&addr), None);
                            self.active_tab = AppTab::Search;
                        } else {
                            self.contact_status =
                                "Contact has no address to open a thread".to_string();
                        }
                    }
                    if request_merge_preview {
                    self.prepare_merge_state();
                }
                if let Some((target, source)) = pending_duplicate_merge {
                    self.selected_contact_id = Some(target.clone());
                    self.contact_merge_source = Some(source);
                    self.load_contact_detail(&target);
                    self.prepare_merge_state();
                }
                if let Some(state) = &mut self.contact_merge_state {
                    ui.separator();
                    ui.label("Merge conflict helper");
                    ui.columns(3, |cols| {
                        cols[0].label("Field");
                        cols[1].label("Target");
                        cols[2].label("Source");
                        let mut row = |label: &str,
                                       target: &str,
                                       source: &str,
                                       choice: &mut MergeChoice| {
                            cols[0].label(label);
                            let target_sel = *choice == MergeChoice::Target;
                            let source_sel = *choice == MergeChoice::Source;
                            if cols[1].selectable_label(target_sel, target).clicked() {
                                *choice = MergeChoice::Target;
                            }
                            if cols[2].selectable_label(source_sel, source).clicked() {
                                *choice = MergeChoice::Source;
                            }
                        };
                        row(
                            "Name",
                            &state.target.display_name,
                            &state.source.display_name,
                            &mut state.name,
                        );
                        row(
                            "Nickname",
                            &state.target.nickname,
                            &state.source.nickname,
                            &mut state.nickname,
                        );
                        row(
                            "Company",
                            &state.target.company,
                            &state.source.company,
                            &mut state.company,
                        );
                        row(
                            "Email",
                            &state.target.email,
                            &state.source.email,
                            &mut state.email,
                        );
                        row(
                            "Phone 1",
                            &state.target.phone_primary,
                            &state.source.phone_primary,
                            &mut state.phone_primary,
                        );
                        row(
                            "Phone 2",
                            &state.target.phone_secondary,
                            &state.source.phone_secondary,
                            &mut state.phone_secondary,
                        );
                        row(
                            "Phone 1 type",
                            &state.target.phone_primary_type,
                            &state.source.phone_primary_type,
                            &mut state.phone_primary_type,
                        );
                        row(
                            "Phone 2 type",
                            &state.target.phone_secondary_type,
                            &state.source.phone_secondary_type,
                            &mut state.phone_secondary_type,
                        );
                        row(
                            "Website",
                            &state.target.website,
                            &state.source.website,
                            &mut state.website,
                        );
                        row(
                            "Social",
                            &state.target.social_media,
                            &state.source.social_media,
                            &mut state.social_media,
                        );
                        row(
                            "Notes",
                            &state.target.notes,
                            &state.source.notes,
                            &mut state.notes,
                        );
                        row(
                            "Address",
                            &state.target.address,
                            &state.source.address,
                            &mut state.address,
                        );
                        row(
                            "Birthday",
                            &state.target.birthday,
                            &state.source.birthday,
                            &mut state.birthday,
                        );
                        let target_last = state
                            .target
                            .last_contacted
                            .map(format_timestamp)
                            .unwrap_or_else(|| "—".to_string());
                        let source_last = state
                            .source
                            .last_contacted
                            .map(format_timestamp)
                            .unwrap_or_else(|| "—".to_string());
                        row(
                            "Last contacted",
                            &target_last,
                            &source_last,
                            &mut state.last_contacted,
                        );
                        let target_fav = if state.target.favorite { "Yes" } else { "No" };
                        let source_fav = if state.source.favorite { "Yes" } else { "No" };
                        row(
                            "Favorite",
                            target_fav,
                            source_fav,
                            &mut state.favorite,
                        );
                        row(
                            "Avatar",
                            &state.target.avatar_path,
                            &state.source.avatar_path,
                            &mut state.avatar_path,
                        );
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Apply merge").clicked() {
                            self.apply_merge_state();
                        }
                        if ui.button("Cancel merge").clicked() {
                            self.contact_merge_state = None;
                        }
                    });
                }
                    });
            }

            if matches!(self.active_tab, AppTab::Timeline) {
                ui.push_id("timeline_tab", |ui| {
                    egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                        ui.heading("Timeline");
                        ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Since");
                    ui.text_edit_singleline(&mut self.timeline_filters.since)
                        .on_hover_text("YYYY-MM-DD or epoch ms");
                    ui.label("Until");
                    ui.text_edit_singleline(&mut self.timeline_filters.until)
                        .on_hover_text("YYYY-MM-DD or epoch ms");
                    ui.label("Names");
                    ui.text_edit_singleline(&mut self.timeline_filters.address)
                        .on_hover_text("Filter by sender/recipient name or address");
                });
                ui.horizontal(|ui| {
                    ui.label("Granularity");
                    egui::ComboBox::from_id_source("timeline_granularity")
                        .selected_text(self.timeline_filters.granularity.label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.timeline_filters.granularity,
                                TimelineGranularity::Day,
                                TimelineGranularity::Day.label(),
                            );
                            ui.selectable_value(
                                &mut self.timeline_filters.granularity,
                                TimelineGranularity::Week,
                                TimelineGranularity::Week.label(),
                            );
                            ui.selectable_value(
                                &mut self.timeline_filters.granularity,
                                TimelineGranularity::Month,
                                TimelineGranularity::Month.label(),
                            );
                        });
                    ui.label("Chart");
                    egui::ComboBox::from_id_source("timeline_chart_mode")
                        .selected_text(self.timeline_chart_mode.label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.timeline_chart_mode,
                                TimelineChartMode::Bar,
                                TimelineChartMode::Bar.label(),
                            );
                            ui.selectable_value(
                                &mut self.timeline_chart_mode,
                                TimelineChartMode::Line,
                                TimelineChartMode::Line.label(),
                            );
                        });
                    if ui.button("Refresh").clicked() {
                        self.refresh_timeline();
                    }
                    if ui.button("Clear").clicked() {
                        self.timeline_filters = TimelineFilters::default();
                    }
                    if self.timeline_in_flight {
                        ui.label("Loading timeline...");
                    }
                });

                ui.group(|ui| {
                    ui.label("Names");
                    ui.horizontal(|ui| {
                        ui.label("Find:");
                        ui.text_edit_singleline(&mut self.timeline_name_query);
                        if ui.button("Clear").clicked() {
                            self.timeline_name_query.clear();
                        }
                        ui.label(format!(
                            "Selected: {}",
                            self.timeline_selected_addresses.len()
                        ));
                        if ui.button("Select all").clicked() {
                            self.timeline_selected_addresses = self
                                .contact_name_cache
                                .keys()
                                .cloned()
                                .collect();
                            self.sync_timeline_address_filter();
                        }
                        if ui.button("Select none").clicked() {
                            self.timeline_selected_addresses.clear();
                            self.sync_timeline_address_filter();
                        }
                    });
                    let mut entries: Vec<(String, String)> = self
                        .contact_name_cache
                        .iter()
                        .map(|(addr, name)| (name.clone(), addr.clone()))
                        .collect();
                    entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
                    let needle = self.timeline_name_query.trim().to_lowercase();
                    ui.push_id("timeline_names", |ui| {
                        egui::ScrollArea::vertical()
                            .id_source("timeline_names_scroll")
                            .max_height(180.0)
                            .show(ui, |ui| {
                            for (name, addr) in entries {
                                if !needle.is_empty() {
                                    let name_lc = name.to_lowercase();
                                    let addr_lc = addr.to_lowercase();
                                    if !name_lc.contains(&needle) && !addr_lc.contains(&needle) {
                                        continue;
                                    }
                                }
                                let label = if name.trim().is_empty() {
                                    addr.clone()
                                } else {
                                    format!("{} ({})", name, addr)
                                };
                                let mut selected = self.timeline_selected_addresses.contains(&addr);
                                ui.push_id(&addr, |ui| {
                                    if ui.checkbox(&mut selected, label).clicked() {
                                        if selected {
                                            self.timeline_selected_addresses.insert(addr.clone());
                                        } else {
                                            self.timeline_selected_addresses.remove(&addr);
                                        }
                                        self.sync_timeline_address_filter();
                                    }
                                });
                            }
                                });
                            });
                });

                if let Some(stats) = &self.timeline_stats {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(format!("Total messages: {}", stats.total_messages));
                        ui.label(format!("Threads: {}", stats.total_threads));
                        ui.label(format!("Sent: {}", stats.sent_messages));
                        ui.label(format!("Received: {}", stats.received_messages));
                        ui.label(format!("Attachments: {}", stats.total_attachments));
                    });
                    if let Some((hour, count)) = stats.busiest_hour {
                        ui.label(format!("Busiest hour: {:02}:00 ({} msgs)", hour, count));
                    }
                    if let Some((bucket, count)) = &stats.busiest_bucket {
                        ui.label(format!("Busiest {}: {} ({} msgs)", self.timeline_filters.granularity.label(), bucket, count));
                    }
                    ui.add_space(6.0);
                    ui.label(format!(
                        "Activity by {}:",
                        self.timeline_filters.granularity.label()
                    ));
                    let mut buckets: Vec<String> = stats
                        .series_text
                        .iter()
                        .map(|(label, _)| label.clone())
                        .collect();
                    for (label, _) in &stats.series_media {
                        if !buckets.iter().any(|b| b == label) {
                            buckets.push(label.clone());
                        }
                    }
                    buckets.sort();
                    let text_map: HashMap<String, i64> = stats
                        .series_text
                        .iter()
                        .cloned()
                        .collect();
                    let media_map: HashMap<String, i64> = stats
                        .series_media
                        .iter()
                        .cloned()
                        .collect();
                    let mut max = 1i64;
                    for label in &buckets {
                        let t = text_map.get(label).copied().unwrap_or(0);
                        let m = media_map.get(label).copied().unwrap_or(0);
                        max = max.max(t.max(m));
                    }
                    let bar_height = 140.0;
                    let bar_width = 14.0;
                    let spacing = 6.0;
                    let max_label_len = buckets
                        .iter()
                        .map(|b| b.chars().count())
                        .max()
                        .unwrap_or(1);
                    let label_height = (max_label_len as f32 * 9.0).clamp(28.0, 160.0);
                    let total_width = (buckets.len() as f32) * (bar_width + spacing) + spacing;

                    ui.push_id("timeline_chart", |ui| {
                        egui::ScrollArea::horizontal()
                            .id_source("timeline_chart_plot")
                            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let width = total_width.max(ui.available_width());
                                let height = bar_height + label_height + 28.0;
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(width, height),
                                    egui::Sense::hover(),
                                );
                                let painter = ui.painter_at(rect);
                                let mut x = rect.left() + spacing;
                                let y = rect.bottom() - label_height - 6.0;
                                let label_y = rect.bottom() - label_height + 2.0;
                                let label_font = egui::FontId::proportional(10.0);
                                if self.timeline_chart_mode == TimelineChartMode::Bar {
                                    for label in &buckets {
                                        let value = text_map.get(label).copied().unwrap_or(0);
                                        let height = (value as f32 / max as f32) * bar_height;
                                        let bar_rect = egui::Rect::from_min_max(
                                            egui::pos2(x, y - height),
                                            egui::pos2(x + bar_width, y),
                                        );
                                        painter.rect_filled(
                                            bar_rect,
                                            2.0,
                                            egui::Color32::from_rgb(80, 140, 240),
                                        );
                                        painter.text(
                                            egui::pos2(x + (bar_width * 0.5), label_y),
                                            egui::Align2::CENTER_TOP,
                                            vertical_label(label),
                                            label_font.clone(),
                                            egui::Color32::GRAY,
                                        );
                                        x += bar_width + spacing;
                                    }
                                } else {
                                    let text_color = egui::Color32::from_rgb(80, 140, 240);
                                    let media_color = egui::Color32::from_rgb(240, 120, 80);
                                    let mut prev_text: Option<egui::Pos2> = None;
                                    let mut prev_media: Option<egui::Pos2> = None;
                                    for label in &buckets {
                                        let t = text_map.get(label).copied().unwrap_or(0);
                                        let m = media_map.get(label).copied().unwrap_or(0);
                                        let tx = x + (bar_width * 0.5);
                                        let t_y = y - (t as f32 / max as f32) * bar_height;
                                        let m_y = y - (m as f32 / max as f32) * bar_height;
                                        let t_pos = egui::pos2(tx, t_y);
                                        let m_pos = egui::pos2(tx, m_y);
                                        painter.circle_filled(t_pos, 2.5, text_color);
                                        painter.circle_filled(m_pos, 2.5, media_color);
                                        if let Some(prev) = prev_text {
                                            painter.line_segment(
                                                [prev, t_pos],
                                                egui::Stroke::new(1.5, text_color),
                                            );
                                        }
                                        if let Some(prev) = prev_media {
                                            painter.line_segment(
                                                [prev, m_pos],
                                                egui::Stroke::new(1.5, media_color),
                                            );
                                        }
                                        painter.text(
                                            egui::pos2(tx, label_y),
                                            egui::Align2::CENTER_TOP,
                                            vertical_label(label),
                                            label_font.clone(),
                                            egui::Color32::GRAY,
                                        );
                                        prev_text = Some(t_pos);
                                        prev_media = Some(m_pos);
                                        x += bar_width + spacing;
                                    }
                                }
                            });
                    });

                    ui.add_space(8.0);
                    ui.label("Top contacts:");
                    if stats.top_contacts.is_empty() {
                        ui.label("No contacts found.");
                    } else {
                        for (addr, count) in &stats.top_contacts {
                            let label = self
                                .contact_name_cache
                                .get(addr)
                                .map(|name| format!("{} ({})", name, addr))
                                .unwrap_or_else(|| addr.clone());
                            ui.label(format!("{} — {}", label, count));
                        }
                    }
                } else {
                    ui.label("No stats loaded yet.");
                }
                        });
                    });
            }

            if matches!(self.active_tab, AppTab::Analytics) {
                analytics_tab::render_analytics_tab(self, ui);
            }

            if matches!(self.active_tab, AppTab::Assistant) {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label("Assistant");
                        ui.add_space(4.0);
                        ui.group(|ui| {
                            ui.label("Assistant Settings");
                            ui.horizontal(|ui| {
                                ui.label("Base URL:");
                                ui.text_edit_singleline(&mut self.assistant_base_url);
                                if ui.button("Refresh models").clicked() {
                                    let base = self.assistant_base_url.trim().to_string();
                                    self.refresh_ollama_models_for(&base);
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("Model:");
                                ui.text_edit_singleline(&mut self.assistant_model);
                            });
                            ui.horizontal(|ui| {
                                ui.label("Pick:");
                                let mut changed = false;
                                egui::ComboBox::from_id_source("assistant_models")
                                    .selected_text(if self.assistant_model.is_empty() {
                                        "Select model"
                                    } else {
                                        &self.assistant_model
                                    })
                                    .show_ui(ui, |ui| {
                                        for model in &self.ollama_models {
                                            if ui
                                                .selectable_value(
                                                    &mut self.assistant_model,
                                                    model.name.clone(),
                                                    &model.name,
                                                )
                                                .clicked()
                                            {
                                                changed = true;
                                            }
                                        }
                                    });
                                if ui.button("Check model").clicked() {
                                    self.check_assistant_model();
                                }
                                if changed {
                                    self.check_assistant_model();
                                }
                            });
                            if self.assistant_model_check_in_flight {
                                ui.label("Checking model...");
                            } else if !self.assistant_model_status.is_empty() {
                                ui.label(&self.assistant_model_status);
                            }
                            if ui.button("Apply & Save").clicked() {
                                self.assistant.ollama_url =
                                    self.assistant_base_url.trim().to_string();
                                self.assistant.model = self.assistant_model.trim().to_string();
                                self.save_llm_settings();
                            }
                            if !self.ollama_models_source.is_empty() {
                                ui.label(format!(
                                    "Models source: {}",
                                    self.ollama_models_source
                                ));
                            }
                        });
                        egui::ScrollArea::vertical()
                            .stick_to_bottom(true)
                            .max_height((ui.available_height() - 100.0).max(100.0))
                            .show(ui, |ui| {
                                for msg in self.assistant.get_messages() {
                                    ui.group(|ui| {
                                        let (label, color) = match msg.role.as_str() {
                                            "user" => ("You:", egui::Color32::BLUE),
                                            "assistant" => {
                                                ("Assistant:", egui::Color32::DARK_GREEN)
                                            }
                                            "tool" => ("Tool:", egui::Color32::DARK_GRAY),
                                            _ => ("Message:", egui::Color32::GRAY),
                                        };
                                        ui.colored_label(color, label);
                                        ui.label(&msg.content);
                                    });
                                }
                                // Live-streaming answer for the in-flight turn.
                                if self.assistant_waiting {
                                    let live = self
                                        .assistant_stream
                                        .lock()
                                        .ok()
                                        .map(|b| b.clone())
                                        .unwrap_or_default();
                                    ui.group(|ui| {
                                        ui.colored_label(
                                            egui::Color32::DARK_GREEN,
                                            "Assistant:",
                                        );
                                        if live.is_empty() {
                                            ui.horizontal(|ui| {
                                                ui.add(egui::Spinner::new());
                                                ui.label("Thinking…");
                                            });
                                        } else {
                                            ui.label(&live);
                                        }
                                    });
                                }
                            });
                        ui.separator();
                        ui.horizontal(|ui| {
                            let send_enabled = !self.assistant_waiting;
                            let input_width = (ui.available_width() - 140.0).max(80.0);
                            let input = ui.add_enabled(
                                send_enabled,
                                egui::TextEdit::singleline(&mut self.assistant_input)
                                    .desired_width(input_width),
                            );
                            let send_clicked = ui
                                .add_enabled(send_enabled, egui::Button::new("Send"))
                                .clicked();
                            let enter_pressed = input.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            if send_clicked || enter_pressed {
                                self.send_assistant_message();
                            }
                            if ui.button("Clear").clicked() {
                                self.assistant.clear_history();
                            }
                            if self.assistant_waiting
                                && ui.button("⏹ Stop").clicked()
                            {
                                self.assistant_cancel.store(true, Ordering::Relaxed);
                            }
                        });
                    });
            }

            if matches!(self.active_tab, AppTab::Map) {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.heading("Map");
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label("Since");
                            ui.text_edit_singleline(&mut self.map_filters.since)
                                .on_hover_text("YYYY-MM-DD or epoch ms");
                            ui.label("Until");
                            ui.text_edit_singleline(&mut self.map_filters.until)
                                .on_hover_text("YYYY-MM-DD or epoch ms");
                            ui.label("Name/Sender");
                            ui.text_edit_singleline(&mut self.map_filters.address)
                                .on_hover_text("Filter by sender/recipient name or address");
                        });
                        ui.horizontal(|ui| {
                            ui.label("MIME");
                            ui.text_edit_singleline(&mut self.map_filters.mime_prefix)
                                .on_hover_text("Prefix like image/");
                            if ui.button("Refresh map").clicked() {
                                self.refresh_map();
                            }
                            if ui.button("Clear").clicked() {
                                self.map_filters = MapFilters::default();
                            }
                            if self.map_in_flight {
                                ui.label("Processing GPS cache...");
                            }
                            if !self.map_status.is_empty() {
                                ui.label(self.map_status.clone());
                            }
                        });
                        ui.separator();

                        if self.map_points.is_empty() {
                            ui.label("No geotagged media found (EXIF GPS required).");
                        } else {
                            ui.horizontal(|ui| {
                                let desired_size = egui::vec2(640.0, 360.0);
                                let (rect, _) =
                                    ui.allocate_exact_size(desired_size, egui::Sense::hover());
                                let painter = ui.painter_at(rect);
                                painter.rect_filled(rect, 6.0, egui::Color32::from_gray(245));

                                let mut lat_min = self.map_points[0].lat;
                                let mut lat_max = self.map_points[0].lat;
                                let mut lon_min = self.map_points[0].lon;
                                let mut lon_max = self.map_points[0].lon;
                                for point in &self.map_points {
                                    lat_min = lat_min.min(point.lat);
                                    lat_max = lat_max.max(point.lat);
                                    lon_min = lon_min.min(point.lon);
                                    lon_max = lon_max.max(point.lon);
                                }

                                let zoom = choose_map_zoom(
                                    lat_min,
                                    lat_max,
                                    lon_min,
                                    lon_max,
                                    rect.width(),
                                    rect.height(),
                                );
                                let center_lat = (lat_min + lat_max) * 0.5;

                                let center_lon = (lon_min + lon_max) * 0.5;
                                let (center_x, center_y) = lonlat_to_pixel(center_lat, center_lon, zoom);
                                let top_left_x = center_x - rect.width() as f64 / 2.0;
                                let top_left_y = center_y - rect.height() as f64 / 2.0;
                                let tile_size = 256.0;
                                let tiles_per_side = 2i32.pow(zoom as u32);
                                let x_start = (top_left_x / tile_size).floor() as i32;
                                let y_start = (top_left_y / tile_size).floor() as i32;
                                let x_end = ((top_left_x + rect.width() as f64) / tile_size).floor() as i32;
                                let y_end = ((top_left_y + rect.height() as f64) / tile_size).floor() as i32;

                                for ty in y_start..=y_end {
                                    if ty < 0 || ty >= tiles_per_side {
                                        continue;
                                    }
                                    for tx in x_start..=x_end {
                                        let wrapped_x =
                                            ((tx % tiles_per_side) + tiles_per_side) % tiles_per_side;
                                        let key = MapTileKey {
                                            z: zoom,
                                            x: wrapped_x,
                                            y: ty,
                                        };
                                        let tile_left = rect.left()
                                            + ((tx as f64 * tile_size - top_left_x) as f32);
                                        let tile_top = rect.top()
                                            + ((ty as f64 * tile_size - top_left_y) as f32);
                                        let tile_rect = egui::Rect::from_min_size(
                                            egui::pos2(tile_left, tile_top),
                                            egui::vec2(tile_size as f32, tile_size as f32),
                                        );
                                        if let Some(texture) = self.map_tiles.get(&key) {
                                            painter.image(
                                                texture.id(),
                                                tile_rect,
                                                egui::Rect::from_min_max(
                                                    egui::pos2(0.0, 0.0),
                                                    egui::pos2(1.0, 1.0),
                                                ),
                                                egui::Color32::WHITE,
                                            );
                                        } else {
                                            self.request_map_tile(key);
                                            painter.rect_filled(
                                                tile_rect,
                                                0.0,
                                                egui::Color32::from_gray(220),
                                            );
                                        }
                                    }
                                }

                                for (idx, point) in self.map_points.iter().enumerate() {
                                    let (px, py) = lonlat_to_pixel(point.lat, point.lon, zoom);
                                    let x = rect.left() + ((px - top_left_x) as f32);
                                    let y = rect.top() + ((py - top_left_y) as f32);
                                    let pos = egui::pos2(x, y);
                                    if rect.contains(pos) {
                                        let color = if self.map_selected == Some(idx) {
                                            egui::Color32::from_rgb(30, 120, 230)
                                        } else {
                                            egui::Color32::from_rgb(220, 90, 90)
                                        };
                                        painter.circle_filled(pos, 4.0, color);
                                    }
                                }

                                ui.add_space(12.0);
                                ui.vertical(|ui| {
                                    ui.label("Geotagged items");
                                    egui::ScrollArea::vertical()
                                        .max_height(360.0)
                                        .show(ui, |ui| {
                                            for (idx, point) in self.map_points.iter().enumerate() {
                                                let label = format!(
                                                    "{} | {} | {}",
                                                    format_timestamp(point.timestamp),
                                                    point.address,
                                                    point.mime_type
                                                );
                                                if ui
                                                    .selectable_label(
                                                        self.map_selected == Some(idx),
                                                        label,
                                                    )
                                                    .clicked()
                                                {
                                                    self.map_selected = Some(idx);
                                                }
                                            }
                                        });
                                });
                            });

                            if let Some(idx) = self.map_selected {
                                if let Some(point) = self.map_points.get(idx) {
                                    ui.separator();
                                    ui.label(format!(
                                        "Selected: {} ({:.5}, {:.5})",
                                        point.address, point.lat, point.lon
                                    ));
                                    if let Some(path) = self.resolve_media_path(&point.file_path) {
                                        let thread_id = point.thread_id.clone();
                                        let message_id = point.message_id.clone();
                                        let address = point.address.clone();

                                        ui.horizontal(|ui| {
                                            if ui.button("Open image").clicked() {
                                                open_file(&path);
                                            }
                                            if ui.button("Open location").clicked() {
                                                open_file_location(&path);
                                            }
                                            if ui.button("Open thread").clicked() {
                                                let anchor =
                                                    uuid::Uuid::parse_str(&message_id).ok();
                                                self.open_thread_from_parts(
                                                    thread_id.as_ref(),
                                                    Some(&address),
                                                    anchor,
                                                );
                                                self.active_tab = AppTab::Search;
                                            }
                                        });
                                    }
                                }
                            }
                        }
                    });
            }

            if matches!(self.active_tab, AppTab::Logs) {
                if self.log_files.is_empty() {
                    self.refresh_log_files();
                }
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.heading("Logs");
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label("Filter:");
                            ui.text_edit_singleline(&mut self.log_filter);
                            ui.label("Max lines:");
                            ui.add(
                                egui::DragValue::new(&mut self.log_max_lines)
                                    .clamp_range(100..=100_000),
                            );
                            if ui.button("Refresh files").clicked() {
                                self.refresh_log_files();
                            }
                            if ui.button("Reload file").clicked() {
                                if let Some(path) = self.log_selected.clone() {
                                    self.load_log_file(path);
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Log file:");
                            let mut changed = false;
                            egui::ComboBox::from_id_source("log_files")
                                .selected_text(
                                    self.log_selected
                                        .as_ref()
                                        .and_then(|p| p.file_name())
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("Select log"),
                                )
                                .show_ui(ui, |ui| {
                                    for path in &self.log_files {
                                        let name = path
                                            .file_name()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("log");
                                        if ui
                                            .selectable_value(
                                                &mut self.log_selected,
                                                Some(path.clone()),
                                                name,
                                            )
                                            .clicked()
                                        {
                                            changed = true;
                                        }
                                    }
                                });
                            if ui.button("Browse").clicked() {
                                if let Some(path) = FileDialog::new().pick_file() {
                                    self.log_selected = Some(path.clone());
                                    self.load_log_file(path);
                                }
                            }
                            if changed {
                                if let Some(path) = self.log_selected.clone() {
                                    self.load_log_file(path);
                                }
                            }
                        });
                        if !self.log_status.is_empty() {
                            ui.label(self.log_status.clone());
                        }
                        ui.separator();
                        let filter = self.log_filter.to_lowercase();
                        egui::ScrollArea::vertical()
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                for line in &self.log_lines {
                                    if filter.is_empty()
                                        || line.to_lowercase().contains(&filter)
                                    {
                                        ui.label(line);
                                    }
                                }
                            });
                        // #todo: add regex filtering and log severity toggles.
                    });
            }
        });

        // Stuck import dialog
        if self.show_stuck_dialog {
            egui::Window::new("Import Stuck")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label("⚠ Import hasn't progressed in 30 seconds");
                    ui.add_space(8.0);
                    ui.label("The import appears to be stuck. This could be due to:");
                    ui.label("• A very large message or attachment");
                    ui.label("• A corrupted XML section");
                    ui.label("• System resource constraints");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Continue Waiting").clicked() {
                            // Reset the timer
                            self.import_last_update = Instant::now();
                            self.show_stuck_dialog = false;
                        }
                        if ui.button("Skip file").clicked() {
                            if let Some(job) = &self.import_job {
                                job.progress
                                    .skip_current_file
                                    .store(true, Ordering::Relaxed);
                            }
                            self.import_status =
                                "Skipping current file (continuing import queue)...".to_string();
                            self.import_last_update = Instant::now();
                            self.show_stuck_dialog = false;
                            // #todo: provide a per-file retry option when a file is skipped.
                        }
                        if ui.button("Cancel Import").clicked() {
                            if let Some(job) = &self.import_job {
                                job.progress.cancelled.store(true, Ordering::Relaxed);
                            }
                            self.show_stuck_dialog = false;
                        }
                    });
                });
        }

        self.maybe_persist_ui_settings();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist_ui_settings_now();
    }
}

impl SmsArchiveApp {
    fn apply_global_settings(&mut self, settings: Option<GlobalSettings>) {
        if let Some(settings) = settings {
            if let Some(value) = settings.clip_model_path {
                if self.clip_model_path.trim().is_empty() {
                    self.clip_model_path = value;
                }
            }
            if let Some(value) = settings.clip_nsfw_weights_path {
                if self.clip_nsfw_weights_path.trim().is_empty() {
                    self.clip_nsfw_weights_path = value;
                }
            }
            if let Some(value) = settings.clip_text_model_path {
                if self.clip_text_model_path.trim().is_empty() {
                    self.clip_text_model_path = value;
                }
            }
            if let Some(value) = settings.clip_text_tokenizer_path {
                if self.clip_text_tokenizer_path.trim().is_empty() {
                    self.clip_text_tokenizer_path = value;
                }
            }
            if let Some(value) = settings.media_embed_prompt {
                if self.media_embed_prompt.trim().is_empty() {
                    self.media_embed_prompt = value;
                }
            }
            if let Some(value) = settings.vision_prompt {
                if self.vision_prompt.trim().is_empty() {
                    self.vision_prompt = value;
                }
            }
            if let Some(value) = settings.tesseract_cmd {
                if self.tesseract_cmd.trim().is_empty() {
                    self.tesseract_cmd = value;
                }
            }
        }
    }

    fn apply_ui_settings(&mut self, settings: Option<UiSettings>) {
        if let Some(settings) = settings {
            self.active_tab = settings.active_tab;
            self.db_path = settings.db_path;
            self.db_folder = settings.db_folder.and_then(|value| {
                let trimmed = value.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(trimmed))
                }
            });
            self.new_db_name = settings.new_db_name;
            self.search_query = settings.search_query;
            self.search_filters = settings.search_filters;
            self.search_max_results = settings.search_max_results;
            self.search_unlimited = settings.search_unlimited;
            self.context_window_size = settings.context_window_size;
            self.show_perf = settings.show_perf;
            self.page_size = settings.page_size.max(1);
            self.page_offset = settings.page_offset;
            self.page_jump_input = settings.page_jump_input;
            self.semantic_query = settings.semantic_query;
            self.semantic_limit = settings.semantic_limit.max(1);
            self.import_input = settings.import_input;
            self.resume_from_checkpoint = settings.resume_from_checkpoint;
            self.checkpoint_db_path = settings.checkpoint_db_path;
            self.embed_model_path = settings.embed_model_path;
            self.embed_tokenizer_path = settings.embed_tokenizer_path;
            self.embed_model_name = settings.embed_model_name;
            self.embed_model_version = settings.embed_model_version;
            self.embed_dimensions = settings.embed_dimensions.max(1);
            self.embed_batch_size = settings.embed_batch_size.max(1);
            self.embed_max_length = settings.embed_max_length.max(8);
            self.embed_normalize = settings.embed_normalize;
            self.embed_device = device_from_string(&settings.embed_device);
            self.use_ollama = settings.use_ollama;
            self.ollama_base_url = settings.ollama_base_url;
            self.ollama_selected = settings.ollama_selected;
            self.ollama_pull_name = settings.ollama_pull_name;
            self.ollama_models_source = settings.ollama_models_source;
            self.assistant_base_url = settings.assistant_base_url;
            self.assistant_model = settings.assistant_model;
            self.assistant_input = settings.assistant_input;
            self.vision_base_url = settings.vision_base_url;
            self.vision_model = settings.vision_model;
            self.vision_prompt = settings.vision_prompt;
            self.tesseract_cmd = settings.tesseract_cmd;
            self.media_query = settings.media_query;
            self.media_nsfw_filter = settings.media_nsfw_filter;
            self.media_embed_prompt = settings.media_embed_prompt;
            self.media_embed_use_local = settings.media_embed_use_local;
            self.media_nsfw_prompt = settings.media_nsfw_prompt;
            self.media_nsfw_threshold = settings.media_nsfw_threshold;
            self.media_keyframe_max = settings.media_keyframe_max.max(1);
            self.media_semantic_query = settings.media_semantic_query;
            self.media_semantic_use_clip = settings.media_semantic_use_clip;
            self.media_semantic_limit = settings.media_semantic_limit.max(1);
            self.media_page_size = settings.media_page_size.max(1);
            self.media_page_offset = settings.media_page_offset;
            self.clip_model_path = settings.clip_model_path;
            self.clip_nsfw_weights_path = settings.clip_nsfw_weights_path;
            self.clip_batch_size = settings.clip_batch_size.max(1);
            self.clip_max_keyframes = settings.clip_max_keyframes.max(1);
            self.clip_workers = settings.clip_workers.max(1);
            self.clip_reprocess = settings.clip_reprocess;
            self.clip_auto_on_import = settings.clip_auto_on_import;
            self.clip_use_cuda = settings.clip_use_cuda;
            self.clip_text_model_path = settings.clip_text_model_path;
            self.clip_text_tokenizer_path = settings.clip_text_tokenizer_path;
            self.selected_model_id = settings.selected_model_id;
            self.thread_limit = settings.thread_limit.max(1);
            self.contact_search = settings.contact_search;
            self.self_addresses = settings.self_addresses;
            self.self_address_input = settings.self_address_input;
            self.timeline_filters = settings.timeline_filters;
            self.timeline_chart_mode = settings.timeline_chart_mode;
            self.timeline_name_query = settings.timeline_name_query;
            self.timeline_selected_addresses =
                settings.timeline_selected_addresses.into_iter().collect();
            self.map_filters = settings.map_filters;
            self.log_filter = settings.log_filter;
            self.log_max_lines = settings.log_max_lines.max(100);
            self.log_selected = settings.log_selected.map(PathBuf::from);
            if let Some(path) = self.log_selected.clone() {
                self.load_log_file(path);
            }

            self.ui_settings_snapshot =
                serde_json::to_string_pretty(&self.build_ui_settings()).unwrap_or_default();
            self.ui_settings_last_save = Instant::now();
        }
    }

    fn build_ui_settings(&self) -> UiSettings {
        UiSettings {
            version: 1,
            active_tab: self.active_tab,
            db_path: self.db_path.clone(),
            db_folder: self
                .db_folder
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            new_db_name: self.new_db_name.clone(),
            search_query: self.search_query.clone(),
            search_filters: self.search_filters.clone(),
            search_max_results: self.search_max_results.clone(),
            search_unlimited: self.search_unlimited,
            context_window_size: self.context_window_size.clone(),
            show_perf: self.show_perf,
            page_size: self.page_size,
            page_offset: self.page_offset,
            page_jump_input: self.page_jump_input.clone(),
            semantic_query: self.semantic_query.clone(),
            semantic_limit: self.semantic_limit,
            import_input: self.import_input.clone(),
            resume_from_checkpoint: self.resume_from_checkpoint,
            checkpoint_db_path: self.checkpoint_db_path.clone(),
            embed_model_path: self.embed_model_path.clone(),
            embed_tokenizer_path: self.embed_tokenizer_path.clone(),
            embed_model_name: self.embed_model_name.clone(),
            embed_model_version: self.embed_model_version.clone(),
            embed_dimensions: self.embed_dimensions,
            embed_batch_size: self.embed_batch_size,
            embed_max_length: self.embed_max_length,
            embed_normalize: self.embed_normalize,
            embed_device: device_to_string(self.embed_device),
            use_ollama: self.use_ollama,
            ollama_base_url: self.ollama_base_url.clone(),
            ollama_selected: self.ollama_selected.clone(),
            ollama_pull_name: self.ollama_pull_name.clone(),
            ollama_models_source: self.ollama_models_source.clone(),
            assistant_base_url: self.assistant_base_url.clone(),
            assistant_model: self.assistant_model.clone(),
            assistant_input: self.assistant_input.clone(),
            vision_base_url: self.vision_base_url.clone(),
            vision_model: self.vision_model.clone(),
            vision_prompt: self.vision_prompt.clone(),
            tesseract_cmd: self.tesseract_cmd.clone(),
            media_query: self.media_query.clone(),
            media_nsfw_filter: self.media_nsfw_filter,
            media_embed_prompt: self.media_embed_prompt.clone(),
            media_embed_use_local: self.media_embed_use_local,
            media_nsfw_prompt: self.media_nsfw_prompt.clone(),
            media_nsfw_threshold: self.media_nsfw_threshold,
            media_keyframe_max: self.media_keyframe_max,
            media_semantic_query: self.media_semantic_query.clone(),
            media_semantic_use_clip: self.media_semantic_use_clip,
            media_semantic_limit: self.media_semantic_limit,
            media_page_size: self.media_page_size,
            media_page_offset: self.media_page_offset,
            clip_model_path: self.clip_model_path.clone(),
            clip_nsfw_weights_path: self.clip_nsfw_weights_path.clone(),
            clip_batch_size: self.clip_batch_size,
            clip_max_keyframes: self.clip_max_keyframes,
            clip_workers: self.clip_workers,
            clip_reprocess: self.clip_reprocess,
            clip_auto_on_import: self.clip_auto_on_import,
            clip_use_cuda: self.clip_use_cuda,
            clip_text_model_path: self.clip_text_model_path.clone(),
            clip_text_tokenizer_path: self.clip_text_tokenizer_path.clone(),
            selected_model_id: self.selected_model_id.clone(),
            thread_limit: self.thread_limit,
            contact_search: self.contact_search.clone(),
            self_addresses: self.self_addresses.clone(),
            self_address_input: self.self_address_input.clone(),
            timeline_filters: self.timeline_filters.clone(),
            timeline_chart_mode: self.timeline_chart_mode,
            timeline_name_query: self.timeline_name_query.clone(),
            timeline_selected_addresses: self.timeline_selected_addresses.iter().cloned().collect(),
            map_filters: self.map_filters.clone(),
            log_filter: self.log_filter.clone(),
            log_max_lines: self.log_max_lines,
            log_selected: self
                .log_selected
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
        }
    }

    fn maybe_persist_ui_settings(&mut self) {
        if self.ui_settings_last_save.elapsed() < Duration::from_secs(2) {
            return;
        }
        let settings = self.build_ui_settings();
        let serialized = match serde_json::to_string_pretty(&settings) {
            Ok(data) => data,
            Err(err) => {
                self.status = format!("Failed to serialize UI settings: {}", err);
                self.ui_settings_last_save = Instant::now();
                return;
            }
        };
        if serialized == self.ui_settings_snapshot {
            self.ui_settings_last_save = Instant::now();
            return;
        }
        if let Err(err) = persist_ui_settings_raw(&serialized) {
            self.status = format!("Failed to save UI settings: {}", err);
        } else {
            self.ui_settings_snapshot = serialized;
        }
        self.ui_settings_last_save = Instant::now();
        // #todo: expose UI settings auto-save interval in advanced settings.
    }

    fn persist_ui_settings_now(&mut self) {
        let settings = self.build_ui_settings();
        let serialized = match serde_json::to_string_pretty(&settings) {
            Ok(data) => data,
            Err(err) => {
                self.status = format!("Failed to serialize UI settings: {}", err);
                return;
            }
        };
        if let Err(err) = persist_ui_settings_raw(&serialized) {
            self.status = format!("Failed to save UI settings: {}", err);
        } else {
            self.ui_settings_snapshot = serialized;
            self.ui_settings_last_save = Instant::now();
        }
        // #todo: emit a one-shot toast when UI settings are persisted on exit.
    }

    fn save_global_settings(&mut self) {
        let settings = GlobalSettings {
            clip_model_path: Some(self.clip_model_path.trim().to_string()),
            clip_nsfw_weights_path: Some(self.clip_nsfw_weights_path.trim().to_string()),
            clip_text_model_path: Some(self.clip_text_model_path.trim().to_string()),
            clip_text_tokenizer_path: Some(self.clip_text_tokenizer_path.trim().to_string()),
            media_embed_prompt: Some(self.media_embed_prompt.trim().to_string()),
            vision_prompt: Some(self.vision_prompt.trim().to_string()),
            tesseract_cmd: Some(self.tesseract_cmd.trim().to_string()),
        };
        if let Err(err) = persist_global_settings(&settings) {
            self.status = format!("Failed to save global settings: {}", err);
        } else {
            self.status = "Global settings saved".to_string();
        }
    }
    fn open_db(&mut self) {
        if self.db_path.trim().is_empty() {
            self.status = "Enter a DB path".to_string();
            return;
        }
        let path = std::path::Path::new(&self.db_path);
        if path.is_dir() {
            self.status = "Select or create a DB file (folder selected)".to_string();
            return;
        }

        match Database::open(path, ResourceProfile::detect()) {
            Ok(db) => {
                if self.log_guard.is_none() {
                    let log_dir = std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .join("logs");
                    if let Ok(guard) = init_logging(&log_dir) {
                        self.log_guard = Some(guard);
                    }
                }
                self.db_folder = path.parent().map(PathBuf::from);
                let conn = db.connection();
                let count: i64 = conn
                    .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
                    .unwrap_or(0);
                self.message_count = count;
                self.search_backend = Fts5Backend::open(
                    std::path::Path::new(&self.db_path),
                    ResourceProfile::detect(),
                )
                .ok();
                self.results.clear();
                self.selected = None;
                self.selected_attachments.clear();
                self.status = "Opened".to_string();
                self.refresh_checkpoint();
                self.load_self_addresses();
                self.load_media_root();
                self.load_llm_settings();
                self.load_clip_settings();
                self.refresh_contacts();
                self.load_media_page();
            }
            Err(e) => {
                self.status = format!("Error: {}", e);
            }
        }
    }

    fn create_new_db(&mut self) {
        let folder = match &self.db_folder {
            Some(folder) => folder.clone(),
            None => {
                self.status = "Choose a folder first".to_string();
                return;
            }
        };
        let mut name = self.new_db_name.trim().to_string();
        if name.is_empty() {
            name = "sms_archive.db".to_string();
        }
        let path = folder.join(name);
        self.db_path = path.display().to_string();
        self.open_db();
    }

    fn refresh_contacts(&mut self) {
        if self.db_path.trim().is_empty() {
            self.contact_status = "Open a database first".to_string();
            return;
        }
        if self.contacts_in_flight {
            return;
        }
        self.contacts_in_flight = true;
        let db_path = self.db_path.clone();
        let query = self.contact_search.trim().to_string();
        let pending = Arc::clone(&self.pending_contacts);
        std::thread::spawn(move || {
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(load_contact_snapshot(&db_path, &query));
            }
        });
    }

    fn load_self_addresses(&mut self) {
        if self.db_path.trim().is_empty() {
            return;
        }
        let db = match Database::open(
            std::path::Path::new(&self.db_path),
            ResourceProfile::detect(),
        ) {
            Ok(db) => db,
            Err(_) => return,
        };
        let conn = db.connection();
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = 'self_addresses'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap_or(None);
        if let Some(value) = value {
            if let Ok(list) = serde_json::from_str::<Vec<String>>(&value) {
                self.self_addresses = list;
            }
        }
    }

    fn load_media_root(&mut self) {
        self.media_root = None;
        let db_path = self.db_path.trim();
        if db_path.is_empty() {
            return;
        }
        let default_dir = resolve_media_dir(db_path);
        let db = match Database::open(std::path::Path::new(db_path), ResourceProfile::detect()) {
            Ok(db) => db,
            Err(_) => {
                self.media_root = Some(default_dir);
                return;
            }
        };
        let conn = db.connection();
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = 'media_dir'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap_or(None);
        if let Some(value) = value {
            self.media_root = Some(PathBuf::from(value));
            return;
        }
        self.media_root = Some(default_dir.clone());
        if default_dir.exists() {
            self.save_media_root(&default_dir);
        }
    }

    fn save_media_root(&mut self, path: &Path) {
        if self.db_path.trim().is_empty() {
            return;
        }
        let db = match Database::open(
            std::path::Path::new(&self.db_path),
            ResourceProfile::detect(),
        ) {
            Ok(db) => db,
            Err(_) => return,
        };
        let conn = db.connection();
        let value = path.to_string_lossy().to_string();
        let _ = conn.execute(
            "INSERT INTO app_settings (key, value) VALUES ('media_dir', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![value],
        );
    }

    fn load_llm_settings(&mut self) {
        if self.db_path.trim().is_empty() {
            return;
        }
        let db = match Database::open(
            std::path::Path::new(&self.db_path),
            ResourceProfile::detect(),
        ) {
            Ok(db) => db,
            Err(_) => return,
        };
        let conn = db.connection();
        let load_key = |conn: &rusqlite::Connection, key: &str| -> Option<String> {
            conn.query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .unwrap_or(None)
        };
        if let Some(value) = load_key(conn, "assistant_base_url") {
            self.assistant_base_url = value;
        }
        if let Some(value) = load_key(conn, "assistant_model") {
            self.assistant_model = value;
        }
        if let Some(value) = load_key(conn, "vision_base_url") {
            self.vision_base_url = value;
        }
        if let Some(value) = load_key(conn, "vision_model") {
            self.vision_model = value;
        }
        if let Some(value) = load_key(conn, "vision_prompt") {
            self.vision_prompt = value;
        }
        if let Some(value) = load_key(conn, "tesseract_cmd") {
            self.tesseract_cmd = value;
        }
        self.assistant.ollama_url = self.assistant_base_url.clone();
        self.assistant.model = self.assistant_model.clone();
    }

    fn load_clip_settings(&mut self) {
        if self.db_path.trim().is_empty() {
            return;
        }
        let db = match Database::open(
            std::path::Path::new(&self.db_path),
            ResourceProfile::detect(),
        ) {
            Ok(db) => db,
            Err(_) => return,
        };
        let conn = db.connection();
        let load_key = |conn: &rusqlite::Connection, key: &str| -> Option<String> {
            conn.query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .unwrap_or(None)
        };
        let parse_bool = |value: &str| -> bool {
            matches!(value.trim().to_lowercase().as_str(), "1" | "true" | "yes")
        };
        if let Some(value) = load_key(conn, "clip_model_path") {
            self.clip_model_path = value;
        }
        if let Some(value) = load_key(conn, "clip_nsfw_weights_path") {
            self.clip_nsfw_weights_path = value;
        }
        if let Some(value) = load_key(conn, "clip_batch_size") {
            if let Ok(parsed) = value.parse::<usize>() {
                self.clip_batch_size = parsed.max(1);
            }
        }
        if let Some(value) = load_key(conn, "clip_max_keyframes") {
            if let Ok(parsed) = value.parse::<usize>() {
                self.clip_max_keyframes = parsed.max(1);
            }
        }
        if let Some(value) = load_key(conn, "clip_workers") {
            if let Ok(parsed) = value.parse::<usize>() {
                self.clip_workers = parsed.max(1);
            }
        }
        if let Some(value) = load_key(conn, "clip_reprocess") {
            self.clip_reprocess = parse_bool(&value);
        }
        if let Some(value) = load_key(conn, "clip_auto_on_import") {
            self.clip_auto_on_import = parse_bool(&value);
        }
        if let Some(value) = load_key(conn, "clip_use_cuda") {
            self.clip_use_cuda = parse_bool(&value);
        }
        self.autofill_clip_paths();
    }

    fn save_llm_settings(&mut self) {
        if self.db_path.trim().is_empty() {
            return;
        }
        let db = match Database::open(
            std::path::Path::new(&self.db_path),
            ResourceProfile::detect(),
        ) {
            Ok(db) => db,
            Err(_) => return,
        };
        let conn = db.connection();
        let _ = conn.execute(
            "INSERT INTO app_settings (key, value) VALUES ('assistant_base_url', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![self.assistant_base_url.clone()],
        );
        let _ = conn.execute(
            "INSERT INTO app_settings (key, value) VALUES ('assistant_model', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![self.assistant_model.clone()],
        );
        let _ = conn.execute(
            "INSERT INTO app_settings (key, value) VALUES ('vision_base_url', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![self.vision_base_url.clone()],
        );
        let _ = conn.execute(
            "INSERT INTO app_settings (key, value) VALUES ('vision_model', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![self.vision_model.clone()],
        );
        let _ = conn.execute(
            "INSERT INTO app_settings (key, value) VALUES ('vision_prompt', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![self.vision_prompt.clone()],
        );
        if !self.tesseract_cmd.trim().is_empty() {
            let _ = conn.execute(
                "INSERT INTO app_settings (key, value) VALUES ('tesseract_cmd', ?1) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![self.tesseract_cmd.clone()],
            );
        }
    }

    fn save_clip_settings(&mut self) {
        if self.db_path.trim().is_empty() {
            return;
        }
        let db = match Database::open(
            std::path::Path::new(&self.db_path),
            ResourceProfile::detect(),
        ) {
            Ok(db) => db,
            Err(_) => return,
        };
        let conn = db.connection();
        let save_key = |conn: &rusqlite::Connection, key: &str, value: &str| {
            let _ = conn.execute(
                "INSERT INTO app_settings (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            );
        };
        save_key(conn, "clip_model_path", self.clip_model_path.trim());
        save_key(
            conn,
            "clip_nsfw_weights_path",
            self.clip_nsfw_weights_path.trim(),
        );
        save_key(conn, "clip_batch_size", &self.clip_batch_size.to_string());
        save_key(
            conn,
            "clip_max_keyframes",
            &self.clip_max_keyframes.to_string(),
        );
        save_key(conn, "clip_workers", &self.clip_workers.to_string());
        save_key(conn, "clip_reprocess", &self.clip_reprocess.to_string());
        save_key(
            conn,
            "clip_auto_on_import",
            &self.clip_auto_on_import.to_string(),
        );
        save_key(conn, "clip_use_cuda", &self.clip_use_cuda.to_string());
    }

    fn autofill_clip_paths(&mut self) {
        let model_candidates = [
            "ml/CLIP1/vision_model_fp16.onnx",
            "ml/clip-vit-l-14.onnx",
            "ml/clip-vit-l-14-336.onnx",
        ];
        // Prefer the Marqo image classifier (scripts/setup_marqo_nsfw.py),
        // fall back to the LAION embedding head. The old ml/nsfw_probe.npz
        // was a hidden-layer dump, not a classifier, and was removed.
        let nsfw_candidates = ["ml/nsfw_marqo_384.onnx", "ml/nsfw_classifier.onnx"];

        let mut changed = false;
        if self.clip_model_path.trim().is_empty()
            || !Path::new(self.clip_model_path.trim()).exists()
        {
            if let Some(path) = pick_existing_relative(&model_candidates) {
                self.clip_model_path = path.display().to_string();
                changed = true;
            }
        }
        if self.clip_nsfw_weights_path.trim().is_empty()
            || !Path::new(self.clip_nsfw_weights_path.trim()).exists()
        {
            if let Some(path) = pick_existing_relative(&nsfw_candidates) {
                self.clip_nsfw_weights_path = path.display().to_string();
                changed = true;
            }
        }
        if changed && !self.db_path.trim().is_empty() {
            self.save_clip_settings();
        }
    }

    fn resolve_media_path(&self, rel_path: &str) -> Option<PathBuf> {
        if rel_path.trim().is_empty() {
            return None;
        }
        let raw = Path::new(rel_path);
        if raw.is_absolute() {
            return Some(raw.to_path_buf());
        }
        let root = self.media_root_dir();
        resolve_media_path_candidates(&root, rel_path)
            .into_iter()
            .find(|p| p.exists())
            .or_else(|| {
                resolve_media_path_candidates(&root, rel_path)
                    .into_iter()
                    .next()
            })
    }

    fn media_root_dir(&self) -> PathBuf {
        self.media_root
            .as_ref()
            .cloned()
            .unwrap_or_else(|| resolve_media_dir(&self.db_path))
    }

    fn save_self_addresses(&mut self) {
        if self.db_path.trim().is_empty() {
            return;
        }
        let db = match Database::open(
            std::path::Path::new(&self.db_path),
            ResourceProfile::detect(),
        ) {
            Ok(db) => db,
            Err(_) => return,
        };
        let conn = db.connection();
        if let Ok(value) = serde_json::to_string(&self.self_addresses) {
            let _ = conn.execute(
                "INSERT INTO app_settings (key, value) VALUES ('self_addresses', ?1) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![value],
            );
        }
    }

    fn load_contact_detail(&mut self, contact_id: &str) {
        let db_path = self.db_path.clone();
        let contact_id = contact_id.to_string();
        let pending = Arc::clone(&self.pending_contact_detail);
        std::thread::spawn(move || {
            let db = match Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())
            {
                Ok(db) => db,
                Err(_) => return,
            };
            let conn = db.connection();
            let mut stmt = match conn.prepare(
                "SELECT id, display_name, nickname, company, notes, email, phone_primary, \
                        phone_secondary, phone_primary_type, phone_secondary_type, website, social_media, \
                        address, birthday, avatar_path, last_contacted, favorite \
                 FROM contacts WHERE id = ?1",
            ) {
                Ok(stmt) => stmt,
                Err(_) => return,
            };
            let detail = stmt.query_row(params![contact_id], |row| {
                Ok(ContactDetail {
                    id: row.get(0)?,
                    display_name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    nickname: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    company: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    notes: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    email: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    phone_primary: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    phone_secondary: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                    phone_primary_type: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
                    phone_secondary_type: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                    website: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
                    social_media: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
                    address: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
                    birthday: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
                    avatar_path: row.get::<_, Option<String>>(14)?.unwrap_or_default(),
                    last_contacted: row.get::<_, Option<i64>>(15)?,
                    favorite: row.get::<_, Option<i64>>(16)?.unwrap_or(0) != 0,
                    addresses: Vec::new(),
                })
            });
            let mut detail = match detail {
                Ok(d) => d,
                Err(_) => return,
            };
            if let Ok(mut stmt) = conn.prepare(
                "SELECT address FROM contact_addresses WHERE contact_id = ?1 ORDER BY address",
            ) {
                if let Ok(rows) = stmt.query_map(params![detail.id.clone()], |row| row.get(0)) {
                    for row in rows.flatten() {
                        detail.addresses.push(row);
                    }
                }
            }
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(detail);
            }
        });
    }

    fn open_contact_for_address(&mut self, address: &str) {
        if self.db_path.trim().is_empty() {
            self.status = "Open a database first".to_string();
            return;
        }
        let addr = address.trim();
        if addr.is_empty() {
            self.status = "No sender available".to_string();
            return;
        }
        let contact_id = match lookup_contact_id_by_address(&self.db_path, addr) {
            Some(id) => id,
            None => {
                self.status = "No contact found for sender".to_string();
                return;
            }
        };
        self.selected_contact_id = Some(contact_id.clone());
        self.load_contact_detail(&contact_id);
        self.active_tab = AppTab::Contacts;
    }

    fn new_contact(&mut self) {
        let id = uuid::Uuid::new_v4().to_string();
        self.selected_contact_id = Some(id.clone());
        self.contact_detail = Some(ContactDetail {
            id,
            display_name: String::new(),
            nickname: String::new(),
            company: String::new(),
            notes: String::new(),
            email: String::new(),
            phone_primary: String::new(),
            phone_secondary: String::new(),
            phone_primary_type: "mobile".to_string(),
            phone_secondary_type: "home".to_string(),
            website: String::new(),
            social_media: String::new(),
            address: String::new(),
            birthday: String::new(),
            avatar_path: String::new(),
            last_contacted: None,
            favorite: false,
            addresses: Vec::new(),
        });
    }

    fn save_contact(&mut self) {
        let detail = match &self.contact_detail {
            Some(detail) => detail.clone(),
            None => return,
        };
        let db_path = self.db_path.clone();
        let mut addresses = detail.addresses.clone();
        if !detail.phone_primary.is_empty() {
            addresses.push(detail.phone_primary.clone());
        }
        if !detail.phone_secondary.is_empty() {
            addresses.push(detail.phone_secondary.clone());
        }
        if !detail.address.is_empty() {
            addresses.push(detail.address.clone());
        }
        addresses.sort();
        addresses.dedup();
        let pending = Arc::clone(&self.pending_contact_detail);
        self.contact_status = "Saving contact...".to_string();
        std::thread::spawn(move || {
            let db = match Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())
            {
                Ok(db) => db,
                Err(_) => return,
            };
            let conn = db.connection();
            let _ = conn.execute(
                "INSERT INTO contacts (id, display_name, nickname, company, notes, email, \
                        phone_primary, phone_secondary, phone_primary_type, phone_secondary_type, \
                        website, social_media, address, birthday, avatar_path, last_contacted, favorite, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, strftime('%s','now')) \
                 ON CONFLICT(id) DO UPDATE SET \
                    display_name=excluded.display_name, \
                    nickname=excluded.nickname, \
                    company=excluded.company, \
                    notes=excluded.notes, \
                    email=excluded.email, \
                    phone_primary=excluded.phone_primary, \
                    phone_secondary=excluded.phone_secondary, \
                    phone_primary_type=excluded.phone_primary_type, \
                    phone_secondary_type=excluded.phone_secondary_type, \
                    website=excluded.website, \
                    social_media=excluded.social_media, \
                    address=excluded.address, \
                    birthday=excluded.birthday, \
                    avatar_path=excluded.avatar_path, \
                    last_contacted=excluded.last_contacted, \
                    favorite=excluded.favorite, \
                    updated_at=strftime('%s','now')",
                params![
                    detail.id,
                    detail.display_name,
                    nullable(detail.nickname.clone()),
                    nullable(detail.company.clone()),
                    nullable(detail.notes.clone()),
                    nullable(detail.email.clone()),
                    nullable(detail.phone_primary.clone()),
                    nullable(detail.phone_secondary.clone()),
                    nullable(detail.phone_primary_type.clone()),
                    nullable(detail.phone_secondary_type.clone()),
                    nullable(detail.website.clone()),
                    nullable(detail.social_media.clone()),
                    nullable(detail.address.clone()),
                    nullable(detail.birthday.clone()),
                    nullable(detail.avatar_path.clone()),
                    detail.last_contacted,
                    if detail.favorite { 1 } else { 0 },
                ],
            );
            let _ = conn.execute(
                "DELETE FROM contact_addresses WHERE contact_id = ?1",
                params![detail.id],
            );
            for addr in addresses {
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO contact_addresses (id, contact_id, address) VALUES (?1, ?2, ?3)",
                    params![uuid::Uuid::new_v4().to_string(), detail.id, addr],
                );
            }
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(detail);
            }
        });
        self.refresh_contacts();
    }

    fn delete_contact(&mut self) {
        let contact_id = match &self.selected_contact_id {
            Some(id) => id.clone(),
            None => return,
        };
        let db_path = self.db_path.clone();
        self.contact_status = "Deleting contact...".to_string();
        std::thread::spawn(move || {
            if let Ok(db) =
                Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())
            {
                let conn = db.connection();
                let _ = conn.execute("DELETE FROM contacts WHERE id = ?1", params![contact_id]);
            }
        });
        self.selected_contact_id = None;
        self.contact_detail = None;
        self.refresh_contacts();
    }

    fn merge_contacts(&mut self) {
        let target = match &self.selected_contact_id {
            Some(id) => id.clone(),
            None => return,
        };
        let source = match &self.contact_merge_source {
            Some(id) => id.clone(),
            None => return,
        };
        if target == source {
            return;
        }
        let db_path = self.db_path.clone();
        self.contact_status = "Merging contacts...".to_string();
        std::thread::spawn(move || {
            if let Ok(db) =
                Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())
            {
                let conn = db.connection();
                if let Ok(mut stmt) =
                    conn.prepare("SELECT address FROM contact_addresses WHERE contact_id = ?1")
                {
                    if let Ok(rows) =
                        stmt.query_map(params![source.clone()], |row| row.get::<_, String>(0))
                    {
                        for row in rows.flatten() {
                            let _ = conn.execute(
                                "INSERT OR IGNORE INTO contact_addresses (id, contact_id, address) VALUES (?1, ?2, ?3)",
                                params![uuid::Uuid::new_v4().to_string(), target.clone(), row],
                            );
                        }
                    }
                }
                let _ = conn.execute(
                    "DELETE FROM contact_addresses WHERE contact_id = ?1",
                    params![source.clone()],
                );
                let _ = conn.execute("DELETE FROM contacts WHERE id = ?1", params![source]);
            }
        });
        self.contact_merge_source = None;
        self.refresh_contacts();
    }

    fn prepare_merge_state(&mut self) {
        let target_id = match &self.selected_contact_id {
            Some(id) => id.clone(),
            None => return,
        };
        let source_id = match &self.contact_merge_source {
            Some(id) => id.clone(),
            None => return,
        };
        if target_id == source_id {
            return;
        }
        let target = match load_contact_detail_sync(&self.db_path, &target_id) {
            Some(d) => d,
            None => return,
        };
        let source = match load_contact_detail_sync(&self.db_path, &source_id) {
            Some(d) => d,
            None => return,
        };
        self.contact_merge_state = Some(ContactMergeState {
            target,
            source,
            name: MergeChoice::Target,
            nickname: MergeChoice::Target,
            company: MergeChoice::Target,
            notes: MergeChoice::Target,
            email: MergeChoice::Target,
            phone_primary: MergeChoice::Target,
            phone_secondary: MergeChoice::Target,
            phone_primary_type: MergeChoice::Target,
            phone_secondary_type: MergeChoice::Target,
            website: MergeChoice::Target,
            social_media: MergeChoice::Target,
            address: MergeChoice::Target,
            birthday: MergeChoice::Target,
            avatar_path: MergeChoice::Target,
            last_contacted: MergeChoice::Target,
            favorite: MergeChoice::Target,
        });
    }

    fn apply_merge_state(&mut self) {
        let state = match &self.contact_merge_state {
            Some(state) => state.clone(),
            None => return,
        };
        let db_path = self.db_path.clone();
        let target_id = state.target.id.clone();
        let source_id = state.source.id.clone();
        let target_for_ui = target_id.clone();
        std::thread::spawn(move || {
            if let Ok(db) =
                Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())
            {
                let conn = db.connection();
                let merged = ContactDetail {
                    id: target_id.clone(),
                    display_name: pick_merge(
                        &state.target.display_name,
                        &state.source.display_name,
                        state.name,
                    ),
                    nickname: pick_merge(
                        &state.target.nickname,
                        &state.source.nickname,
                        state.nickname,
                    ),
                    company: pick_merge(
                        &state.target.company,
                        &state.source.company,
                        state.company,
                    ),
                    notes: pick_merge(&state.target.notes, &state.source.notes, state.notes),
                    email: pick_merge(&state.target.email, &state.source.email, state.email),
                    phone_primary: pick_merge(
                        &state.target.phone_primary,
                        &state.source.phone_primary,
                        state.phone_primary,
                    ),
                    phone_secondary: pick_merge(
                        &state.target.phone_secondary,
                        &state.source.phone_secondary,
                        state.phone_secondary,
                    ),
                    phone_primary_type: pick_merge(
                        &state.target.phone_primary_type,
                        &state.source.phone_primary_type,
                        state.phone_primary_type,
                    ),
                    phone_secondary_type: pick_merge(
                        &state.target.phone_secondary_type,
                        &state.source.phone_secondary_type,
                        state.phone_secondary_type,
                    ),
                    website: pick_merge(
                        &state.target.website,
                        &state.source.website,
                        state.website,
                    ),
                    social_media: pick_merge(
                        &state.target.social_media,
                        &state.source.social_media,
                        state.social_media,
                    ),
                    address: pick_merge(
                        &state.target.address,
                        &state.source.address,
                        state.address,
                    ),
                    birthday: pick_merge(
                        &state.target.birthday,
                        &state.source.birthday,
                        state.birthday,
                    ),
                    avatar_path: pick_merge(
                        &state.target.avatar_path,
                        &state.source.avatar_path,
                        state.avatar_path,
                    ),
                    last_contacted: match state.last_contacted {
                        MergeChoice::Target => state.target.last_contacted,
                        MergeChoice::Source => state.source.last_contacted,
                    }
                    .or(state.target.last_contacted)
                    .or(state.source.last_contacted),
                    favorite: match state.favorite {
                        MergeChoice::Target => state.target.favorite,
                        MergeChoice::Source => state.source.favorite,
                    },
                    addresses: Vec::new(),
                };
                let _ = conn.execute(
                    "UPDATE contacts SET display_name = ?1, nickname = ?2, company = ?3, notes = ?4, \
                            email = ?5, phone_primary = ?6, phone_secondary = ?7, phone_primary_type = ?8, \
                            phone_secondary_type = ?9, website = ?10, social_media = ?11, address = ?12, \
                            birthday = ?13, avatar_path = ?14, last_contacted = ?15, favorite = ?16, \
                            updated_at = strftime('%s','now') \
                     WHERE id = ?17",
                    params![
                        merged.display_name,
                        nullable(merged.nickname),
                        nullable(merged.company),
                        nullable(merged.notes),
                        nullable(merged.email),
                        nullable(merged.phone_primary),
                        nullable(merged.phone_secondary),
                        nullable(merged.phone_primary_type),
                        nullable(merged.phone_secondary_type),
                        nullable(merged.website),
                        nullable(merged.social_media),
                        nullable(merged.address),
                        nullable(merged.birthday),
                        nullable(merged.avatar_path),
                        merged.last_contacted,
                        if merged.favorite { 1 } else { 0 },
                        target_id.clone(),
                    ],
                );
                if let Ok(mut stmt) =
                    conn.prepare("SELECT address FROM contact_addresses WHERE contact_id = ?1")
                {
                    if let Ok(rows) =
                        stmt.query_map(params![source_id.clone()], |row| row.get::<_, String>(0))
                    {
                        for row in rows.flatten() {
                            let _ = conn.execute(
                                "INSERT OR IGNORE INTO contact_addresses (id, contact_id, address) VALUES (?1, ?2, ?3)",
                                params![uuid::Uuid::new_v4().to_string(), target_id.clone(), row],
                            );
                        }
                    }
                }
                let _ = conn.execute(
                    "DELETE FROM contact_addresses WHERE contact_id = ?1",
                    params![source_id.clone()],
                );
                let _ = conn.execute("DELETE FROM contacts WHERE id = ?1", params![source_id]);
            }
        });
        self.contact_merge_state = None;
        self.contact_merge_source = None;
        self.refresh_contacts();
        self.load_contact_detail(&target_for_ui);
    }

    fn show_contact_media(&mut self) {
        let detail = match &self.contact_detail {
            Some(detail) => detail.clone(),
            None => return,
        };
        let mut addresses = detail.addresses.clone();
        if !detail.phone_primary.is_empty() {
            addresses.push(detail.phone_primary.clone());
        }
        if !detail.phone_secondary.is_empty() {
            addresses.push(detail.phone_secondary.clone());
        }
        if !detail.address.is_empty() {
            addresses.push(detail.address.clone());
        }
        addresses.sort();
        addresses.dedup();
        if addresses.is_empty() {
            self.contact_status = "No addresses to filter media".to_string();
            return;
        }
        self.active_tab = AppTab::Media;
        self.load_media_for_addresses(addresses);
    }

    fn load_media_for_addresses(&mut self, addresses: Vec<String>) {
        if self.media_in_flight {
            return;
        }
        self.media_in_flight = true;
        let db_path = self.db_path.clone();
        let pending = Arc::clone(&self.pending_media);
        std::thread::spawn(move || {
            let db = match Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())
            {
                Ok(db) => db,
                Err(_) => {
                    if let Ok(mut lock) = pending.lock() {
                        *lock = Some((Vec::new(), 0));
                    }
                    return;
                }
            };
            let conn = db.connection();
            let mut results = Vec::new();
            let mut placeholders = String::new();
            let mut params_vec: Vec<rusqlite::types::Value> = Vec::new();
            for (idx, addr) in addresses.iter().enumerate() {
                if idx > 0 {
                    placeholders.push_str(", ");
                }
                placeholders.push('?');
                params_vec.push(addr.clone().into());
            }
            let sql = format!(
                "SELECT attachments.id, attachments.mime_type, attachments.file_path, attachments.thumbnail_path, \
                        attachments.message_id, messages.thread_id, messages.timestamp, messages.address, \
                        attachments.ocr_text, attachments.ocr_model, attachments.ocr_timestamp, \
                        attachments.vision_analysis, attachments.vision_model, attachments.vision_timestamp, \
                        attachments.nsfw_label, attachments.nsfw_score, attachments.nsfw_model, attachments.nsfw_timestamp \
                 FROM attachments \
                 JOIN messages ON messages.id = attachments.message_id \
                 WHERE messages.address IN ({}) \
                 ORDER BY attachments.created_at DESC",
                placeholders
            );
            if let Ok(mut stmt) = conn.prepare(&sql) {
                let rows = stmt.query_map(rusqlite::params_from_iter(params_vec), |row| {
                    Ok(AttachmentRow {
                        id: row.get(0)?,
                        mime_type: row.get(1)?,
                        file_path: row.get(2)?,
                        thumbnail_path: row.get(3)?,
                        message_id: row.get(4)?,
                        thread_id: row.get(5)?,
                        timestamp: row.get(6)?,
                        address: row.get(7)?,
                        ocr_text: row.get(8)?,
                        ocr_model: row.get(9)?,
                        ocr_timestamp: row.get(10)?,
                        vision_analysis: row.get(11)?,
                        vision_model: row.get(12)?,
                        vision_timestamp: row.get(13)?,
                        nsfw_label: row.get(14)?,
                        nsfw_score: row.get(15)?,
                        nsfw_model: row.get(16)?,
                        nsfw_timestamp: row.get(17)?,
                    })
                });
                if let Ok(rows) = rows {
                    for row in rows.flatten() {
                        results.push(row);
                    }
                }
            }
            let total = results.len();
            if let Ok(mut lock) = pending.lock() {
                *lock = Some((results, total));
            }
        });
    }

    fn refresh_timeline(&mut self) {
        if self.db_path.trim().is_empty() {
            return;
        }
        if self.timeline_in_flight {
            return;
        }
        self.sync_timeline_address_filter();
        self.timeline_in_flight = true;
        let db_path = self.db_path.clone();
        let self_addrs = self.self_addresses.clone();
        let filters = self.timeline_filters.clone();
        let pending = Arc::clone(&self.pending_timeline);
        std::thread::spawn(move || {
            let stats = load_timeline_stats(&db_path, &self_addrs, &filters).unwrap_or_default();
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(stats);
            }
        });
    }

    fn sync_timeline_address_filter(&mut self) {
        if self.timeline_selected_addresses.is_empty() {
            self.timeline_filters.address.clear();
        } else {
            let mut list: Vec<String> = self.timeline_selected_addresses.iter().cloned().collect();
            list.sort();
            self.timeline_filters.address = list.join("|");
        }
    }

    fn refresh_map(&mut self) {
        if self.db_path.trim().is_empty() {
            self.map_status = "Open a database first".to_string();
            return;
        }
        if self.map_in_flight {
            return;
        }
        self.map_in_flight = true;
        self.map_status = "Tagging GPS media...".to_string();
        let db_path = self.db_path.clone();
        let filters = self.map_filters.clone();
        let media_root = self.media_root.clone();
        let pending = Arc::clone(&self.pending_map);
        std::thread::spawn(move || {
            let _ = tag_gps_cache(&db_path, media_root.clone());
            let points = load_map_points(&db_path, media_root, &filters).unwrap_or_default();
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(points);
            }
        });
    }

    fn request_map_tile(&mut self, key: MapTileKey) {
        if self.map_tiles.contains_key(&key) || self.map_tiles_in_flight.contains(&key) {
            return;
        }
        self.map_tiles_in_flight.insert(key);
        let pending = Arc::clone(&self.pending_map_tiles);
        std::thread::spawn(move || {
            let image = fetch_map_tile_image(key);
            if let Ok(mut lock) = pending.lock() {
                lock.push(MapTileUpdate { key, image });
            }
        });
    }

    fn refresh_log_files(&mut self) {
        let log_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("logs");
        let _ = fs::create_dir_all(&log_dir);
        let mut entries: Vec<(SystemTime, PathBuf)> = Vec::new();
        if let Ok(iter) = fs::read_dir(&log_dir) {
            for entry in iter.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if !name.starts_with("sms-archive.log") {
                    continue;
                }
                let modified = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                entries.push((modified, path));
            }
        }
        if entries.is_empty() {
            tracing::info!("log viewer refresh");
            if let Ok(iter) = fs::read_dir(&log_dir) {
                for entry in iter.flatten() {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    let name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    if !name.starts_with("sms-archive.log") {
                        continue;
                    }
                    let modified = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    entries.push((modified, path));
                }
            }
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0));
        self.log_files = entries.into_iter().map(|(_, path)| path).collect();
        if self.log_selected.is_none() {
            if let Some(path) = self.log_files.first().cloned() {
                self.log_selected = Some(path.clone());
                self.load_log_file(path);
            }
        }
        self.log_status = format!("Found {} log file(s)", self.log_files.len());
        if self.log_files.is_empty() {
            self.log_lines = vec!["No log files found in logs/ yet.".to_string()];
        }
    }

    fn load_log_file(&mut self, path: PathBuf) {
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(err) => {
                self.log_status = format!("Failed to open log: {}", err);
                return;
            }
        };
        let reader = std::io::BufReader::new(file);
        let mut lines: Vec<String> = reader.lines().map_while(std::result::Result::ok).collect();
        if lines.len() > self.log_max_lines {
            let start = lines.len().saturating_sub(self.log_max_lines);
            lines = lines.split_off(start);
        }
        self.log_lines = lines;
        self.log_selected = Some(path);
        self.log_status = format!("Loaded {} line(s)", self.log_lines.len());
    }

    fn send_assistant_message(&mut self) {
        if self.assistant_waiting {
            return;
        }
        if self.assistant_base_url.trim().is_empty() || self.assistant_model.trim().is_empty() {
            self.status = "Set assistant base URL and model first".to_string();
            return;
        }
        if self.db_path.trim().is_empty() {
            self.status = "Open a database first".to_string();
            return;
        }
        let user_msg = self.assistant_input.trim().to_string();
        if user_msg.is_empty() {
            return;
        }
        self.assistant_input.clear();
        if self.assistant.messages.is_empty() {
            self.assistant.messages.push(sms_assistant::ChatMessage::new(
                "system",
                "You are an SMS Archive assistant. Use the search_messages and get_thread tools for database queries. If a tool is needed, call it rather than guessing.".to_string(),
            ));
        }
        self.assistant.ollama_url = self.assistant_base_url.trim().to_string();
        self.assistant.model = self.assistant_model.trim().to_string();
        self.assistant
            .messages
            .push(sms_assistant::ChatMessage::new("user", user_msg));
        self.assistant_waiting = true;
        self.assistant_cancel.store(false, Ordering::Relaxed);
        if let Ok(mut buf) = self.assistant_stream.lock() {
            buf.clear();
        }
        let pending = Arc::clone(&self.pending_assistant);
        let stream = Arc::clone(&self.assistant_stream);
        let cancel = Arc::clone(&self.assistant_cancel);
        let messages = self.assistant.messages.clone();
        let assistant = self.assistant.clone();
        let db_path = self.db_path.clone();
        std::thread::spawn(move || {
            let result = assistant.complete_chat_streaming(messages, &db_path, &cancel, |delta| {
                if let Ok(mut buf) = stream.lock() {
                    buf.push_str(delta);
                }
            });
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(result);
            }
        });
    }

    fn import_contacts_from_messages(&mut self) {
        if self.db_path.trim().is_empty() {
            self.contact_status = "Open a database first".to_string();
            return;
        }
        let db_path = self.db_path.clone();
        self.contact_status = "Importing contacts from messages...".to_string();
        std::thread::spawn(move || {
            if let Ok(db) =
                Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())
            {
                let conn = db.connection();
                let mut stmt = match conn
                    .prepare("SELECT DISTINCT address FROM messages WHERE address != ''")
                {
                    Ok(stmt) => stmt,
                    Err(_) => return,
                };
                let rows = match stmt.query_map([], |row| row.get::<_, String>(0)) {
                    Ok(rows) => rows,
                    Err(_) => return,
                };
                for row in rows.flatten() {
                    let exists: Option<String> = conn
                        .query_row(
                            "SELECT contact_id FROM contact_addresses WHERE address = ?1",
                            params![row.clone()],
                            |r| r.get(0),
                        )
                        .optional()
                        .unwrap_or(None);
                    if exists.is_some() {
                        continue;
                    }
                    let id = uuid::Uuid::new_v4().to_string();
                    let _ = conn.execute(
                        "INSERT INTO contacts (id, display_name, phone_primary, updated_at) \
                         VALUES (?1, ?2, ?3, strftime('%s','now'))",
                        params![id, row.clone(), row.clone()],
                    );
                    let _ = conn.execute(
                        "INSERT INTO contact_addresses (id, contact_id, address) VALUES (?1, ?2, ?3)",
                        params![uuid::Uuid::new_v4().to_string(), id, row],
                    );
                }
            }
        });
        self.refresh_contacts();
    }

    fn import_contacts_from_xml_path(&mut self, xml_path: PathBuf) {
        if self.db_path.trim().is_empty() {
            self.contact_status = "Open a database first".to_string();
            return;
        }
        let db_path = self.db_path.clone();
        let query = self.contact_search.trim().to_string();
        let pending_status = Arc::clone(&self.pending_contact_status);
        let pending_contacts = Arc::clone(&self.pending_contacts);
        self.contact_status = "Importing contacts from XML...".to_string();
        std::thread::spawn(move || {
            let result = import_contacts_from_xml(&db_path, &xml_path);
            let status = match result {
                Ok(count) => {
                    let synced = sync_contact_names_from_xml(&db_path, &xml_path).unwrap_or(0);
                    format!(
                        "Imported {} contacts. Synced {} names from XML.",
                        count, synced
                    )
                }
                Err(e) => format!("XML contact import failed: {}", e),
            };
            if let Ok(mut lock) = pending_status.lock() {
                *lock = Some(status);
            }
            if let Ok(mut lock) = pending_contacts.lock() {
                *lock = Some(load_contact_snapshot(&db_path, &query));
            }
        });
    }

    fn import_contacts_from_xml(&mut self) {
        if self.db_path.trim().is_empty() {
            self.contact_status = "Open a database first".to_string();
            return;
        }
        let Some(path) = FileDialog::new().add_filter("XML", &["xml"]).pick_file() else {
            return;
        };
        self.import_contacts_from_xml_path(path);
    }

    fn test_contact_name_sync_workspace_xml(&mut self) {
        if self.db_path.trim().is_empty() {
            self.contact_status = "Open a database first".to_string();
            return;
        }
        let xml_path = std::path::PathBuf::from("docs/sms-20240321085442.xml");
        if !xml_path.exists() {
            self.contact_status = format!("Workspace XML not found: {}", xml_path.display());
            return;
        }
        let db_path = self.db_path.clone();
        let query = self.contact_search.trim().to_string();
        let pending_status = Arc::clone(&self.pending_contact_status);
        let pending_contacts = Arc::clone(&self.pending_contacts);
        self.contact_status = "Testing XML contact sync...".to_string();
        std::thread::spawn(move || {
            let status = match extract_contact_names_from_xml(&xml_path) {
                Ok(map) => {
                    let mut entries: Vec<(String, String)> = map.into_iter().collect();
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    let mut lines = Vec::new();
                    let sample = entries.iter().take(10);
                    for (addr, name) in sample {
                        lines.push(format!("{} -> {}", addr, name));
                    }
                    let elizabeth = entries
                        .iter()
                        .find(|(addr, _)| addr.contains("2147172243"))
                        .map(|(_, name)| name.clone())
                        .unwrap_or_else(|| "<not found>".to_string());
                    format!(
                        "Workspace XML sample (first 10):\n{}\nElizabeth Ti for 2147172243: {}",
                        lines.join("\n"),
                        elizabeth
                    )
                }
                Err(e) => format!("Workspace XML test failed: {}", e),
            };
            if let Ok(mut lock) = pending_status.lock() {
                *lock = Some(status);
            }
            if let Ok(mut lock) = pending_contacts.lock() {
                *lock = Some(load_contact_snapshot(&db_path, &query));
            }
        });
    }

    fn import_contacts_vcf(&mut self) {
        if self.db_path.trim().is_empty() {
            self.contact_status = "Open a database first".to_string();
            return;
        }
        let Some(path) = FileDialog::new().add_filter("vCard", &["vcf"]).pick_file() else {
            return;
        };
        let db_path = self.db_path.clone();
        let query = self.contact_search.trim().to_string();
        let pending_status = Arc::clone(&self.pending_contact_status);
        let pending_contacts = Arc::clone(&self.pending_contacts);
        self.contact_status = "Importing vCard...".to_string();
        std::thread::spawn(move || {
            let result = import_contacts_from_vcf(&path)
                .and_then(|contacts| upsert_contacts(&db_path, &contacts));
            let status = match result {
                Ok(count) => format!("Imported {} contacts from {}", count, path.display()),
                Err(e) => format!("vCard import failed: {}", e),
            };
            if let Ok(mut lock) = pending_status.lock() {
                *lock = Some(status);
            }
            if let Ok(mut lock) = pending_contacts.lock() {
                *lock = Some(load_contact_snapshot(&db_path, &query));
            }
        });
    }

    fn export_contacts_vcf(&mut self) {
        if self.db_path.trim().is_empty() {
            self.contact_status = "Open a database first".to_string();
            return;
        }
        let Some(path) = FileDialog::new().add_filter("vCard", &["vcf"]).save_file() else {
            return;
        };
        let db_path = self.db_path.clone();
        let pending_status = Arc::clone(&self.pending_contact_status);
        self.contact_status = "Exporting vCard...".to_string();
        std::thread::spawn(move || {
            let status = match export_contacts_to_vcf(&db_path, &path) {
                Ok(count) => format!("Exported {} contacts to {}", count, path.display()),
                Err(e) => format!("vCard export failed: {}", e),
            };
            if let Ok(mut lock) = pending_status.lock() {
                *lock = Some(status);
            }
        });
    }

    fn import_contacts_csv(&mut self) {
        if self.db_path.trim().is_empty() {
            self.contact_status = "Open a database first".to_string();
            return;
        }
        let Some(path) = FileDialog::new().add_filter("CSV", &["csv"]).pick_file() else {
            return;
        };
        let db_path = self.db_path.clone();
        let query = self.contact_search.trim().to_string();
        let pending_status = Arc::clone(&self.pending_contact_status);
        let pending_contacts = Arc::clone(&self.pending_contacts);
        self.contact_status = "Importing CSV...".to_string();
        std::thread::spawn(move || {
            let result = import_contacts_from_csv(&path)
                .and_then(|contacts| upsert_contacts(&db_path, &contacts));
            let status = match result {
                Ok(count) => format!("Imported {} contacts from {}", count, path.display()),
                Err(e) => format!("CSV import failed: {}", e),
            };
            if let Ok(mut lock) = pending_status.lock() {
                *lock = Some(status);
            }
            if let Ok(mut lock) = pending_contacts.lock() {
                *lock = Some(load_contact_snapshot(&db_path, &query));
            }
        });
    }

    fn export_contacts_csv(&mut self) {
        if self.db_path.trim().is_empty() {
            self.contact_status = "Open a database first".to_string();
            return;
        }
        let Some(path) = FileDialog::new().add_filter("CSV", &["csv"]).save_file() else {
            return;
        };
        let db_path = self.db_path.clone();
        let pending_status = Arc::clone(&self.pending_contact_status);
        self.contact_status = "Exporting CSV...".to_string();
        std::thread::spawn(move || {
            let status = match export_contacts_to_csv(&db_path, &path) {
                Ok(count) => format!("Exported {} contacts to {}", count, path.display()),
                Err(e) => format!("CSV export failed: {}", e),
            };
            if let Ok(mut lock) = pending_status.lock() {
                *lock = Some(status);
            }
        });
    }

    fn find_contact_duplicates(&mut self) {
        if self.db_path.trim().is_empty() {
            self.contact_status = "Open a database first".to_string();
            return;
        }
        if self.duplicates_in_flight {
            return;
        }
        self.duplicates_in_flight = true;
        self.contact_status = "Scanning for duplicates...".to_string();
        let db_path = self.db_path.clone();
        let pending = Arc::clone(&self.pending_duplicate_groups);
        std::thread::spawn(move || {
            let groups = find_duplicate_groups(&db_path);
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(groups);
            }
        });
    }

    fn sender_label(&self, msg: &Message) -> String {
        self.contact_name_cache
            .get(&msg.address)
            .cloned()
            .unwrap_or_else(|| msg.address.clone())
    }

    fn is_self_message(&self, msg: &Message) -> bool {
        // #todo: surface direction status (sent/received/unknown) in the thread header tooltip.
        match msg.direction {
            sms_types::MessageDirection::Outgoing => return true,
            sms_types::MessageDirection::Incoming => return false,
            sms_types::MessageDirection::Unknown => {}
        }
        if self.self_addresses.is_empty() {
            return false;
        }
        self.self_addresses.iter().any(|addr| addr == &msg.address)
    }

    fn run_search(&mut self) {
        if self.search_query.trim().is_empty() {
            self.results.clear();
            self.status = "Empty query".to_string();
            return;
        }
        if self.search_backend.is_none() {
            self.status = "Open a database first".to_string();
            return;
        }
        if self.search_in_flight {
            return;
        }
        self.thread_results.clear();
        self.thread_anchor = None;
        self.search_in_flight = true;
        self.status = "Searching...".to_string();
        let query = self.search_query.clone();
        let db_path = self.db_path.clone();
        let limit = self.page_size;
        let offset = self.page_offset;
        let filters = self.search_filters.clone();

        // Calculate max_results based on user settings
        let max_results = if self.search_unlimited {
            None // Unlimited search
        } else {
            self.search_max_results.parse::<usize>().ok()
        };

        let pending = Arc::clone(&self.pending_results);
        std::thread::spawn(move || {
            let results =
                run_paged_fts_filtered(&db_path, &query, limit, offset, &filters, max_results)
                    .unwrap_or_default();
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(results);
            }
        });
    }

    fn rebuild_fts_index(&mut self) {
        if self.db_path.trim().is_empty() {
            self.status = "Open a database first".to_string();
            return;
        }
        let db_path = self.db_path.clone();
        let pending = Arc::clone(&self.pending_fts_rebuild);
        self.status = "Rebuilding FTS index...".to_string();
        std::thread::spawn(move || {
            let result: sms_errors::Result<()> = (|| {
                let db = Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())?;
                let conn = db.connection();
                sms_db::rebuild_fts(conn)?;
                Ok(())
            })();
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(match result {
                    Ok(()) => "FTS rebuild complete".to_string(),
                    Err(err) => format!("FTS rebuild failed: {}", err),
                });
            }
        });
        // #todo: expose rebuild progress or row counts for large archives.
    }

    fn start_thread_view(&mut self, thread_id: &str, anchor: Option<uuid::Uuid>) {
        if self.db_path.trim().is_empty() {
            self.status = "Open a database first".to_string();
            return;
        }
        if thread_id.trim().is_empty() {
            self.status = "No thread id available".to_string();
            return;
        }
        if self.thread_in_flight {
            return;
        }
        self.thread_in_flight = true;
        self.thread_anchor = anchor;
        self.status = "Loading thread...".to_string();
        let db_path = self.db_path.clone();
        let thread = thread_id.to_string();

        // Use context_window_size for anchored views (±N around message),
        // otherwise use thread_limit for full thread view
        let context_size = if anchor.is_some() {
            self.context_window_size.parse::<usize>().unwrap_or(25)
        } else {
            self.thread_limit
        };

        let anchor_id = anchor;
        let pending = Arc::clone(&self.pending_thread_results);
        std::thread::spawn(move || {
            let results = if let Some(anchor_id) = anchor_id {
                if context_size > 0 {
                    load_thread_window(&db_path, &thread, anchor_id, context_size)
                        .unwrap_or_default()
                } else {
                    load_thread_messages(&db_path, &thread, 0).unwrap_or_default()
                }
            } else {
                load_thread_messages(&db_path, &thread, context_size).unwrap_or_default()
            };
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(results);
            }
        });
    }

    fn start_address_view(&mut self, address: &str, anchor: Option<uuid::Uuid>) {
        if self.db_path.trim().is_empty() {
            self.status = "Open a database first".to_string();
            return;
        }
        if address.trim().is_empty() {
            self.status = "No sender available".to_string();
            return;
        }
        if self.thread_in_flight {
            return;
        }
        self.thread_in_flight = true;
        self.thread_anchor = anchor;
        self.status = "Loading conversation...".to_string();
        let db_path = self.db_path.clone();
        let addr = address.to_string();

        let context_size = if anchor.is_some() {
            self.context_window_size.parse::<usize>().unwrap_or(25)
        } else {
            self.thread_limit
        };

        let anchor_id = anchor;
        let pending = Arc::clone(&self.pending_thread_results);
        std::thread::spawn(move || {
            let results = if let Some(anchor_id) = anchor_id {
                if context_size > 0 {
                    load_address_window(&db_path, &addr, anchor_id, context_size)
                        .unwrap_or_default()
                } else {
                    load_address_messages(&db_path, &addr, 0).unwrap_or_default()
                }
            } else {
                load_address_messages(&db_path, &addr, context_size).unwrap_or_default()
            };
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(results);
            }
        });
    }

    fn open_thread_from_parts(
        &mut self,
        thread_id: Option<&String>,
        address: Option<&String>,
        anchor: Option<uuid::Uuid>,
    ) {
        // Skip bogus addresses from backfilled attachments
        let address = address.filter(|a| !a.is_empty() && *a != "backfill");
        if let Some(thread) = thread_id.filter(|t| !t.is_empty()) {
            self.start_thread_view(thread, anchor);
        } else if let Some(addr) = address {
            self.start_address_view(addr, anchor);
        } else {
            self.status = "No thread info — this media was recovered by backfill".to_string();
        }
    }

    fn clear_thread_view(&mut self) {
        self.thread_results.clear();
        self.thread_attachments.clear();
        self.thread_anchor = None;
        self.thread_scroll_to_anchor = false;
    }

    fn load_media_page(&mut self) {
        if self.db_path.trim().is_empty() {
            return;
        }
        if self.media_in_flight {
            return;
        }
        self.media_in_flight = true;
        let db_path = self.db_path.clone();
        let query = self.media_query.trim().to_string();
        let limit = self.media_page_size;
        let offset = self.media_page_offset;
        let nsfw_filter = self.media_nsfw_filter;
        let nsfw_threshold = self.media_nsfw_threshold as f64;
        let pending = Arc::clone(&self.pending_media);
        std::thread::spawn(move || {
            let db = match Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())
            {
                Ok(db) => db,
                Err(_) => {
                    if let Ok(mut lock) = pending.lock() {
                        *lock = Some((Vec::new(), 0));
                    }
                    return;
                }
            };
            let conn = db.connection();
            let mut results = Vec::new();
            let base_sql = "SELECT attachments.id, attachments.mime_type, attachments.file_path, attachments.thumbnail_path, \
                            attachments.message_id, messages.thread_id, messages.timestamp, messages.address, \
                            attachments.ocr_text, attachments.ocr_model, attachments.ocr_timestamp, \
                            attachments.vision_analysis, attachments.vision_model, attachments.vision_timestamp, \
                            attachments.nsfw_label, attachments.nsfw_score, attachments.nsfw_model, attachments.nsfw_timestamp \
                        FROM attachments \
                        LEFT JOIN messages ON messages.id = attachments.message_id";
            let mut where_clauses = Vec::new();
            let mut params_vec: Vec<rusqlite::types::Value> = Vec::new();
            if !query.is_empty() {
                where_clauses.push(
                    "(attachments.mime_type LIKE ? OR attachments.file_path LIKE ?)".to_string(),
                );
                let like = format!("%{}%", query);
                params_vec.push(like.clone().into());
                params_vec.push(like.into());
            }
            match nsfw_filter {
                MediaNsfwFilter::ShowAll => {}
                MediaNsfwFilter::OnlyNsfw => {
                    where_clauses.push(
                        "attachments.nsfw_score IS NOT NULL AND attachments.nsfw_score >= ?"
                            .to_string(),
                    );
                    params_vec.push(nsfw_threshold.into());
                }
                MediaNsfwFilter::HideNsfw => {
                    where_clauses.push(
                        "attachments.nsfw_score IS NULL OR attachments.nsfw_score < ?".to_string(),
                    );
                    params_vec.push(nsfw_threshold.into());
                }
            }
            let where_sql = if where_clauses.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", where_clauses.join(" AND "))
            };
            let sql = format!(
                "{}{} ORDER BY attachments.created_at DESC LIMIT ? OFFSET ?",
                base_sql, where_sql
            );
            params_vec.push((limit as i64).into());
            params_vec.push((offset as i64).into());
            if let Ok(mut stmt) = conn.prepare(&sql) {
                let rows = stmt.query_map(rusqlite::params_from_iter(params_vec), |row| {
                    Ok(AttachmentRow {
                        id: row.get(0)?,
                        mime_type: row.get(1)?,
                        file_path: row.get(2)?,
                        thumbnail_path: row.get(3)?,
                        message_id: row.get(4)?,
                        thread_id: row.get(5)?,
                        timestamp: row.get(6)?,
                        address: row.get(7)?,
                        ocr_text: row.get(8)?,
                        ocr_model: row.get(9)?,
                        ocr_timestamp: row.get(10)?,
                        vision_analysis: row.get(11)?,
                        vision_model: row.get(12)?,
                        vision_timestamp: row.get(13)?,
                        nsfw_label: row.get(14)?,
                        nsfw_score: row.get(15)?,
                        nsfw_model: row.get(16)?,
                        nsfw_timestamp: row.get(17)?,
                    })
                });
                if let Ok(rows) = rows {
                    for row in rows.flatten() {
                        results.push(row);
                    }
                }
            }
            // Query total count (same filters, no LIMIT/OFFSET)
            let count_sql = format!(
                "SELECT COUNT(*) FROM attachments LEFT JOIN messages ON messages.id = attachments.message_id{}",
                where_sql
            );
            let mut count_params: Vec<rusqlite::types::Value> = Vec::new();
            if !query.is_empty() {
                let like = format!("%{}%", query);
                count_params.push(like.clone().into());
                count_params.push(like.into());
            }
            match nsfw_filter {
                MediaNsfwFilter::ShowAll => {}
                MediaNsfwFilter::OnlyNsfw | MediaNsfwFilter::HideNsfw => {
                    count_params.push(nsfw_threshold.into());
                }
            }
            let total: usize = conn
                .query_row(
                    &count_sql,
                    rusqlite::params_from_iter(count_params),
                    |row| row.get(0),
                )
                .unwrap_or(0);

            if let Ok(mut lock) = pending.lock() {
                *lock = Some((results, total));
            }
        });
    }

    fn start_ocr_for_attachment(&mut self, attachment: &AttachmentRow) {
        if self.db_path.trim().is_empty() {
            self.status = "Open a database first".to_string();
            return;
        }
        if !attachment.mime_type.starts_with("image/") {
            self.status = "OCR supports image attachments only".to_string();
            return;
        }
        if self.ocr_in_progress.contains(&attachment.id) {
            return;
        }
        let Some(path) = self.resolve_media_path(&attachment.file_path) else {
            self.status = "Media file not found".to_string();
            return;
        };
        if !path.exists() {
            self.status = "Media file not found".to_string();
            return;
        }
        self.ocr_in_progress.insert(attachment.id.clone());
        let db_path = self.db_path.clone();
        let attachment_id = attachment.id.clone();
        let cmd_override = self.tesseract_cmd.trim().to_string();
        let pending = Arc::clone(&self.pending_ocr);
        std::thread::spawn(move || {
            let attachment_id_for_update = attachment_id.clone();
            let result = run_ocr_tesseract(&path, Some(cmd_override.as_str())).and_then(|payload| {
                let db = Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())?;
                let conn = db.connection();
                conn.execute(
                    "UPDATE attachments SET ocr_text = ?1, ocr_model = ?2, ocr_timestamp = ?3 WHERE id = ?4",
                    params![
                        payload.text,
                        payload.model,
                        payload.timestamp,
                        attachment_id_for_update
                    ],
                )?;
                Ok(payload)
            });
            if let Ok(mut lock) = pending.lock() {
                lock.push(OcrUpdate {
                    attachment_id,
                    result,
                });
            }
        });
    }

    fn start_vision_for_attachment(&mut self, attachment: &AttachmentRow) {
        if self.db_path.trim().is_empty() {
            self.status = "Open a database first".to_string();
            return;
        }
        if !attachment.mime_type.starts_with("image/") {
            self.status = "Vision supports image attachments only".to_string();
            return;
        }
        let base_url = self.vision_base_url.trim().to_string();
        let model = self.vision_model.trim().to_string();
        let prompt = self.vision_prompt.trim().to_string();
        if base_url.is_empty() || model.is_empty() {
            self.status = "Set vision base URL and model first".to_string();
            return;
        }
        if self.vision_in_progress.contains(&attachment.id) {
            return;
        }
        let Some(path) = self.resolve_media_path(&attachment.file_path) else {
            self.status = "Media file not found".to_string();
            return;
        };
        if !path.exists() {
            self.status = "Media file not found".to_string();
            return;
        }
        self.vision_in_progress.insert(attachment.id.clone());
        let db_path = self.db_path.clone();
        let attachment_id = attachment.id.clone();
        let pending = Arc::clone(&self.pending_vision);
        std::thread::spawn(move || {
            let attachment_id_for_update = attachment_id.clone();
            let result = run_vision_ollama(&path, &base_url, &model, &prompt)
                .and_then(|payload| {
                let db = Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())?;
                let conn = db.connection();
                conn.execute(
                    "UPDATE attachments SET vision_analysis = ?1, vision_model = ?2, vision_timestamp = ?3 WHERE id = ?4",
                    params![
                        payload.analysis,
                        payload.model,
                        payload.timestamp,
                        attachment_id_for_update
                    ],
                )?;
                Ok(payload)
            });
            if let Ok(mut lock) = pending.lock() {
                lock.push(VisionUpdate {
                    attachment_id,
                    result,
                });
            }
        });
    }

    fn start_nsfw_for_attachment(&mut self, attachment: &AttachmentRow) {
        if self.db_path.trim().is_empty() {
            self.status = "Open a database first".to_string();
            return;
        }
        if self.nsfw_in_progress.contains(&attachment.id) {
            return;
        }
        let Some(path) = self.resolve_media_path(&attachment.file_path) else {
            self.status = "Media file not found".to_string();
            return;
        };
        if !path.exists() {
            self.status = "Media file not found".to_string();
            return;
        }
        let base_url = self.vision_base_url.trim().to_string();
        let model = self.vision_model.trim().to_string();
        let prompt = self.media_nsfw_prompt.trim().to_string();
        let threshold = self.media_nsfw_threshold as f64;
        if base_url.is_empty() || model.is_empty() {
            self.status = "Set vision base URL and model first".to_string();
            return;
        }
        self.nsfw_in_progress.insert(attachment.id.clone());
        let db_path = self.db_path.clone();
        let attachment_id = attachment.id.clone();
        let mime_type = attachment.mime_type.clone();
        let max_frames = self.media_keyframe_max.max(1);
        let pending = Arc::clone(&self.pending_nsfw);
        std::thread::spawn(move || {
            let result: Result<NsfwPayload> = (|| {
                let (frames, temp_dir) = extract_keyframes(&path, &mime_type, max_frames)?;
                let result: Result<NsfwPayload> = (|| {
                    let payload =
                        classify_nsfw_frames(&frames, &base_url, &model, &prompt, threshold)?;
                    let db =
                        Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())?;
                    let conn = db.connection();
                    conn.execute(
                        "UPDATE attachments SET nsfw_label = ?1, nsfw_score = ?2, nsfw_model = ?3, nsfw_timestamp = ?4 WHERE id = ?5",
                        params![
                            payload.label,
                            payload.score,
                            payload.model,
                            payload.timestamp,
                            attachment_id
                        ],
                    )?;
                    Ok(payload)
                })();
                cleanup_temp_dir(temp_dir);
                result
            })();
            if let Ok(mut lock) = pending.lock() {
                lock.push(NsfwUpdate {
                    attachment_id,
                    result,
                });
            }
        });
    }

    fn start_media_embedding_for_attachment(&mut self, attachment: &AttachmentRow) {
        if self.db_path.trim().is_empty() {
            self.media_embed_status = "Open a database first".to_string();
            return;
        }
        if self.media_embed_in_progress.contains(&attachment.id) {
            return;
        }
        let Some(path) = self.resolve_media_path(&attachment.file_path) else {
            self.media_embed_status = "Media file not found".to_string();
            return;
        };
        if !path.exists() {
            self.media_embed_status = "Media file not found".to_string();
            return;
        }
        let vision_base = self.vision_base_url.trim().to_string();
        let vision_model = self.vision_model.trim().to_string();
        let prompt = self.media_embed_prompt.trim().to_string();
        if vision_base.is_empty() || vision_model.is_empty() {
            self.media_embed_status = "Set vision base URL and model first".to_string();
            return;
        }
        let use_local = self.media_embed_use_local;
        let embed_base = self.ollama_base_url.trim().to_string();
        let embed_model = self.ollama_selected.trim().to_string();
        let local_config = if use_local {
            let model_path = self.embed_model_path.trim();
            let tokenizer_path = self.embed_tokenizer_path.trim();
            if model_path.is_empty() || tokenizer_path.is_empty() {
                self.media_embed_status =
                    "Set local model + tokenizer in Embeddings tab".to_string();
                return;
            }
            Some(EmbeddingConfig {
                model_path: Some(PathBuf::from(model_path)),
                tokenizer_path: Some(PathBuf::from(tokenizer_path)),
                model_name: self.embed_model_name.trim().to_string(),
                model_version: self.embed_model_version.trim().to_string(),
                dimensions: self.embed_dimensions,
                device: self.embed_device,
                max_length: self.embed_max_length,
                normalize: self.embed_normalize,
                input_ids_name: None,
                attention_mask_name: None,
                token_type_ids_name: None,
                output_name: None,
            })
        } else {
            if embed_base.is_empty() || embed_model.is_empty() {
                self.media_embed_status =
                    "Set Ollama base URL and embedding model in Embeddings tab".to_string();
                return;
            }
            None
        };
        let max_frames = self.media_keyframe_max.max(1);
        let db_path = self.db_path.clone();
        let attachment_id = attachment.id.clone();
        let mime_type = attachment.mime_type.clone();
        let pending_status = Arc::clone(&self.pending_media_embed_status);
        let pending_done = Arc::clone(&self.pending_media_embed_done);
        self.media_embed_in_progress.insert(attachment.id.clone());
        std::thread::spawn(move || {
            let result: Result<usize> = (|| {
                let (frames, temp_dir) = extract_keyframes(&path, &mime_type, max_frames)?;
                let result: Result<usize> = (|| {
                    let db =
                        Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())?;
                    let conn = db.connection();
                    let mut model_id: Option<String> = None;
                    let mut inserted = 0usize;
                    let mut local_service = if let Some(config) = local_config {
                        Some(EmbeddingService::new(config)?)
                    } else {
                        None
                    };
                    for frame in &frames {
                        let vision =
                            run_vision_ollama(&frame.path, &vision_base, &vision_model, &prompt)?;
                        let (_, caption) = split_vision_analysis(&vision.analysis);
                        let embed_input = sanitize_ollama_embed_input(&caption, &vision.analysis);
                        let embedding = if let Some(service) = local_service.as_mut() {
                            service.embed(&embed_input)?
                        } else {
                            SmsArchiveApp::ollama_embed(&embed_base, &embed_model, &embed_input)?
                        };
                        if model_id.is_none() {
                            let (model_name, model_version, model_meta) =
                                if let Some(service) = local_service.as_ref() {
                                    let info = service.model_info();
                                    let meta = service.model_meta();
                                    (
                                        info.name.as_str(),
                                        "local-media",
                                        sms_db::ModelMeta {
                                            dims: Some(meta.dimensions as i64),
                                            max_length: Some(meta.max_length as i64),
                                            normalize: Some(meta.normalize),
                                            tokenizer_path: meta.tokenizer_path.clone(),
                                            input_ids_name: meta.input_ids_name.clone(),
                                            attention_mask_name: meta.attention_mask_name.clone(),
                                            token_type_ids_name: meta.token_type_ids_name.clone(),
                                            output_name: meta.output_name.clone(),
                                        },
                                    )
                                } else {
                                    (
                                        embed_model.as_str(),
                                        "ollama-media",
                                        sms_db::ModelMeta {
                                            dims: Some(embedding.len() as i64),
                                            max_length: None,
                                            normalize: None,
                                            tokenizer_path: None,
                                            input_ids_name: None,
                                            attention_mask_name: None,
                                            token_type_ids_name: None,
                                            output_name: None,
                                        },
                                    )
                                };
                            let id = sms_db::upsert_ml_model_with_meta(
                                conn,
                                model_name,
                                model_version,
                                None,
                                &model_meta,
                            )?;
                            model_id = Some(id);
                        }
                        // Set in the is_none() branch above; guard instead of
                        // unwrapping so a future refactor can't panic here.
                        let Some(model_id) = model_id.as_ref() else {
                            continue;
                        };
                        sms_db::insert_media_embedding(
                            conn,
                            &attachment_id,
                            model_id,
                            frame.index as i64,
                            frame.time_ms,
                            Some(&caption),
                            &embedding,
                        )?;
                        inserted += 1;
                    }
                    Ok(inserted)
                })();
                cleanup_temp_dir(temp_dir);
                result
            })();
            if let Ok(mut lock) = pending_status.lock() {
                let msg = match result {
                    Ok(count) => format!("Embedded {} keyframe(s)", count),
                    Err(err) => format!("Embed error: {}", err),
                };
                lock.push(msg);
            }
            if let Ok(mut lock) = pending_done.lock() {
                lock.push(attachment_id);
            }
        });
    }

    fn start_media_embedding_inspect(&mut self, attachment: &AttachmentRow) {
        if self.db_path.trim().is_empty() {
            self.media_embed_inspect_status = "Open a database first".to_string();
            return;
        }
        if self.media_embed_inspect_in_flight {
            return;
        }
        self.media_embed_inspect_target = Some(attachment.id.clone());
        self.media_embed_inspect_in_flight = true;
        self.media_embed_inspect_status = "Inspecting embeddings...".to_string();
        let db_path = self.db_path.clone();
        let attachment_id = attachment.id.clone();
        let pending = Arc::clone(&self.pending_media_embed_inspect);
        std::thread::spawn(move || {
            let result: Result<Vec<MediaEmbedInspectRow>> = (|| {
                let db = Database::open(Path::new(&db_path), ResourceProfile::detect())?;
                let conn = db.connection();
                let mut stmt = conn.prepare(
                    "SELECT me.frame_index, me.frame_time_ms, me.caption, me.vector, me.dims, \
                            COALESCE(m.name, 'unknown'), COALESCE(m.version, 'unknown') \
                     FROM media_embeddings me \
                     LEFT JOIN ml_models m ON m.id = me.model_id \
                     WHERE me.attachment_id = ?1 \
                     ORDER BY me.frame_index ASC",
                )?;
                let mut rows = stmt.query([attachment_id.as_str()])?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    let frame_index: i64 = row.get(0)?;
                    let frame_time_ms: Option<i64> = row.get(1)?;
                    let caption: Option<String> = row.get(2)?;
                    let bytes: Vec<u8> = row.get(3)?;
                    let dims: i64 = row.get(4)?;
                    let model_name: String = row.get(5)?;
                    let model_version: String = row.get(6)?;
                    let embedding = match decode_f32_vec(&bytes, dims as usize) {
                        Some(v) => v,
                        None => continue,
                    };
                    let stats = summarize_embedding(&embedding);
                    out.push(MediaEmbedInspectRow {
                        model_name,
                        model_version,
                        frame_index,
                        frame_time_ms,
                        caption,
                        stats,
                    });
                }
                Ok(out)
            })();
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(result.unwrap_or_default());
            }
        });
    }

    fn run_media_audit(&mut self) {
        if self.db_path.trim().is_empty() {
            self.media_audit_status = "Open a database first".to_string();
            return;
        }
        if self.media_audit_in_flight {
            return;
        }
        self.media_audit_in_flight = true;
        self.media_audit_status = "Running media audit...".to_string();
        let db_path = self.db_path.clone();
        let media_root = self.media_root.clone();
        let pending = Arc::clone(&self.pending_media_audit);
        std::thread::spawn(move || {
            let result: Result<MediaAuditSnapshot> = (|| {
                let db = Database::open(Path::new(&db_path), ResourceProfile::detect())?;
                let conn = db.connection();
                let db_attachments_total: usize = conn
                    .query_row("SELECT COUNT(*) FROM attachments", [], |row| row.get(0))
                    .unwrap_or(0);
                let db_image_video_total: usize = conn
                    .query_row(
                        "SELECT COUNT(*) FROM attachments WHERE mime_type LIKE 'image/%' OR mime_type LIKE 'video/%'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);

                let mut mime_counts: HashMap<String, usize> = HashMap::new();
                let mut db_paths: HashSet<String> = HashSet::new();
                let mut missing = 0usize;
                let mut missing_samples = Vec::new();
                let mut stmt = conn.prepare("SELECT mime_type, file_path FROM attachments")?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let mime: String = row.get(0)?;
                    let path: String = row.get(1)?;
                    let key = mime.split('/').next().unwrap_or("other").to_string();
                    *mime_counts.entry(key).or_insert(0) += 1;
                    if let Some(normalized) =
                        normalize_media_rel(&path, &db_path, media_root.as_ref())
                    {
                        db_paths.insert(normalized);
                    }
                    let resolved =
                        resolve_media_path_with_root(&db_path, media_root.as_ref(), &path);
                    if !resolved.as_ref().map(|p| p.exists()).unwrap_or(false) {
                        missing += 1;
                        if missing_samples.len() < 10 {
                            missing_samples.push(path);
                        }
                    }
                }

                let mut media_files_total = 0usize;
                let mut fs_unlinked_total = 0usize;
                let mut fs_unlinked_samples = Vec::new();
                let root = media_root
                    .clone()
                    .unwrap_or_else(|| resolve_media_dir(&db_path));
                if root.exists() {
                    for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
                        if entry.file_type().is_dir() {
                            let name = entry.file_name().to_string_lossy().to_lowercase();
                            if name == "thumbnails" || name == "previews" {
                                continue;
                            }
                        }
                        if entry.file_type().is_file() {
                            let name = entry.file_name().to_string_lossy().to_lowercase();
                            if name.ends_with(".db") {
                                continue;
                            }
                            media_files_total += 1;
                            if let Ok(rel) = entry.path().strip_prefix(&root) {
                                let rel_str = rel.to_string_lossy().replace('\\', "/");
                                let rel_norm = rel_str.to_lowercase();
                                if !db_paths.contains(&rel_norm) {
                                    fs_unlinked_total += 1;
                                    if fs_unlinked_samples.len() < 10 {
                                        fs_unlinked_samples.push(rel_str);
                                    }
                                }
                            }
                        }
                    }
                }

                let mut db_mime_counts: Vec<(String, usize)> = mime_counts.into_iter().collect();
                db_mime_counts.sort_by(|a, b| a.0.cmp(&b.0));

                Ok(MediaAuditSnapshot {
                    media_root: Some(root),
                    media_files_total,
                    db_attachments_total,
                    db_image_video_total,
                    db_missing_files: missing,
                    db_missing_samples: missing_samples,
                    db_mime_counts,
                    fs_unlinked_total,
                    fs_unlinked_samples,
                })
            })();
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(result.unwrap_or_default());
            }
        });
    }

    fn run_media_backfill(&mut self) {
        if self.db_path.trim().is_empty() {
            self.media_backfill_status = "Open a database first".to_string();
            return;
        }
        if self.media_backfill_in_flight {
            return;
        }
        self.media_backfill_in_flight = true;
        self.media_backfill_status = "Scanning media directory...".to_string();
        let db_path = self.db_path.clone();
        let media_root = self.media_root.clone();
        let pending = Arc::clone(&self.pending_media_backfill);
        std::thread::spawn(move || {
            let result: Result<String> = (|| {
                let db = Database::open(Path::new(&db_path), ResourceProfile::detect())?;
                let conn = db.connection();

                // Collect existing file_path values (lowercased) for fast lookup
                let mut existing: HashSet<String> = HashSet::new();
                let mut stmt = conn.prepare("SELECT file_path FROM attachments")?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let path: String = row.get(0)?;
                    existing.insert(path.replace('\\', "/").to_lowercase());
                }

                let root = media_root
                    .clone()
                    .unwrap_or_else(|| resolve_media_dir(&db_path));
                if !root.exists() {
                    return Ok("Media directory not found".to_string());
                }

                let mut inserted = 0usize;
                let mut scanned = 0usize;
                let progress_pending = pending.clone();

                // Create a placeholder message for orphaned attachments
                let placeholder_id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT OR IGNORE INTO messages (id, timestamp, address, body, body_searchable, message_type, message_direction) \
                     VALUES (?1, 0, 'backfill', '[backfill] Attachments recovered from media directory', \
                             'backfill attachments recovered', 2, 0)",
                    rusqlite::params![&placeholder_id],
                )?;

                // Build a hash->message_id lookup from existing attachments to try matching
                let mut hash_to_msg: HashMap<Vec<u8>, String> = HashMap::new();
                {
                    let mut hash_stmt =
                        conn.prepare("SELECT file_hash, message_id FROM attachments")?;
                    let mut rows = hash_stmt.query([])?;
                    while let Some(row) = rows.next()? {
                        let hash: Vec<u8> = row.get(0)?;
                        let msg_id: String = row.get(1)?;
                        hash_to_msg.insert(hash, msg_id);
                    }
                }

                // Build address (lowercased) -> most-recent message id map for directory-based matching.
                // SMS Backup & Restore stores media in per-contact subdirs named after the address.
                let mut address_to_msg: HashMap<String, String> = HashMap::new();
                {
                    let mut addr_stmt = conn.prepare(
                        "SELECT address, id FROM messages \
                         WHERE address != 'backfill' AND address != '' \
                         ORDER BY timestamp DESC",
                    )?;
                    let mut rows = addr_stmt.query([])?;
                    while let Some(row) = rows.next()? {
                        let addr: String = row.get(0)?;
                        let msg_id: String = row.get(1)?;
                        // or_insert keeps the first (most-recent) entry per address
                        address_to_msg.entry(addr.to_lowercase()).or_insert(msg_id);
                    }
                }

                // Re-link attachments already in the DB that are tied to a backfill placeholder
                // but whose file_path directory matches a real contact address.
                conn.execute_batch(
                    "UPDATE attachments \
                     SET message_id = ( \
                         SELECT m.id FROM messages m \
                         WHERE lower(m.address) = lower(substr(attachments.file_path, 1, instr(attachments.file_path, '/') - 1)) \
                         AND m.address != 'backfill' \
                         ORDER BY m.timestamp DESC \
                         LIMIT 1 \
                     ) \
                     WHERE message_id IN (SELECT id FROM messages WHERE address = 'backfill') \
                     AND instr(file_path, '/') > 0 \
                     AND EXISTS ( \
                         SELECT 1 FROM messages m \
                         WHERE lower(m.address) = lower(substr(attachments.file_path, 1, instr(attachments.file_path, '/') - 1)) \
                         AND m.address != 'backfill' \
                     )",
                )?;

                conn.execute_batch("BEGIN TRANSACTION")?;

                let walker = WalkDir::new(&root).into_iter().filter_entry(|e| {
                    if e.file_type().is_dir() {
                        let name = e.file_name().to_string_lossy().to_lowercase();
                        name != "thumbnails" && name != "previews"
                    } else {
                        true
                    }
                });
                for entry in walker.filter_map(|e| e.ok()) {
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let name_lower = entry.file_name().to_string_lossy().to_lowercase();
                    if name_lower.ends_with(".db")
                        || name_lower.ends_with(".json")
                        || name_lower.starts_with('.')
                    {
                        continue;
                    }
                    scanned += 1;

                    let rel = match entry.path().strip_prefix(&root) {
                        Ok(r) => r.to_string_lossy().replace('\\', "/"),
                        Err(_) => continue,
                    };
                    let rel_lower = rel.to_lowercase();
                    if existing.contains(&rel_lower) {
                        continue;
                    }

                    // Determine mime type from extension
                    let mime = match rel_lower.rsplit('.').next() {
                        Some("jpg") | Some("jpeg") => "image/jpeg",
                        Some("png") => "image/png",
                        Some("gif") => "image/gif",
                        Some("webp") => "image/webp",
                        Some("heic") | Some("heif") => "image/heic",
                        Some("mp4") => "video/mp4",
                        Some("mov") => "video/quicktime",
                        Some("3gp") => "video/3gpp",
                        Some("mp3") => "audio/mpeg",
                        Some("m4a") => "audio/mp4",
                        Some("amr") => "audio/amr",
                        Some("pdf") => "application/pdf",
                        _ => "application/octet-stream",
                    };

                    // Compute file hash
                    let file_bytes = match std::fs::read(entry.path()) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    let hash = blake3::hash(&file_bytes);
                    let hash_bytes = hash.as_bytes().to_vec();

                    // Try to find original parent message via hash, then by directory address, then placeholder
                    let parent_id = hash_to_msg
                        .get(&hash_bytes)
                        .cloned()
                        .or_else(|| {
                            let dir = rel.split('/').next().unwrap_or("").to_lowercase();
                            address_to_msg.get(&dir).cloned()
                        })
                        .unwrap_or_else(|| placeholder_id.clone());

                    // Check for thumbnail in thumbnails dir
                    let thumb_rel = {
                        let thumb_subdir = rel.split('/').next().unwrap_or("");
                        let thumb_filename = format!("{}.jpg", hex_hash_backfill(&hash));
                        let thumb_path = root
                            .join("thumbnails")
                            .join(thumb_subdir)
                            .join(&thumb_filename);
                        if thumb_path.exists() {
                            Some(format!("thumbnails/{}/{}", thumb_subdir, thumb_filename))
                        } else {
                            None
                        }
                    };

                    let att_id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO attachments (id, message_id, mime_type, file_path, file_hash, thumbnail_path) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT DO NOTHING",
                        rusqlite::params![att_id, parent_id, mime, rel, &hash.as_bytes()[..], thumb_rel],
                    )?;
                    inserted += 1;
                    existing.insert(rel_lower);

                    // Batch commit every 1000 inserts
                    if inserted.is_multiple_of(1000) {
                        conn.execute_batch("COMMIT; BEGIN TRANSACTION")?;
                    }

                    // Report progress every 500 files scanned
                    if scanned.is_multiple_of(500) {
                        if let Ok(mut lock) = progress_pending.lock() {
                            *lock = Some((
                                format!(
                                    "Backfill: scanned {} files, {} new so far...",
                                    scanned, inserted
                                ),
                                false,
                            ));
                        }
                    }
                }

                conn.execute_batch("COMMIT")?;

                Ok(format!(
                    "Backfill complete: scanned {} files, inserted {} new attachment records",
                    scanned, inserted
                ))
            })();

            let status = match result {
                Ok(s) => s,
                Err(e) => format!("Backfill error: {}", e),
            };
            if let Ok(mut lock) = pending.lock() {
                *lock = Some((status, true));
            }
        });
    }

    fn run_media_semantic_search(&mut self) {
        if self.media_semantic_query.trim().is_empty() {
            self.media_semantic_status = "Empty media query".to_string();
            return;
        }
        if self.db_path.trim().is_empty() {
            self.media_semantic_status = "Open a database first".to_string();
            return;
        }
        if self.media_semantic_in_flight {
            return;
        }
        if self.media_semantic_use_clip {
            let model_path = self.clip_text_model_path.trim();
            let tokenizer_path = self.clip_text_tokenizer_path.trim();
            if model_path.is_empty() || tokenizer_path.is_empty() {
                self.media_semantic_status = "Set CLIP text model + tokenizer".to_string();
                return;
            }
            self.media_semantic_in_flight = true;
            self.media_semantic_status = "Searching CLIP media...".to_string();
            let db_path = self.db_path.clone();
            let limit = self.media_semantic_limit.max(1);
            let pending = Arc::clone(&self.pending_media_semantic);
            let config = EmbeddingConfig {
                model_path: Some(PathBuf::from(model_path)),
                tokenizer_path: Some(PathBuf::from(tokenizer_path)),
                model_name: "clip-text".to_string(),
                model_version: "v1".to_string(),
                dimensions: 768,
                device: if self.clip_use_cuda {
                    DevicePreference::Gpu
                } else {
                    DevicePreference::Cpu
                },
                max_length: self.embed_max_length,
                normalize: true,
                input_ids_name: None,
                attention_mask_name: None,
                token_type_ids_name: None,
                output_name: None,
            };
            let query = self.media_semantic_query.trim().to_string();
            std::thread::spawn(move || {
                let hits =
                    media_semantic_search_clip(&db_path, config, &query, limit).unwrap_or_default();
                if let Ok(mut lock) = pending.lock() {
                    *lock = Some(hits);
                }
            });
            return;
        }
        let use_local = self.media_embed_use_local;
        let embed_base = self.ollama_base_url.trim().to_string();
        let embed_model = self.ollama_selected.trim().to_string();
        let local_config = if use_local {
            let model_path = self.embed_model_path.trim();
            let tokenizer_path = self.embed_tokenizer_path.trim();
            if model_path.is_empty() || tokenizer_path.is_empty() {
                self.media_semantic_status =
                    "Set local model + tokenizer in Embeddings tab".to_string();
                return;
            }
            Some(EmbeddingConfig {
                model_path: Some(PathBuf::from(model_path)),
                tokenizer_path: Some(PathBuf::from(tokenizer_path)),
                model_name: self.embed_model_name.trim().to_string(),
                model_version: self.embed_model_version.trim().to_string(),
                dimensions: self.embed_dimensions,
                device: self.embed_device,
                max_length: self.embed_max_length,
                normalize: self.embed_normalize,
                input_ids_name: None,
                attention_mask_name: None,
                token_type_ids_name: None,
                output_name: None,
            })
        } else {
            if embed_base.is_empty() || embed_model.is_empty() {
                self.media_semantic_status =
                    "Set Ollama base URL and embedding model in Embeddings tab".to_string();
                return;
            }
            None
        };
        self.media_semantic_in_flight = true;
        self.media_semantic_status = "Searching media...".to_string();
        let query = self.media_semantic_query.trim().to_string();
        let db_path = self.db_path.clone();
        let limit = self.media_semantic_limit.max(1);
        let pending = Arc::clone(&self.pending_media_semantic);
        std::thread::spawn(move || {
            let hits = if let Some(config) = local_config {
                media_semantic_search_local(&db_path, config, &query, limit).unwrap_or_default()
            } else {
                media_semantic_search_ollama(&db_path, &embed_base, &embed_model, &query, limit)
                    .unwrap_or_default()
            };
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(hits);
            }
        });
    }

    fn refresh_ollama_models(&mut self) {
        let base_url = self.ollama_base_url.trim().to_string();
        self.refresh_ollama_models_for(&base_url);
    }

    fn refresh_ollama_models_for(&mut self, base_url: &str) {
        if self.ollama_in_flight {
            return;
        }
        let base_url = base_url.trim();
        if base_url.is_empty() {
            self.ollama_status = "Set an Ollama base URL first".to_string();
            return;
        }
        self.ollama_in_flight = true;
        self.ollama_status = "Refreshing Ollama models...".to_string();
        self.ollama_models_source = base_url.to_string();
        let base_url = base_url.to_string();
        let pending = Arc::clone(&self.pending_ollama_models);
        let pending_log = Arc::clone(&self.pending_ollama_log);
        self.ollama_job_done.store(false, Ordering::Relaxed);
        let job_done = Arc::clone(&self.ollama_job_done);
        std::thread::spawn(move || {
            let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
            let resp = ureq::get(&url).call();
            match resp {
                Ok(resp) => {
                    let parsed: Result<OllamaTagsResponse, _> = resp.into_json();
                    match parsed {
                        Ok(tags) => {
                            if let Ok(mut lock) = pending.lock() {
                                *lock = Some(tags.models);
                            }
                            if let Ok(mut lock) = pending_log.lock() {
                                lock.push(format!("Ollama models refreshed ({})", base_url));
                            }
                        }
                        Err(err) => {
                            if let Ok(mut lock) = pending_log.lock() {
                                lock.push(format!("Ollama parse error: {}", err));
                            }
                        }
                    }
                }
                Err(err) => {
                    if let Ok(mut lock) = pending_log.lock() {
                        lock.push(format!("Ollama error: {}", err));
                    }
                }
            }
            job_done.store(true, Ordering::Relaxed);
        });
    }

    fn check_assistant_model(&mut self) {
        if self.assistant_model_check_in_flight {
            return;
        }
        let base_url = self.assistant_base_url.trim().to_string();
        let model = self.assistant_model.trim().to_string();
        if base_url.is_empty() || model.is_empty() {
            self.assistant_model_status = "FAIL! Set base URL and model".to_string();
            return;
        }
        self.assistant_model_check_in_flight = true;
        self.assistant_model_status = "Checking model...".to_string();
        let pending = Arc::clone(&self.pending_assistant_model_check);
        std::thread::spawn(move || {
            let status = check_ollama_model(&base_url, &model);
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(status);
            }
        });
    }

    fn check_vision_model(&mut self) {
        if self.vision_model_check_in_flight {
            return;
        }
        let base_url = self.vision_base_url.trim().to_string();
        let model = self.vision_model.trim().to_string();
        if base_url.is_empty() || model.is_empty() {
            self.vision_model_status = "FAIL! Set base URL and model".to_string();
            return;
        }
        self.vision_model_check_in_flight = true;
        self.vision_model_status = "Checking model...".to_string();
        let pending = Arc::clone(&self.pending_vision_model_check);
        std::thread::spawn(move || {
            let status = check_ollama_model(&base_url, &model);
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(status);
            }
        });
    }

    fn pull_ollama_model(&mut self) {
        let name = self.ollama_pull_name.trim().to_string();
        if name.is_empty() {
            self.ollama_status = "Enter a model name to pull".to_string();
            return;
        }
        if self.ollama_in_flight {
            return;
        }
        self.ollama_in_flight = true;
        self.ollama_status = "Pulling model...".to_string();
        let base_url = self.ollama_base_url.trim().to_string();
        let pending_log = Arc::clone(&self.pending_ollama_log);
        self.ollama_job_done.store(false, Ordering::Relaxed);
        let job_done = Arc::clone(&self.ollama_job_done);
        std::thread::spawn(move || {
            let url = format!("{}/api/pull", base_url.trim_end_matches('/'));
            let resp = ureq::post(&url).send_json(serde_json::json!({ "name": name }));
            match resp {
                Ok(resp) => {
                    let reader = resp.into_reader();
                    let mut buf = std::io::BufReader::new(reader);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        let read = match buf.read_line(&mut line) {
                            Ok(n) => n,
                            Err(err) => {
                                if let Ok(mut lock) = pending_log.lock() {
                                    lock.push(format!("Ollama pull read error: {}", err));
                                }
                                break;
                            }
                        };
                        if read == 0 {
                            break;
                        }
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let msg =
                            if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
                                if let Some(status) = value.get("status").and_then(|v| v.as_str()) {
                                    let completed = value.get("completed").and_then(|v| v.as_i64());
                                    let total = value.get("total").and_then(|v| v.as_i64());
                                    if let (Some(c), Some(t)) = (completed, total) {
                                        format!("{} {} / {}", status, c, t)
                                    } else {
                                        status.to_string()
                                    }
                                } else {
                                    trimmed.to_string()
                                }
                            } else {
                                trimmed.to_string()
                            };
                        if let Ok(mut lock) = pending_log.lock() {
                            lock.push(msg);
                        }
                    }
                }
                Err(err) => {
                    if let Ok(mut lock) = pending_log.lock() {
                        lock.push(format!("Ollama pull error: {}", err));
                    }
                }
            }
            job_done.store(true, Ordering::Relaxed);
        });
    }

    fn ollama_embed(base_url: &str, model: &str, text: &str) -> sms_errors::Result<Vec<f32>> {
        let url = format!("{}/api/embeddings", base_url.trim_end_matches('/'));
        let resp = ureq::post(&url)
            .send_json(serde_json::json!({ "model": model, "prompt": text }))
            .map_err(|err| sms_errors::AppError::External(format!("Ollama error: {}", err)))?;
        let parsed: OllamaEmbedResponse = resp.into_json().map_err(|err| {
            sms_errors::AppError::External(format!("Ollama parse error: {}", err))
        })?;
        if let Some(embedding) = parsed.embedding {
            return Ok(embedding);
        }
        if let Some(data) = parsed.data {
            if let Some(first) = data.into_iter().next() {
                return Ok(first.embedding);
            }
        }
        Err(sms_errors::AppError::External(
            "Ollama response missing embedding".to_string(),
        ))
    }

    fn run_semantic_search(&mut self) {
        if self.semantic_query.trim().is_empty() {
            self.semantic_status = "Empty semantic query".to_string();
            return;
        }
        if self.db_path.trim().is_empty() {
            self.semantic_status = "Open a database first".to_string();
            return;
        }
        if self.semantic_in_flight {
            return;
        }
        let use_ollama = self.use_ollama;
        let ollama_model = self.ollama_selected.trim().to_string();
        let ollama_base = self.ollama_base_url.trim().to_string();
        if use_ollama && ollama_model.is_empty() {
            self.semantic_status = "Select an Ollama model first".to_string();
            return;
        }
        if use_ollama && ollama_base.is_empty() {
            self.semantic_status = "Set Ollama base URL".to_string();
            return;
        }
        let (model_path, tokenizer_path) = if use_ollama {
            (None, None)
        } else {
            let model_path = {
                let trimmed = self.embed_model_path.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    let path = std::path::PathBuf::from(trimmed);
                    if !path.exists() {
                        self.semantic_status = "Model path not found".to_string();
                        return;
                    }
                    Some(path)
                }
            };
            let tokenizer_path = {
                let trimmed = self.embed_tokenizer_path.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    let path = std::path::PathBuf::from(trimmed);
                    if !path.exists() {
                        self.semantic_status = "Tokenizer path not found".to_string();
                        return;
                    }
                    Some(path)
                }
            };
            if model_path.is_some() && tokenizer_path.is_none() {
                self.semantic_status = "Tokenizer path required for ONNX model".to_string();
                return;
            }
            (model_path, tokenizer_path)
        };
        if !use_ollama && model_path.is_none() {
            self.semantic_status =
                "Set a local ONNX model + tokenizer or enable Ollama for semantic search"
                    .to_string();
            return;
        }

        self.semantic_in_flight = true;
        self.semantic_status = "Searching...".to_string();
        let query = self.semantic_query.clone();
        let db_path = self.db_path.clone();
        let model_name = self.embed_model_name.clone();
        let model_version = self.embed_model_version.clone();
        let dims = self.embed_dimensions.max(8);
        let max_length = self.embed_max_length.max(8);
        let normalize = self.embed_normalize;
        let limit = if self.semantic_limit == 0 {
            5000
        } else {
            self.semantic_limit
        };
        let pending = Arc::clone(&self.pending_semantic);
        std::thread::spawn(move || {
            let db = match Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())
            {
                Ok(db) => db,
                Err(_) => {
                    if let Ok(mut lock) = pending.lock() {
                        *lock = Some(Vec::new());
                    }
                    return;
                }
            };
            let conn = db.connection();
            let (model_id, query_vec) = if use_ollama {
                let vec = match SmsArchiveApp::ollama_embed(&ollama_base, &ollama_model, &query) {
                    Ok(v) => v,
                    Err(_) => {
                        if let Ok(mut lock) = pending.lock() {
                            *lock = Some(Vec::new());
                        }
                        return;
                    }
                };
                let model_meta = sms_db::ModelMeta {
                    dims: Some(vec.len() as i64),
                    max_length: None,
                    normalize: None,
                    tokenizer_path: None,
                    input_ids_name: None,
                    attention_mask_name: None,
                    token_type_ids_name: None,
                    output_name: None,
                };
                let model_id = match sms_db::upsert_ml_model_with_meta(
                    conn,
                    &ollama_model,
                    "ollama",
                    None,
                    &model_meta,
                ) {
                    Ok(id) => id,
                    Err(_) => {
                        if let Ok(mut lock) = pending.lock() {
                            *lock = Some(Vec::new());
                        }
                        return;
                    }
                };
                (model_id, vec)
            } else {
                let mut service = match EmbeddingService::new(EmbeddingConfig {
                    model_path,
                    tokenizer_path,
                    model_name,
                    model_version,
                    dimensions: dims,
                    device: DevicePreference::Cpu,
                    max_length,
                    normalize,
                    input_ids_name: None,
                    attention_mask_name: None,
                    token_type_ids_name: None,
                    output_name: None,
                }) {
                    Ok(s) => s,
                    Err(_) => {
                        if let Ok(mut lock) = pending.lock() {
                            *lock = Some(Vec::new());
                        }
                        return;
                    }
                };
                let info = service.model_info().clone();
                let meta = service.model_meta().clone();
                let model_meta = sms_db::ModelMeta {
                    dims: Some(meta.dimensions as i64),
                    max_length: Some(meta.max_length as i64),
                    normalize: Some(meta.normalize),
                    tokenizer_path: meta.tokenizer_path.clone(),
                    input_ids_name: meta.input_ids_name.clone(),
                    attention_mask_name: meta.attention_mask_name.clone(),
                    token_type_ids_name: meta.token_type_ids_name.clone(),
                    output_name: meta.output_name.clone(),
                };
                let model_id = match sms_db::upsert_ml_model_with_meta(
                    conn,
                    &info.name,
                    &info.version,
                    info.sha256.as_deref(),
                    &model_meta,
                ) {
                    Ok(id) => id,
                    Err(_) => {
                        if let Ok(mut lock) = pending.lock() {
                            *lock = Some(Vec::new());
                        }
                        return;
                    }
                };
                let query_vec = match service.embed(&query) {
                    Ok(v) => v,
                    Err(_) => {
                        if let Ok(mut lock) = pending.lock() {
                            *lock = Some(Vec::new());
                        }
                        return;
                    }
                };
                (model_id, query_vec)
            };
            let hits = semantic_search(
                std::path::Path::new(&db_path),
                model_id.as_str(),
                &query_vec,
                limit,
            )
            .unwrap_or_default();
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(hits);
            }
        });
    }

    fn refresh_model_stats(&mut self) {
        if self.db_path.trim().is_empty() {
            self.model_stats_status = "Open a database first".to_string();
            return;
        }
        if self.model_stats_in_flight {
            return;
        }
        self.model_stats_in_flight = true;
        self.model_stats_status = "Refreshing...".to_string();
        let db_path = self.db_path.clone();
        let pending = Arc::clone(&self.pending_model_stats);
        std::thread::spawn(move || {
            let db = match Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())
            {
                Ok(db) => db,
                Err(_) => {
                    if let Ok(mut lock) = pending.lock() {
                        *lock = Some(ModelStatsSnapshot::default());
                    }
                    return;
                }
            };
            let conn = db.connection();
            let total_messages: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE body_searchable != ''",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            let mut stmt = match conn.prepare(
                "SELECT id, name, version, sha256, created_at, \
                        (SELECT COUNT(*) FROM embeddings e WHERE e.model_id = ml_models.id), \
                        dims, max_length, normalize, tokenizer_path, input_ids_name, attention_mask_name, \
                        token_type_ids_name, output_name \
                 FROM ml_models ORDER BY created_at DESC",
            ) {
                Ok(stmt) => stmt,
                Err(_) => {
                    if let Ok(mut lock) = pending.lock() {
                        *lock = Some(ModelStatsSnapshot::default());
                    }
                    return;
                }
            };
            let rows = stmt.query_map([], |row| {
                Ok(ModelStat {
                    id: row.get::<_, String>(0)?,
                    name: row.get(1)?,
                    version: row.get(2)?,
                    sha256: row.get(3)?,
                    created_at: row.get(4)?,
                    embedding_count: row.get(5)?,
                    dims: row.get(6)?,
                    max_length: row.get(7)?,
                    normalize: row.get(8)?,
                    tokenizer_path: row.get(9)?,
                    input_ids_name: row.get(10)?,
                    attention_mask_name: row.get(11)?,
                    token_type_ids_name: row.get(12)?,
                    output_name: row.get(13)?,
                })
            });
            let mut models = Vec::new();
            if let Ok(rows) = rows {
                for row in rows.flatten() {
                    models.push(row);
                }
            }
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(ModelStatsSnapshot {
                    total_messages,
                    models,
                });
            }
        });
    }

    fn start_model_purge(&mut self) {
        let db_path = self.db_path.trim().to_string();
        let model_id = match &self.selected_model_id {
            Some(id) => id.clone(),
            None => {
                self.model_stats_status = "Select a model first".to_string();
                return;
            }
        };
        if db_path.is_empty() {
            self.model_stats_status = "Open a database first".to_string();
            return;
        }
        if self.model_action_in_flight {
            return;
        }
        self.model_action_in_flight = true;
        self.model_stats_status = "Purging embeddings...".to_string();
        let pending = Arc::clone(&self.pending_model_action);
        std::thread::spawn(move || {
            let msg =
                match Database::open(std::path::Path::new(&db_path), ResourceProfile::detect()) {
                    Ok(db) => {
                        let conn = db.connection();
                        match conn.execute(
                            "DELETE FROM embeddings WHERE model_id = ?1",
                            [model_id.as_str()],
                        ) {
                            Ok(deleted) => format!("Purged embeddings: {}", deleted),
                            Err(err) => format!("Purge failed: {}", err),
                        }
                    }
                    Err(err) => format!("Purge failed: {}", err),
                };
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(msg);
            }
        });
    }

    fn start_model_delete(&mut self) {
        let db_path = self.db_path.trim().to_string();
        let model_id = match &self.selected_model_id {
            Some(id) => id.clone(),
            None => {
                self.model_stats_status = "Select a model first".to_string();
                return;
            }
        };
        if db_path.is_empty() {
            self.model_stats_status = "Open a database first".to_string();
            return;
        }
        if self.model_action_in_flight {
            return;
        }
        self.model_action_in_flight = true;
        self.model_stats_status = "Deleting model...".to_string();
        let pending = Arc::clone(&self.pending_model_action);
        std::thread::spawn(move || {
            let msg =
                match Database::open(std::path::Path::new(&db_path), ResourceProfile::detect()) {
                    Ok(db) => {
                        let conn = db.connection();
                        match conn
                            .execute("DELETE FROM ml_models WHERE id = ?1", [model_id.as_str()])
                        {
                            Ok(deleted) => format!("Deleted models: {}", deleted),
                            Err(err) => format!("Delete failed: {}", err),
                        }
                    }
                    Err(err) => format!("Delete failed: {}", err),
                };
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(msg);
            }
        });
    }

    fn start_reembed(&mut self) {
        if self.embed_job.is_some() {
            self.embed_status = "Embedding already running".to_string();
            return;
        }
        let model_id = self.selected_model_id.clone();
        if let Some(id) = model_id {
            let db_path = self.db_path.trim().to_string();
            if db_path.is_empty() {
                self.model_stats_status = "Open a database first".to_string();
                return;
            }
            if self.model_action_in_flight {
                return;
            }
            self.model_action_in_flight = true;
            self.model_stats_status = "Purging embeddings before re-embed...".to_string();
            let pending = Arc::clone(&self.pending_model_action);
            std::thread::spawn(move || {
                let msg =
                    match Database::open(std::path::Path::new(&db_path), ResourceProfile::detect())
                    {
                        Ok(db) => {
                            let conn = db.connection();
                            match conn.execute(
                                "DELETE FROM embeddings WHERE model_id = ?1",
                                [id.as_str()],
                            ) {
                                Ok(deleted) => format!("Purged embeddings: {}", deleted),
                                Err(err) => format!("Purge failed: {}", err),
                            }
                        }
                        Err(err) => format!("Purge failed: {}", err),
                    };
                if let Ok(mut lock) = pending.lock() {
                    *lock = Some(msg);
                }
            });
            self.embed_generation = self.embed_generation.wrapping_add(1);
        }
    }

    fn start_import(&mut self) {
        if self.import_input.trim().is_empty() {
            self.import_status = "Select an XML file".to_string();
            return;
        }
        if self.db_path.trim().is_empty() {
            self.import_status = "Select a DB path".to_string();
            return;
        }
        if std::path::Path::new(self.db_path.trim()).is_dir() {
            self.import_status = "Select or create a DB file (folder selected)".to_string();
            return;
        }
        if self.import_job.is_some() {
            return;
        }

        let inputs = self.parse_import_inputs();
        if inputs.is_empty() {
            self.import_status = "No XML files found in input".to_string();
            return;
        }
        let db_path = std::path::PathBuf::from(self.db_path.trim());
        let mut xml_sizes = Vec::new();
        let mut missing = Vec::new();
        for input in &inputs {
            if !input.exists() {
                missing.push(input.display().to_string());
                continue;
            }
            match std::fs::metadata(input) {
                Ok(meta) => xml_sizes.push(meta.len()),
                Err(err) => {
                    self.import_status = format!("Failed to read XML: {}", err);
                    return;
                }
            }
        }
        if !missing.is_empty() {
            self.import_status = format!("XML file(s) not found: {}", missing.join(", "));
            return;
        }
        let xml_size = xml_sizes.iter().copied().max().unwrap_or(0);
        let db_dir = db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        if let Ok(reqs) = calculate_minimum_resources(xml_size) {
            if let Ok(available) = available_disk_bytes(db_dir) {
                if available < reqs.min_disk {
                    self.import_status = format!(
                        "Insufficient disk: need ~{} GB, have {} GB",
                        reqs.min_disk / 1024_u64.pow(3),
                        available / 1024_u64.pow(3)
                    );
                    return;
                }
            }
            let resources = detect_resource_limits();
            if resources.total_ram_bytes < reqs.min_ram {
                self.import_status = format!(
                    "Low RAM ({} GB). Import may be slow.",
                    resources.total_ram_bytes / 1024_u64.pow(3)
                );
            }
        }
        let progress = Arc::new(IngestProgress::default());
        if let Ok(mut lock) = progress.error_samples.lock() {
            lock.clear();
        }
        if let Ok(mut lock) = progress.skipped_samples.lock() {
            lock.clear();
        }
        if let Ok(mut lock) = progress.current_file.lock() {
            *lock = None;
        }
        let progress_clone = Arc::clone(&progress);
        let resume = self.resume_from_checkpoint;
        let inputs_clone = inputs.clone();
        let handle = std::thread::spawn(move || {
            let start = Instant::now();
            let mut aggregated = sms_ingest::IngestStats::default();
            for input in inputs_clone.iter() {
                if progress_clone.cancelled.load(Ordering::Relaxed) {
                    return Err(sms_errors::AppError::Cancelled);
                }
                let opts = IngestOptions {
                    resume,
                    progress: Some(Arc::clone(&progress_clone)),
                    ..IngestOptions::default()
                };
                match ingest_file(input, &db_path, &opts) {
                    Ok(stats) => {
                        aggregated.messages_seen += stats.messages_seen;
                        aggregated.attachments_written += stats.attachments_written;
                        aggregated.parse_errors += stats.parse_errors;
                        aggregated.bytes_read += stats.bytes_read;
                        aggregated.messages_inserted += stats.messages_inserted;
                    }
                    Err(sms_errors::AppError::SkippedFile) => {
                        continue;
                    }
                    Err(err) => return Err(err),
                }
            }
            aggregated.elapsed_ms = start.elapsed().as_millis();
            Ok(aggregated)
        });
        self.import_job = Some(ImportJob {
            progress,
            handle,
            started_at: Instant::now(),
            inputs,
        });
        self.import_status = "Import started".to_string();
    }

    fn parse_import_inputs(&self) -> Vec<PathBuf> {
        self.import_input
            .split(['\n', ';'])
            .map(|raw| raw.trim())
            .filter(|raw| !raw.is_empty())
            .map(PathBuf::from)
            .collect()
    }

    fn refresh_checkpoint_if_needed(&mut self) {
        let db_path = self.db_path.trim();
        if db_path.is_empty() {
            return;
        }
        if self.checkpoint_db_path != db_path {
            self.refresh_checkpoint();
        }
    }

    fn refresh_checkpoint(&mut self) {
        let db_path = self.db_path.trim();
        self.checkpoint_db_path = db_path.to_string();
        if db_path.is_empty() {
            self.checkpoint_info = None;
            self.resume_from_checkpoint = false;
            return;
        }
        if std::path::Path::new(db_path).is_dir() {
            self.checkpoint_info = None;
            self.resume_from_checkpoint = false;
            return;
        }
        let cp_path = checkpoint_path_for(db_path);
        match fs::read(&cp_path) {
            Ok(data) => {
                if let Ok(info) = serde_json::from_slice::<CheckpointInfo>(&data) {
                    if info.last_committed_offset > 0 {
                        self.resume_from_checkpoint = true;
                        self.checkpoint_info = Some(info);
                        return;
                    }
                }
                self.checkpoint_info = None;
                self.resume_from_checkpoint = false;
            }
            Err(_) => {
                self.checkpoint_info = None;
                self.resume_from_checkpoint = false;
            }
        }
    }

    fn take_pending_results(&mut self) -> Option<Vec<Message>> {
        if let Ok(mut lock) = self.pending_results.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_thread_results(&mut self) -> Option<Vec<Message>> {
        if let Ok(mut lock) = self.pending_thread_results.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_semantic(&mut self) -> Option<Vec<SemanticHit>> {
        if let Ok(mut lock) = self.pending_semantic.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_model_stats(&mut self) -> Option<ModelStatsSnapshot> {
        if let Ok(mut lock) = self.pending_model_stats.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_model_action(&mut self) -> Option<String> {
        if let Ok(mut lock) = self.pending_model_action.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_contacts(&mut self) -> Option<ContactSnapshot> {
        if let Ok(mut lock) = self.pending_contacts.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_contact_detail(&mut self) -> Option<ContactDetail> {
        if let Ok(mut lock) = self.pending_contact_detail.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_contact_status(&mut self) -> Option<String> {
        if let Ok(mut lock) = self.pending_contact_status.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_duplicate_groups(&mut self) -> Option<Vec<Vec<ContactSummary>>> {
        if let Ok(mut lock) = self.pending_duplicate_groups.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_timeline(&mut self) -> Option<TimelineStats> {
        if let Ok(mut lock) = self.pending_timeline.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_map(&mut self) -> Option<Vec<MapPoint>> {
        if let Ok(mut lock) = self.pending_map.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_map_tiles(&mut self) -> Vec<MapTileUpdate> {
        if let Ok(mut lock) = self.pending_map_tiles.lock() {
            std::mem::take(&mut *lock)
        } else {
            Vec::new()
        }
    }

    fn take_pending_media(&mut self) -> Option<(Vec<AttachmentRow>, usize)> {
        if let Ok(mut lock) = self.pending_media.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_media_semantic(&mut self) -> Option<Vec<MediaSemanticHit>> {
        if let Ok(mut lock) = self.pending_media_semantic.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_fts_rebuild(&mut self) -> Option<String> {
        if let Ok(mut lock) = self.pending_fts_rebuild.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_ocr(&mut self) -> Vec<OcrUpdate> {
        if let Ok(mut lock) = self.pending_ocr.lock() {
            std::mem::take(&mut *lock)
        } else {
            Vec::new()
        }
    }

    fn take_pending_vision(&mut self) -> Vec<VisionUpdate> {
        if let Ok(mut lock) = self.pending_vision.lock() {
            std::mem::take(&mut *lock)
        } else {
            Vec::new()
        }
    }

    fn take_pending_nsfw(&mut self) -> Vec<NsfwUpdate> {
        if let Ok(mut lock) = self.pending_nsfw.lock() {
            std::mem::take(&mut *lock)
        } else {
            Vec::new()
        }
    }

    fn take_pending_ollama_models(&mut self) -> Option<Vec<OllamaModel>> {
        if let Ok(mut lock) = self.pending_ollama_models.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_ollama_log(&mut self) -> Vec<String> {
        if let Ok(mut lock) = self.pending_ollama_log.lock() {
            if lock.is_empty() {
                Vec::new()
            } else {
                let mut lines = Vec::new();
                lines.append(&mut *lock);
                lines
            }
        } else {
            Vec::new()
        }
    }

    fn take_pending_media_embed_status(&mut self) -> Vec<String> {
        if let Ok(mut lock) = self.pending_media_embed_status.lock() {
            if lock.is_empty() {
                Vec::new()
            } else {
                let mut lines = Vec::new();
                lines.append(&mut *lock);
                lines
            }
        } else {
            Vec::new()
        }
    }

    fn take_pending_media_embed_done(&mut self) -> Vec<String> {
        if let Ok(mut lock) = self.pending_media_embed_done.lock() {
            std::mem::take(&mut *lock)
        } else {
            Vec::new()
        }
    }

    fn take_pending_media_embed_inspect(&mut self) -> Option<Vec<MediaEmbedInspectRow>> {
        if let Ok(mut lock) = self.pending_media_embed_inspect.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_media_audit(&mut self) -> Option<MediaAuditSnapshot> {
        if let Ok(mut lock) = self.pending_media_audit.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_assistant(&mut self) -> Option<Result<Vec<sms_assistant::ChatMessage>>> {
        if let Ok(mut lock) = self.pending_assistant.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_assistant_model_check(&mut self) -> Option<String> {
        if let Ok(mut lock) = self.pending_assistant_model_check.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn take_pending_vision_model_check(&mut self) -> Option<String> {
        if let Ok(mut lock) = self.pending_vision_model_check.lock() {
            lock.take()
        } else {
            None
        }
    }

    fn append_ollama_log(&mut self, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        for line in lines {
            self.ollama_log.push(line.clone());
            self.ollama_status = line;
        }
        if self.ollama_log.len() > 50 {
            let drain = self.ollama_log.len().saturating_sub(50);
            self.ollama_log.drain(0..drain);
        }
    }

    fn start_embeddings(&mut self) {
        if self.db_path.trim().is_empty() {
            self.embed_status = "Select a DB path".to_string();
            return;
        }
        if self.embed_job.is_some() {
            return;
        }
        let use_ollama = self.use_ollama;
        let ollama_model = self.ollama_selected.trim().to_string();
        let ollama_base = self.ollama_base_url.trim().to_string();
        if use_ollama && ollama_model.is_empty() {
            self.embed_status = "Select an Ollama model first".to_string();
            return;
        }
        if use_ollama && ollama_base.is_empty() {
            self.embed_status = "Set Ollama base URL".to_string();
            return;
        }
        let (model_path, tokenizer_path) = if use_ollama {
            (None, None)
        } else {
            let model_path = {
                let trimmed = self.embed_model_path.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    let path = std::path::PathBuf::from(trimmed);
                    if !path.exists() {
                        self.embed_status = "Model path not found".to_string();
                        return;
                    }
                    Some(path)
                }
            };
            let tokenizer_path = {
                let trimmed = self.embed_tokenizer_path.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    let path = std::path::PathBuf::from(trimmed);
                    if !path.exists() {
                        self.embed_status = "Tokenizer path not found".to_string();
                        return;
                    }
                    Some(path)
                }
            };
            if model_path.is_some() && tokenizer_path.is_none() {
                self.embed_status = "Tokenizer path required for ONNX model".to_string();
                return;
            }
            (model_path, tokenizer_path)
        };
        let db_path = std::path::PathBuf::from(self.db_path.trim());
        let model_name = if use_ollama {
            ollama_model.clone()
        } else {
            self.embed_model_name.trim().to_string()
        };
        let model_version = if use_ollama {
            "ollama".to_string()
        } else {
            self.embed_model_version.trim().to_string()
        };
        let dims = self.embed_dimensions.max(8);
        let batch = self.embed_batch_size.max(1);
        let max_length = self.embed_max_length.max(8);
        let normalize = self.embed_normalize;
        let device = self.embed_device;
        let progress = Arc::new(EmbedProgress::default());
        let progress_clone = Arc::clone(&progress);
        let handle = std::thread::spawn(move || {
            if use_ollama {
                run_embed_job_ollama(
                    db_path,
                    ollama_base,
                    ollama_model,
                    model_name,
                    model_version,
                    batch,
                    progress_clone,
                )
            } else {
                run_embed_job(
                    db_path,
                    model_path,
                    tokenizer_path,
                    model_name,
                    model_version,
                    dims,
                    batch,
                    max_length,
                    normalize,
                    device,
                    progress_clone,
                )
            }
        });
        self.embed_job = Some(EmbedJob {
            progress,
            handle,
            started_at: Instant::now(),
        });
        self.embed_status = "Embedding started".to_string();
    }

    fn start_clip_processing(&mut self) {
        if self.db_path.trim().is_empty() {
            self.clip_status = "Select a DB path".to_string();
            return;
        }
        if self.clip_job.is_some() {
            return;
        }
        self.autofill_clip_paths();
        let clip_model = self.clip_model_path.trim();
        if clip_model.is_empty() {
            self.clip_status = "Select a CLIP model path".to_string();
            return;
        }
        let nsfw_weights = self.clip_nsfw_weights_path.trim();
        if nsfw_weights.is_empty() {
            self.clip_status = "Select NSFW weights path".to_string();
            return;
        }
        let model_path = PathBuf::from(clip_model);
        if !model_path.exists() {
            self.clip_status = "CLIP model path not found".to_string();
            return;
        }
        let nsfw_path = PathBuf::from(nsfw_weights);
        if !nsfw_path.exists() {
            self.clip_status = "NSFW weights path not found".to_string();
            return;
        }
        let db_path = PathBuf::from(self.db_path.trim());
        let media_root = self.media_root.clone();
        let batch_size = self.clip_batch_size.max(1);
        let max_keyframes = self.clip_max_keyframes.max(1);
        let workers = self.clip_workers.max(1);
        let reprocess = self.clip_reprocess;
        let use_cuda = self.clip_use_cuda;
        let progress = Arc::new(ClipProgress::default());
        let cancel_flag = Arc::clone(&progress.cancelled);
        let pause_flag = Arc::clone(&progress.paused);
        let gps_in_progress = Arc::clone(&progress.gps_in_progress);
        let gps_tagged = Arc::clone(&progress.gps_tagged);
        let progress_total = Arc::clone(&progress.total);
        let progress_done = Arc::clone(&progress.done);
        // Run GPS tagging in parallel — don't block CLIP processing
        {
            let gps_db = db_path.clone();
            let gps_media = media_root.clone();
            std::thread::spawn(move || {
                gps_in_progress.store(true, Ordering::Relaxed);
                let db_str = gps_db.to_string_lossy().to_string();
                let count = tag_gps_cache(&db_str, gps_media).unwrap_or(0);
                gps_tagged.store(count as u64, Ordering::Relaxed);
                gps_in_progress.store(false, Ordering::Relaxed);
            });
        }
        let handle = std::thread::spawn(move || {
            std::env::set_var("SMS_CLIP_USE_CUDA", if use_cuda { "1" } else { "0" });
            let options = MediaProcessOptions {
                db_path,
                clip_model: model_path,
                nsfw_weights: nsfw_path,
                batch_size,
                max_keyframes,
                reprocess,
                limit: None,
                workers,
                show_progress: false,
                media_root,
                cancel_flag: Some(cancel_flag),
                pause_flag: Some(pause_flag),
                progress_total: Some(progress_total),
                progress_done: Some(progress_done),
            };
            process_media(&options)
        });
        self.clip_job = Some(MediaProcessJob {
            progress,
            handle,
            started_at: Instant::now(),
        });
        self.clip_status = "CLIP processing started".to_string();
    }

    fn maybe_start_clip_after_import(&mut self) {
        if !self.clip_auto_on_import {
            return;
        }
        if self.clip_job.is_some() {
            return;
        }
        if self.clip_model_path.trim().is_empty() || self.clip_nsfw_weights_path.trim().is_empty() {
            self.clip_status =
                "CLIP auto-run skipped: set CLIP model and NSFW weights first".to_string();
            return;
        }
        // #todo: allow scheduling CLIP processing after idle time or on demand.
        self.start_clip_processing();
    }

    fn start_gps_tagging_after_import(&mut self) {
        if self.db_path.trim().is_empty() {
            return;
        }
        if self.clip_auto_on_import {
            return;
        }
        let db_path = self.db_path.clone();
        let media_root = self.media_root.clone();
        std::thread::spawn(move || {
            let _ = tag_gps_cache(&db_path, media_root);
        });
        // #todo: surface GPS tagging progress in the import status.
    }

    fn force_reset_import(&mut self) {
        if let Some(job) = self.import_job.take() {
            job.progress.cancelled.store(true, Ordering::Relaxed);
            if job.handle.is_finished() {
                let _ = job.handle.join();
            }
        }
        self.import_status = "Import reset (ready to restart)".to_string();
        self.import_last_offset = 0;
        self.import_last_update = Instant::now();
        self.import_total_paused = Duration::ZERO;
        self.import_pause_start = None;
    }

    /// Return a ready thumbnail texture for an attachment, or `None` while it
    /// decodes in the background. Decoding (and ffmpeg/HEIC thumbnail
    /// generation) happens on worker threads — previously this blocked the UI
    /// thread with `image::open` and ffmpeg shell-outs inside the render pass,
    /// the single biggest source of media-browsing jank.
    fn thumbnail_for(
        &mut self,
        file_path: Option<&std::path::PathBuf>,
        thumb_path: Option<&std::path::PathBuf>,
        mime_type: &str,
    ) -> Option<egui::TextureHandle> {
        let key = thumb_path.or(file_path)?.to_string_lossy().to_string();
        if let Some(entry) = self.thumbnail_cache.get(&key) {
            return Some(entry.clone());
        }
        // A DB-provided thumbnail is decoded directly; otherwise we need the
        // preview cache dir to generate one into.
        let cache_dir = if thumb_path.is_some() {
            std::path::PathBuf::new()
        } else {
            self.ensure_preview_cache()?.dir.clone()
        };
        self.thumbnail_loader.request(ThumbJob {
            key,
            file_path: file_path.cloned(),
            thumb_path: thumb_path.cloned(),
            mime: mime_type.to_string(),
            cache_dir,
        });
        None
    }

    /// Drain decoded thumbnails, upload them to GPU textures, and keep the
    /// preview cache bounded. Called once per frame.
    fn pump_thumbnails(&mut self, ctx: &egui::Context) {
        let ready: Vec<ThumbReady> = self.thumbnail_loader.rx.try_iter().collect();
        let mut uploaded = false;
        for item in ready {
            match item {
                ThumbReady::Ok(key, color) => {
                    let texture =
                        ctx.load_texture(key.clone(), color, egui::TextureOptions::LINEAR);
                    self.thumbnail_cache.put(key.clone(), texture);
                    self.thumbnail_loader.inflight.remove(&key);
                    uploaded = true;
                }
                ThumbReady::Fail(key) => {
                    self.thumbnail_loader.inflight.remove(&key);
                    self.thumbnail_loader.failed.insert(key);
                }
            }
        }
        // Keep polling while work is outstanding so results surface promptly
        // even if the pointer is still.
        if uploaded || !self.thumbnail_loader.inflight.is_empty() {
            ctx.request_repaint();
        }
        // Workers write preview files directly, so prune by rescanning the
        // directory rather than tracking bytes inline.
        if uploaded {
            if let Some(cache) = self.preview_cache.as_mut() {
                if cache.last_scan.elapsed() > Duration::from_secs(20) {
                    cache.rescan();
                    cache.prune_if_needed();
                }
            }
        }
    }

    fn ensure_preview_cache(&mut self) -> Option<&mut PreviewCache> {
        let db_path = self.db_path.trim();
        if db_path.is_empty() {
            return None;
        }
        if self.preview_cache_db_path != db_path {
            self.preview_cache = None;
            self.preview_cache_db_path = db_path.to_string();
        }
        if self.preview_cache.is_none() {
            let dir = self.media_root_dir().join("previews");
            let _ = fs::create_dir_all(&dir);
            let mut cache = PreviewCache::new(dir, 256 * 1024 * 1024);
            cache.rescan();
            self.preview_cache = Some(cache);
        }
        self.preview_cache.as_mut()
    }
}

fn run_paged_fts_filtered(
    db_path: &str,
    query: &str,
    limit: usize,
    offset: usize,
    filters: &SearchFilters,
    max_results: Option<usize>,
) -> sms_errors::Result<Vec<Message>> {
    let backend = Fts5Backend::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = backend.connection();
    let mut sql = String::from(
        "SELECT messages.id, messages.message_id, messages.timestamp, messages.address, \
                messages.body, messages.body_searchable, messages.message_type, messages.message_direction, messages.thread_id, messages.contact_name \
         FROM messages_fts \
         JOIN messages ON messages.rowid = messages_fts.rowid \
         WHERE messages_fts MATCH ?",
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    params.push(sms_search::sanitize_fts5_query(query).into());

    if !filters.address.trim().is_empty() {
        sql.push_str(" AND messages.address = ?");
        params.push(filters.address.trim().to_string().into());
    }
    if !filters.thread_id.trim().is_empty() {
        sql.push_str(" AND messages.thread_id = ?");
        params.push(filters.thread_id.trim().to_string().into());
    }
    if let Some(message_type) = filters.message_type.as_i32() {
        sql.push_str(" AND messages.message_type = ?");
        params.push(message_type.into());
    }
    if let Some(since) = parse_filter_ms(&filters.since, false) {
        sql.push_str(" AND messages.timestamp >= ?");
        params.push(since.into());
    }
    if let Some(until) = parse_filter_ms(&filters.until, true) {
        sql.push_str(" AND messages.timestamp <= ?");
        params.push(until.into());
    }

    // Rank before truncating — without ORDER BY, LIMIT keeps an arbitrary
    // subset and the best matches may never be shown.
    sql.push_str(" ORDER BY bm25(messages_fts)");

    // Page within the optional overall cap. Previously a set cap emitted
    // `LIMIT max` with no OFFSET at all, so Next/Prev re-fetched the same
    // rows on every page.
    let effective_limit = match max_results {
        Some(max) => limit.min(max.saturating_sub(offset)),
        None => limit,
    };
    if effective_limit == 0 {
        return Ok(Vec::new());
    }
    sql.push_str(" LIMIT ? OFFSET ?");
    params.push((effective_limit as i64).into());
    params.push((offset as i64).into());

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        let message_type: i32 = row.get(6)?;
        let message_direction: i32 = row.get(7)?;
        Ok(Message {
            id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?)
                .unwrap_or_else(|_| uuid::Uuid::new_v4()),
            message_id: row.get(1)?,
            dedupe_hash: None,
            timestamp: row.get(2)?,
            address: row.get(3)?,
            body: row.get(4)?,
            body_searchable: row.get(5)?,
            message_type: match message_type {
                2 => sms_types::MessageType::Mms,
                3 => sms_types::MessageType::Rcs,
                _ => sms_types::MessageType::Sms,
            },
            direction: sms_types::MessageDirection::from_i32(message_direction),
            thread_id: row.get(8)?,
            attachments: Vec::new(),
            contact_name: row.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn load_thread_messages(
    db_path: &str,
    thread_id: &str,
    limit: usize,
) -> sms_errors::Result<Vec<Message>> {
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    let mut sql = String::from(
        "SELECT id, message_id, timestamp, address, body, body_searchable, message_type, message_direction, thread_id, contact_name \
         FROM messages WHERE thread_id = ?1 ORDER BY timestamp",
    );
    if limit > 0 {
        sql.push_str(" LIMIT ?2");
    }
    let mut out = Vec::new();
    if limit > 0 {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![thread_id, limit as i64], |row| {
            let message_type: i32 = row.get(6)?;
            let message_direction: i32 = row.get(7)?;
            Ok(Message {
                id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?)
                    .unwrap_or_else(|_| uuid::Uuid::new_v4()),
                message_id: row.get(1)?,
                dedupe_hash: None,
                timestamp: row.get(2)?,
                address: row.get(3)?,
                body: row.get(4)?,
                body_searchable: row.get(5)?,
                message_type: match message_type {
                    2 => sms_types::MessageType::Mms,
                    3 => sms_types::MessageType::Rcs,
                    _ => sms_types::MessageType::Sms,
                },
                direction: sms_types::MessageDirection::from_i32(message_direction),
                thread_id: row.get(8)?,
                attachments: Vec::new(),
                contact_name: row.get(9)?,
            })
        })?;
        for r in rows {
            out.push(r?);
        }
    } else {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![thread_id], |row| {
            let message_type: i32 = row.get(6)?;
            let message_direction: i32 = row.get(7)?;
            Ok(Message {
                id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?)
                    .unwrap_or_else(|_| uuid::Uuid::new_v4()),
                message_id: row.get(1)?,
                dedupe_hash: None,
                timestamp: row.get(2)?,
                address: row.get(3)?,
                body: row.get(4)?,
                body_searchable: row.get(5)?,
                message_type: match message_type {
                    2 => sms_types::MessageType::Mms,
                    3 => sms_types::MessageType::Rcs,
                    _ => sms_types::MessageType::Sms,
                },
                direction: sms_types::MessageDirection::from_i32(message_direction),
                thread_id: row.get(8)?,
                attachments: Vec::new(),
                contact_name: row.get(9)?,
            })
        })?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

fn load_thread_window(
    db_path: &str,
    thread_id: &str,
    anchor_id: uuid::Uuid,
    window: usize,
) -> sms_errors::Result<Vec<Message>> {
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    let anchor_ts: i64 = conn
        .query_row(
            "SELECT timestamp FROM messages WHERE id = ?1",
            params![anchor_id.to_string()],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut stmt_before = conn.prepare(
        "SELECT id, message_id, timestamp, address, body, body_searchable, message_type, message_direction, thread_id, contact_name \
         FROM messages WHERE thread_id = ?1 AND timestamp <= ?2 \
         ORDER BY timestamp DESC LIMIT ?3",
    )?;
    let rows = stmt_before.query_map(params![thread_id, anchor_ts, window as i64], |row| {
        let message_type: i32 = row.get(6)?;
        let message_direction: i32 = row.get(7)?;
        Ok(Message {
            id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?)
                .unwrap_or_else(|_| uuid::Uuid::new_v4()),
            message_id: row.get(1)?,
            dedupe_hash: None,
            timestamp: row.get(2)?,
            address: row.get(3)?,
            body: row.get(4)?,
            body_searchable: row.get(5)?,
            message_type: match message_type {
                2 => sms_types::MessageType::Mms,
                3 => sms_types::MessageType::Rcs,
                _ => sms_types::MessageType::Sms,
            },
            direction: sms_types::MessageDirection::from_i32(message_direction),
            thread_id: row.get(8)?,
            attachments: Vec::new(),
            contact_name: row.get(9)?,
        })
    })?;
    for r in rows {
        before.push(r?);
    }
    before.reverse();

    let mut stmt_after = conn.prepare(
        "SELECT id, message_id, timestamp, address, body, body_searchable, message_type, message_direction, thread_id, contact_name \
         FROM messages WHERE thread_id = ?1 AND timestamp > ?2 \
         ORDER BY timestamp ASC LIMIT ?3",
    )?;
    let rows = stmt_after.query_map(params![thread_id, anchor_ts, window as i64], |row| {
        let message_type: i32 = row.get(6)?;
        let message_direction: i32 = row.get(7)?;
        Ok(Message {
            id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?)
                .unwrap_or_else(|_| uuid::Uuid::new_v4()),
            message_id: row.get(1)?,
            dedupe_hash: None,
            timestamp: row.get(2)?,
            address: row.get(3)?,
            body: row.get(4)?,
            body_searchable: row.get(5)?,
            message_type: match message_type {
                2 => sms_types::MessageType::Mms,
                3 => sms_types::MessageType::Rcs,
                _ => sms_types::MessageType::Sms,
            },
            direction: sms_types::MessageDirection::from_i32(message_direction),
            thread_id: row.get(8)?,
            attachments: Vec::new(),
            contact_name: row.get(9)?,
        })
    })?;
    for r in rows {
        after.push(r?);
    }

    before.extend(after);
    Ok(before)
}

fn load_address_messages(
    db_path: &str,
    address: &str,
    limit: usize,
) -> sms_errors::Result<Vec<Message>> {
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    let mut sql = String::from(
        "SELECT id, message_id, timestamp, address, body, body_searchable, message_type, message_direction, thread_id, contact_name \
         FROM messages WHERE address = ?1 ORDER BY timestamp",
    );
    if limit > 0 {
        sql.push_str(" LIMIT ?2");
    }
    let mut out = Vec::new();
    if limit > 0 {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![address, limit as i64], |row| {
            let message_type: i32 = row.get(6)?;
            let message_direction: i32 = row.get(7)?;
            Ok(Message {
                id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?)
                    .unwrap_or_else(|_| uuid::Uuid::new_v4()),
                message_id: row.get(1)?,
                dedupe_hash: None,
                timestamp: row.get(2)?,
                address: row.get(3)?,
                body: row.get(4)?,
                body_searchable: row.get(5)?,
                message_type: match message_type {
                    2 => sms_types::MessageType::Mms,
                    3 => sms_types::MessageType::Rcs,
                    _ => sms_types::MessageType::Sms,
                },
                direction: sms_types::MessageDirection::from_i32(message_direction),
                thread_id: row.get(8)?,
                attachments: Vec::new(),
                contact_name: row.get(9)?,
            })
        })?;
        for r in rows {
            out.push(r?);
        }
    } else {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![address], |row| {
            let message_type: i32 = row.get(6)?;
            let message_direction: i32 = row.get(7)?;
            Ok(Message {
                id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?)
                    .unwrap_or_else(|_| uuid::Uuid::new_v4()),
                message_id: row.get(1)?,
                dedupe_hash: None,
                timestamp: row.get(2)?,
                address: row.get(3)?,
                body: row.get(4)?,
                body_searchable: row.get(5)?,
                message_type: match message_type {
                    2 => sms_types::MessageType::Mms,
                    3 => sms_types::MessageType::Rcs,
                    _ => sms_types::MessageType::Sms,
                },
                direction: sms_types::MessageDirection::from_i32(message_direction),
                thread_id: row.get(8)?,
                attachments: Vec::new(),
                contact_name: row.get(9)?,
            })
        })?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

fn load_address_window(
    db_path: &str,
    address: &str,
    anchor_id: uuid::Uuid,
    window: usize,
) -> sms_errors::Result<Vec<Message>> {
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    let anchor_ts: i64 = conn
        .query_row(
            "SELECT timestamp FROM messages WHERE id = ?1",
            params![anchor_id.to_string()],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut stmt_before = conn.prepare(
        "SELECT id, message_id, timestamp, address, body, body_searchable, message_type, message_direction, thread_id, contact_name \
         FROM messages WHERE address = ?1 AND timestamp <= ?2 \
         ORDER BY timestamp DESC LIMIT ?3",
    )?;
    let rows = stmt_before.query_map(params![address, anchor_ts, window as i64], |row| {
        let message_type: i32 = row.get(6)?;
        let message_direction: i32 = row.get(7)?;
        Ok(Message {
            id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?)
                .unwrap_or_else(|_| uuid::Uuid::new_v4()),
            message_id: row.get(1)?,
            dedupe_hash: None,
            timestamp: row.get(2)?,
            address: row.get(3)?,
            body: row.get(4)?,
            body_searchable: row.get(5)?,
            message_type: match message_type {
                2 => sms_types::MessageType::Mms,
                3 => sms_types::MessageType::Rcs,
                _ => sms_types::MessageType::Sms,
            },
            direction: sms_types::MessageDirection::from_i32(message_direction),
            thread_id: row.get(8)?,
            attachments: Vec::new(),
            contact_name: row.get(9)?,
        })
    })?;
    for r in rows {
        before.push(r?);
    }
    before.reverse();

    let mut stmt_after = conn.prepare(
        "SELECT id, message_id, timestamp, address, body, body_searchable, message_type, message_direction, thread_id, contact_name \
         FROM messages WHERE address = ?1 AND timestamp > ?2 \
         ORDER BY timestamp ASC LIMIT ?3",
    )?;
    let rows = stmt_after.query_map(params![address, anchor_ts, window as i64], |row| {
        let message_type: i32 = row.get(6)?;
        let message_direction: i32 = row.get(7)?;
        Ok(Message {
            id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?)
                .unwrap_or_else(|_| uuid::Uuid::new_v4()),
            message_id: row.get(1)?,
            dedupe_hash: None,
            timestamp: row.get(2)?,
            address: row.get(3)?,
            body: row.get(4)?,
            body_searchable: row.get(5)?,
            message_type: match message_type {
                2 => sms_types::MessageType::Mms,
                3 => sms_types::MessageType::Rcs,
                _ => sms_types::MessageType::Sms,
            },
            direction: sms_types::MessageDirection::from_i32(message_direction),
            thread_id: row.get(8)?,
            attachments: Vec::new(),
            contact_name: row.get(9)?,
        })
    })?;
    for r in rows {
        after.push(r?);
    }

    before.extend(after);
    Ok(before)
}

fn load_contact_detail_sync(db_path: &str, contact_id: &str) -> Option<ContactDetail> {
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect()).ok()?;
    let conn = db.connection();
    let mut stmt = conn
        .prepare(
            "SELECT id, display_name, nickname, company, notes, email, phone_primary, \
                phone_secondary, phone_primary_type, phone_secondary_type, website, social_media, \
                address, birthday, avatar_path, last_contacted, favorite \
         FROM contacts WHERE id = ?1",
        )
        .ok()?;
    let mut detail = stmt
        .query_row(params![contact_id], |row| {
            Ok(ContactDetail {
                id: row.get(0)?,
                display_name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                nickname: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                company: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                notes: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                email: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                phone_primary: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                phone_secondary: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                phone_primary_type: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
                phone_secondary_type: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                website: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
                social_media: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
                address: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
                birthday: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
                avatar_path: row.get::<_, Option<String>>(14)?.unwrap_or_default(),
                last_contacted: row.get::<_, Option<i64>>(15)?,
                favorite: row.get::<_, Option<i64>>(16)?.unwrap_or(0) != 0,
                addresses: Vec::new(),
            })
        })
        .ok()?;
    if let Ok(mut stmt) =
        conn.prepare("SELECT address FROM contact_addresses WHERE contact_id = ?1")
    {
        if let Ok(rows) = stmt.query_map(params![contact_id], |row| row.get(0)) {
            for row in rows.flatten() {
                detail.addresses.push(row);
            }
        }
    }
    Some(detail)
}

fn lookup_contact_id_by_address(db_path: &str, address: &str) -> Option<String> {
    if address.trim().is_empty() {
        return None;
    }
    let db = Database::open(Path::new(db_path), ResourceProfile::detect()).ok()?;
    let conn = db.connection();
    let addr = address.trim();
    conn.query_row(
        "SELECT id FROM contacts WHERE phone_primary = ?1 OR phone_secondary = ?1 OR address = ?1 OR email = ?1 LIMIT 1",
        params![addr],
        |row| row.get(0),
    )
    .optional()
    .ok()
    .flatten()
    .or_else(|| {
        conn.query_row(
            "SELECT contact_id FROM contact_addresses WHERE address = ?1 LIMIT 1",
            params![addr],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten()
    })
}

fn primary_contact_address(detail: &ContactDetail) -> Option<String> {
    if !detail.phone_primary.trim().is_empty() {
        return Some(detail.phone_primary.trim().to_string());
    }
    if !detail.address.trim().is_empty() {
        return Some(detail.address.trim().to_string());
    }
    detail.addresses.first().cloned()
}

fn load_contact_snapshot(db_path: &str, query: &str) -> ContactSnapshot {
    let db = match Database::open(std::path::Path::new(db_path), ResourceProfile::detect()) {
        Ok(db) => db,
        Err(_) => return ContactSnapshot::default(),
    };
    let conn = db.connection();
    let mut contacts = Vec::new();
    let like = format!("%{}%", query);
    let mut stmt = match conn.prepare(
        "SELECT id, display_name, phone_primary \
         FROM contacts \
         WHERE display_name LIKE ?1 OR phone_primary LIKE ?1 OR email LIKE ?1 \
         ORDER BY display_name",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return ContactSnapshot::default(),
    };
    let rows = stmt.query_map(params![like], |row| {
        Ok(ContactSummary {
            id: row.get(0)?,
            display_name: row.get(1)?,
            primary: row.get(2)?,
        })
    });
    if let Ok(rows) = rows {
        for row in rows.flatten() {
            contacts.push(row);
        }
    }
    let mut address_map = HashMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT contact_addresses.address, contacts.display_name \
         FROM contact_addresses \
         JOIN contacts ON contacts.id = contact_addresses.contact_id",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            for row in rows.flatten() {
                address_map.insert(row.0, row.1);
            }
        }
    }
    ContactSnapshot {
        contacts,
        address_map,
    }
}

fn export_contacts_to_vcf(db_path: &str, path: &Path) -> Result<usize> {
    let contacts = load_all_contact_details(db_path)?;
    let mut file = std::fs::File::create(path)?;
    for contact in &contacts {
        writeln!(file, "BEGIN:VCARD")?;
        writeln!(file, "VERSION:3.0")?;
        if !contact.display_name.is_empty() {
            writeln!(file, "FN:{}", vcard_escape(&contact.display_name))?;
        }
        if !contact.nickname.is_empty() {
            writeln!(file, "NICKNAME:{}", vcard_escape(&contact.nickname))?;
        }
        if !contact.company.is_empty() {
            writeln!(file, "ORG:{}", vcard_escape(&contact.company))?;
        }
        if !contact.notes.is_empty() {
            writeln!(file, "NOTE:{}", vcard_escape(&contact.notes))?;
        }
        if !contact.email.is_empty() {
            writeln!(file, "EMAIL:{}", vcard_escape(&contact.email))?;
        }
        if !contact.phone_primary.is_empty() {
            writeln!(
                file,
                "TEL;TYPE={}:{}",
                vcard_phone_type_label(&contact.phone_primary_type),
                vcard_escape(&contact.phone_primary)
            )?;
        }
        if !contact.phone_secondary.is_empty() {
            writeln!(
                file,
                "TEL;TYPE={}:{}",
                vcard_phone_type_label(&contact.phone_secondary_type),
                vcard_escape(&contact.phone_secondary)
            )?;
        }
        if !contact.website.is_empty() {
            writeln!(file, "URL:{}", vcard_escape(&contact.website))?;
        }
        if !contact.social_media.is_empty() {
            writeln!(
                file,
                "X-SOCIALPROFILE:{}",
                vcard_escape(&contact.social_media)
            )?;
        }
        if !contact.address.is_empty() {
            writeln!(file, "ADR:;;{};;;;", vcard_escape(&contact.address))?;
        }
        if !contact.birthday.is_empty() {
            writeln!(file, "BDAY:{}", vcard_escape(&contact.birthday))?;
        }
        if contact.favorite {
            writeln!(file, "X-FAVORITE:1")?;
        }
        writeln!(file, "END:VCARD")?;
    }
    Ok(contacts.len())
}

fn export_contacts_to_csv(db_path: &str, path: &Path) -> Result<usize> {
    let contacts = load_all_contact_details(db_path)?;
    let mut wtr = csv::Writer::from_path(path)?;
    wtr.write_record([
        "ID",
        "Name",
        "Nickname",
        "Company",
        "Email",
        "Phone Primary",
        "Phone Primary Type",
        "Phone Secondary",
        "Phone Secondary Type",
        "Website",
        "Social Media",
        "Address",
        "Birthday",
        "Notes",
        "Last Contacted",
        "Favorite",
    ])?;
    for contact in &contacts {
        let last_contacted = contact
            .last_contacted
            .map(|ts| ts.to_string())
            .unwrap_or_default();
        let favorite = if contact.favorite { "true" } else { "false" };
        wtr.write_record([
            contact.id.as_str(),
            contact.display_name.as_str(),
            contact.nickname.as_str(),
            contact.company.as_str(),
            contact.email.as_str(),
            contact.phone_primary.as_str(),
            contact.phone_primary_type.as_str(),
            contact.phone_secondary.as_str(),
            contact.phone_secondary_type.as_str(),
            contact.website.as_str(),
            contact.social_media.as_str(),
            contact.address.as_str(),
            contact.birthday.as_str(),
            contact.notes.as_str(),
            last_contacted.as_str(),
            favorite,
        ])?;
    }
    wtr.flush()?;
    Ok(contacts.len())
}

fn import_contacts_from_vcf(path: &Path) -> Result<Vec<ContactDetail>> {
    let contents = std::fs::read_to_string(path)?;
    let lines = unfold_vcard_lines(&contents);
    let mut contacts = Vec::new();
    let mut current: Option<ContactDetail> = None;
    for line in lines {
        if line.eq_ignore_ascii_case("BEGIN:VCARD") {
            current = Some(blank_contact_detail());
            continue;
        }
        if line.eq_ignore_ascii_case("END:VCARD") {
            if let Some(contact) = current.take() {
                contacts.push(normalize_contact_for_import(contact));
            }
            continue;
        }
        let Some(contact) = current.as_mut() else {
            continue;
        };
        let Some((raw_key, raw_value)) = line.split_once(':') else {
            continue;
        };
        let (name, types) = parse_vcard_property(raw_key);
        let value = vcard_unescape(raw_value.trim());
        match name.as_str() {
            "FN" => {
                if contact.display_name.is_empty() {
                    contact.display_name = value;
                }
            }
            "N" => {
                if contact.display_name.is_empty() {
                    contact.display_name = format_vcard_name(&value);
                }
            }
            "NICKNAME" => contact.nickname = value,
            "ORG" => contact.company = value,
            "NOTE" => contact.notes = value,
            "EMAIL" => {
                if contact.email.is_empty() {
                    contact.email = value;
                }
            }
            "TEL" => {
                let phone_type = vcard_phone_type_from_params(&types);
                if contact.phone_primary.is_empty() {
                    contact.phone_primary = value;
                    if !phone_type.is_empty() {
                        contact.phone_primary_type = phone_type;
                    }
                } else if contact.phone_secondary.is_empty() {
                    contact.phone_secondary = value;
                    if !phone_type.is_empty() {
                        contact.phone_secondary_type = phone_type;
                    }
                }
            }
            "URL" => contact.website = value,
            "X-SOCIALPROFILE" => contact.social_media = value,
            "ADR" => contact.address = format_vcard_address(&value),
            "BDAY" => contact.birthday = value,
            "X-FAVORITE" => {
                let normalized = value.trim().to_ascii_lowercase();
                contact.favorite = matches!(normalized.as_str(), "1" | "true" | "yes")
            }
            _ => {}
        }
    }
    Ok(contacts)
}

fn import_contacts_from_csv(path: &Path) -> Result<Vec<ContactDetail>> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_path(path)?;
    let headers = rdr.headers()?.clone();
    let header_map = build_csv_header_map(&headers);
    let mut contacts = Vec::new();
    for result in rdr.records() {
        let record = result?;
        let mut contact = blank_contact_detail();
        contact.id = record_value(&record, &header_map, &["id", "contactid", "uuid"]);
        contact.display_name = record_value(
            &record,
            &header_map,
            &["name", "displayname", "fullname", "fn"],
        );
        contact.nickname = record_value(&record, &header_map, &["nickname", "nick"]);
        contact.company = record_value(&record, &header_map, &["company", "org", "organization"]);
        contact.email = record_value(&record, &header_map, &["email", "emailaddress"]);
        contact.phone_primary = record_value(
            &record,
            &header_map,
            &["phoneprimary", "primaryphone", "phone1"],
        );
        contact.phone_primary_type = record_value(
            &record,
            &header_map,
            &["phoneprimarytype", "primaryphonetype", "phone1type"],
        );
        contact.phone_secondary = record_value(
            &record,
            &header_map,
            &["phonesecondary", "secondaryphone", "phone2"],
        );
        contact.phone_secondary_type = record_value(
            &record,
            &header_map,
            &["phonesecondarytype", "secondaryphonetype", "phone2type"],
        );
        contact.website = record_value(&record, &header_map, &["website", "url"]);
        contact.social_media = record_value(
            &record,
            &header_map,
            &["socialmedia", "social", "socialprofile"],
        );
        contact.address = record_value(&record, &header_map, &["address", "street"]);
        contact.birthday = record_value(&record, &header_map, &["birthday", "bday"]);
        contact.notes = record_value(&record, &header_map, &["notes", "note"]);
        contact.last_contacted = parse_optional_timestamp(&record_value(
            &record,
            &header_map,
            &["lastcontacted", "last_contacted", "lastcontact"],
        ));
        contact.favorite = parse_bool_field(&record_value(
            &record,
            &header_map,
            &["favorite", "favourite", "starred"],
        ));
        contacts.push(normalize_contact_for_import(contact));
    }
    Ok(contacts)
}

fn import_contacts_from_xml(db_path: &str, xml_path: &Path) -> Result<usize> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let file = std::fs::File::open(xml_path)?;
    let reader = std::io::BufReader::new(file);
    let mut xml = Reader::from_reader(reader);
    xml.trim_text(true);
    let mut buf = Vec::new();
    let mut contacts: HashMap<String, ContactDetail> = HashMap::new();

    loop {
        match xml.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let tag_lc = tag.to_ascii_lowercase();
                let mut attrs: HashMap<String, String> = HashMap::new();
                for attr in e.attributes().with_checks(false) {
                    let attr = attr.map_err(|err| {
                        sms_errors::AppError::External(format!("XML attr error: {}", err))
                    })?;
                    let key = String::from_utf8_lossy(attr.key.as_ref())
                        .to_string()
                        .to_ascii_lowercase();
                    let value = attr
                        .unescape_value()
                        .map_err(|err| {
                            sms_errors::AppError::External(format!("XML decode error: {}", err))
                        })?
                        .to_string();
                    attrs.insert(key, value);
                }

                if matches!(tag_lc.as_str(), "sms" | "mms" | "rcs" | "message") {
                    let address_raw = attrs.get("address").cloned().unwrap_or_default();
                    let mut name = attrs
                        .get("contact_name")
                        .cloned()
                        .or_else(|| attrs.get("contact").cloned())
                        .or_else(|| attrs.get("name").cloned())
                        .unwrap_or_default();
                    if is_nullish_value(&name) {
                        name.clear();
                    }
                    let normalized = normalize_phone_like(&address_raw);
                    let address_key = if !normalized.is_empty() {
                        normalized.clone()
                    } else {
                        address_raw.trim().to_string()
                    };
                    if is_nullish_value(&address_raw) && name.trim().is_empty() {
                        buf.clear();
                        continue;
                    }
                    let entry = contacts
                        .entry(address_key.clone())
                        .or_insert_with(blank_contact_detail);
                    if entry.display_name.trim().is_empty()
                        || entry.display_name == entry.phone_primary
                        || entry.display_name == "Unknown"
                    {
                        if !name.trim().is_empty() {
                            entry.display_name = name.clone();
                        } else if entry.display_name.trim().is_empty() {
                            entry.display_name = address_key.clone();
                        }
                    }
                    if entry.phone_primary.trim().is_empty() {
                        entry.phone_primary = if !normalized.is_empty() {
                            normalized.clone()
                        } else {
                            address_raw.trim().to_string()
                        };
                    }
                    let address_clean = address_raw.trim().to_string();
                    if !address_clean.is_empty() && !is_nullish_value(&address_clean) {
                        if !entry.addresses.contains(&address_clean) {
                            entry.addresses.push(address_clean.clone());
                        }
                        if !normalized.is_empty()
                            && normalized != address_clean
                            && !entry.addresses.contains(&normalized)
                        {
                            entry.addresses.push(normalized.clone());
                        }
                    }
                }

                if matches!(tag_lc.as_str(), "contact" | "person") {
                    let mut detail = blank_contact_detail();
                    detail.display_name = attrs
                        .get("display_name")
                        .cloned()
                        .or_else(|| attrs.get("displayname").cloned())
                        .or_else(|| attrs.get("contact_name").cloned())
                        .or_else(|| attrs.get("name").cloned())
                        .unwrap_or_default();
                    let first = attrs
                        .get("first_name")
                        .cloned()
                        .or_else(|| attrs.get("given_name").cloned())
                        .unwrap_or_default();
                    let last = attrs
                        .get("last_name")
                        .cloned()
                        .or_else(|| attrs.get("family_name").cloned())
                        .unwrap_or_default();
                    if detail.display_name.trim().is_empty()
                        && (!first.trim().is_empty() || !last.trim().is_empty())
                    {
                        detail.display_name = format!("{} {}", first, last).trim().to_string();
                    }
                    detail.nickname = attrs.get("nickname").cloned().unwrap_or_default();
                    detail.company = attrs
                        .get("company")
                        .cloned()
                        .or_else(|| attrs.get("organization").cloned())
                        .or_else(|| attrs.get("org").cloned())
                        .unwrap_or_default();
                    detail.notes = attrs
                        .get("notes")
                        .cloned()
                        .or_else(|| attrs.get("note").cloned())
                        .unwrap_or_default();
                    detail.email = attrs
                        .get("email")
                        .cloned()
                        .or_else(|| attrs.get("email1").cloned())
                        .unwrap_or_default();
                    detail.website = attrs
                        .get("website")
                        .cloned()
                        .or_else(|| attrs.get("url").cloned())
                        .unwrap_or_default();
                    detail.social_media = attrs
                        .get("social")
                        .cloned()
                        .or_else(|| attrs.get("social_profile").cloned())
                        .unwrap_or_default();
                    detail.address = attrs
                        .get("address")
                        .cloned()
                        .or_else(|| attrs.get("home_address").cloned())
                        .unwrap_or_default();
                    detail.birthday = attrs
                        .get("birthday")
                        .cloned()
                        .or_else(|| attrs.get("bday").cloned())
                        .unwrap_or_default();
                    detail.avatar_path = attrs
                        .get("avatar")
                        .cloned()
                        .or_else(|| attrs.get("photo").cloned())
                        .unwrap_or_default();

                    let phones = [
                        ("mobile", "mobile"),
                        ("phone", "mobile"),
                        ("phone1", "mobile"),
                        ("home", "home"),
                        ("work", "work"),
                        ("phone2", "home"),
                        ("phone3", "work"),
                    ];
                    for (key, phone_type) in phones {
                        if let Some(value) = attrs.get(key) {
                            if detail.phone_primary.trim().is_empty() {
                                detail.phone_primary = value.clone();
                                detail.phone_primary_type = phone_type.to_string();
                            } else if detail.phone_secondary.trim().is_empty()
                                && detail.phone_primary != *value
                            {
                                detail.phone_secondary = value.clone();
                                detail.phone_secondary_type = phone_type.to_string();
                            } else if !detail.addresses.contains(value) {
                                detail.addresses.push(value.clone());
                            }
                        }
                    }

                    let key = if !detail.phone_primary.trim().is_empty() {
                        let normalized = normalize_phone_like(&detail.phone_primary);
                        if normalized.is_empty() {
                            detail.phone_primary.clone()
                        } else {
                            normalized
                        }
                    } else if !detail.display_name.trim().is_empty() {
                        detail.display_name.clone()
                    } else {
                        buf.clear();
                        continue;
                    };
                    let entry = contacts.entry(key).or_insert_with(blank_contact_detail);
                    *entry = merge_contact_detail(entry.clone(), detail);
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => {
                return Err(
                    sms_errors::AppError::External(format!("XML parse error: {}", err)).into(),
                )
            }
            _ => {}
        }
        buf.clear();
    }

    let mut to_upsert = Vec::new();
    if contacts.is_empty() {
        return Ok(0);
    }
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    for (_, contact) in contacts {
        let mut contact = normalize_contact_for_import(contact);
        let addresses = contact_addresses(&contact);
        let mut existing_id: Option<String> = None;
        for addr in &addresses {
            let found: Option<String> = conn
                .query_row(
                    "SELECT contact_id FROM contact_addresses WHERE address = ?1",
                    params![addr],
                    |row| row.get(0),
                )
                .optional()
                .unwrap_or(None);
            if found.is_some() {
                existing_id = found;
                break;
            }
        }
        if let Some(id) = existing_id {
            if let Some(existing) = load_contact_detail_sync(db_path, &id) {
                let mut merged = merge_contact_detail(existing, contact);
                merged.id = id;
                to_upsert.push(merged);
            } else {
                contact.id = id;
                to_upsert.push(contact);
            }
        } else {
            to_upsert.push(contact);
        }
    }

    upsert_contacts(db_path, &to_upsert)
}

fn extract_contact_names_from_xml(xml_path: &Path) -> Result<HashMap<String, String>> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let file = std::fs::File::open(xml_path)?;
    let reader = std::io::BufReader::new(file);
    let mut xml = Reader::from_reader(reader);
    xml.trim_text(true);
    let mut buf = Vec::new();
    let mut names: HashMap<String, String> = HashMap::new();

    loop {
        match xml.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let tag_lc = tag.to_ascii_lowercase();
                if !matches!(tag_lc.as_str(), "sms" | "mms" | "rcs" | "message") {
                    buf.clear();
                    continue;
                }
                let mut attrs: HashMap<String, String> = HashMap::new();
                for attr in e.attributes().with_checks(false) {
                    let attr = attr.map_err(|err| {
                        sms_errors::AppError::External(format!("XML attr error: {}", err))
                    })?;
                    let key = String::from_utf8_lossy(attr.key.as_ref())
                        .to_string()
                        .to_ascii_lowercase();
                    let value = attr
                        .unescape_value()
                        .map_err(|err| {
                            sms_errors::AppError::External(format!("XML decode error: {}", err))
                        })?
                        .to_string();
                    attrs.insert(key, value);
                }
                let address_raw = attrs.get("address").cloned().unwrap_or_default();
                let mut name = attrs
                    .get("contact_name")
                    .cloned()
                    .or_else(|| attrs.get("contact").cloned())
                    .or_else(|| attrs.get("name").cloned())
                    .unwrap_or_default();
                if is_nullish_value(&name) {
                    name.clear();
                }
                if is_nullish_value(&address_raw) || name.trim().is_empty() {
                    buf.clear();
                    continue;
                }
                let key = address_raw.trim().to_string();
                names.entry(key).or_insert(name);
            }
            Ok(Event::Eof) => break,
            Err(err) => {
                return Err(
                    sms_errors::AppError::External(format!("XML parse error: {}", err)).into(),
                )
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(names)
}

fn sync_contact_names_from_xml(db_path: &str, xml_path: &Path) -> Result<usize> {
    let names = extract_contact_names_from_xml(xml_path)?;
    if names.is_empty() {
        return Ok(0);
    }
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();

    let mut address_index: HashMap<String, (String, String)> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT contacts.id, contacts.display_name, contact_addresses.address \
         FROM contact_addresses JOIN contacts ON contacts.id = contact_addresses.contact_id",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) {
            for row in rows.flatten() {
                let (id, display, addr) = row;
                address_index.insert(addr.clone(), (id.clone(), display.clone()));
                let normalized = normalize_phone_like(&addr);
                if !normalized.is_empty() {
                    address_index
                        .entry(normalized.clone())
                        .or_insert((id.clone(), display.clone()));
                    if normalized.len() >= 10 {
                        let last10 = normalized[normalized.len() - 10..].to_string();
                        address_index.entry(last10).or_insert((id, display));
                    }
                }
            }
        }
    }

    let mut updated = 0usize;
    for (addr, name) in names {
        let normalized = normalize_phone_like(&addr);
        let mut key_candidates = vec![addr.clone()];
        if !normalized.is_empty() {
            key_candidates.push(normalized.clone());
            if normalized.len() >= 10 {
                key_candidates.push(normalized[normalized.len() - 10..].to_string());
            }
        }
        let mut found: Option<(String, String)> = None;
        for key in key_candidates {
            if let Some(entry) = address_index.get(&key) {
                found = Some(entry.clone());
                break;
            }
        }

        if let Some((id, display)) = found {
            if display.trim().is_empty() || display == "Unknown" || is_likely_phone_label(&display)
            {
                let _ = conn.execute(
                    "UPDATE contacts SET display_name = ?1, updated_at = strftime('%s','now') WHERE id = ?2",
                    params![name, id],
                );
                updated += 1;
            }
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            let phone = if !normalized.is_empty() {
                normalized
            } else {
                addr.clone()
            };
            let _ = conn.execute(
                "INSERT INTO contacts (id, display_name, phone_primary, updated_at) VALUES (?1, ?2, ?3, strftime('%s','now'))",
                params![id, name, phone],
            );
            let _ = conn.execute(
                "INSERT INTO contact_addresses (id, contact_id, address) VALUES (?1, ?2, ?3)",
                params![uuid::Uuid::new_v4().to_string(), id, addr],
            );
            updated += 1;
        }
    }

    Ok(updated)
}

fn normalize_phone_like(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut digits = trimmed
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>();
    let had_plus = trimmed.starts_with('+');
    let mut folded_nanp = false;
    if digits.len() == 11 && digits.starts_with('1') {
        digits = digits[1..].to_string();
        folded_nanp = true;
    }
    let mut out = String::new();
    // Keep '+' only when no NANP prefix was folded — otherwise
    // "+15551234567" and "15551234567" normalize to different keys and the
    // same person imports as two separate contacts.
    if had_plus && !folded_nanp {
        out.push('+');
    }
    out.push_str(&digits);
    out
}

fn is_nullish_value(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("null")
        || trimmed.eq_ignore_ascii_case("unknown")
}

fn is_likely_phone_label(value: &str) -> bool {
    let digits = value.chars().filter(|c| c.is_ascii_digit()).count();
    digits >= 7 && value.len() <= digits + 4
}

fn upsert_contacts(db_path: &str, contacts: &[ContactDetail]) -> Result<usize> {
    if contacts.is_empty() {
        return Ok(0);
    }
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result: Result<()> = (|| {
        for contact in contacts {
            let addresses = contact_addresses(contact);
            conn.execute(
                "INSERT INTO contacts (id, display_name, nickname, company, notes, email, \
                        phone_primary, phone_secondary, phone_primary_type, phone_secondary_type, \
                        website, social_media, address, birthday, avatar_path, last_contacted, favorite, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, strftime('%s','now')) \
                 ON CONFLICT(id) DO UPDATE SET \
                    display_name=excluded.display_name, \
                    nickname=excluded.nickname, \
                    company=excluded.company, \
                    notes=excluded.notes, \
                    email=excluded.email, \
                    phone_primary=excluded.phone_primary, \
                    phone_secondary=excluded.phone_secondary, \
                    phone_primary_type=excluded.phone_primary_type, \
                    phone_secondary_type=excluded.phone_secondary_type, \
                    website=excluded.website, \
                    social_media=excluded.social_media, \
                    address=excluded.address, \
                    birthday=excluded.birthday, \
                    avatar_path=excluded.avatar_path, \
                    last_contacted=excluded.last_contacted, \
                    favorite=excluded.favorite, \
                    updated_at=strftime('%s','now')",
                params![
                    contact.id,
                    contact.display_name,
                    nullable(contact.nickname.clone()),
                    nullable(contact.company.clone()),
                    nullable(contact.notes.clone()),
                    nullable(contact.email.clone()),
                    nullable(contact.phone_primary.clone()),
                    nullable(contact.phone_secondary.clone()),
                    nullable(contact.phone_primary_type.clone()),
                    nullable(contact.phone_secondary_type.clone()),
                    nullable(contact.website.clone()),
                    nullable(contact.social_media.clone()),
                    nullable(contact.address.clone()),
                    nullable(contact.birthday.clone()),
                    nullable(contact.avatar_path.clone()),
                    contact.last_contacted,
                    if contact.favorite { 1 } else { 0 },
                ],
            )?;
            conn.execute(
                "DELETE FROM contact_addresses WHERE contact_id = ?1",
                params![contact.id],
            )?;
            for addr in addresses {
                conn.execute(
                    "INSERT OR IGNORE INTO contact_addresses (id, contact_id, address) VALUES (?1, ?2, ?3)",
                    params![uuid::Uuid::new_v4().to_string(), contact.id, addr],
                )?;
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(contacts.len())
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

fn load_all_contact_details(db_path: &str) -> Result<Vec<ContactDetail>> {
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    let mut stmt = conn.prepare(
        "SELECT id, display_name, nickname, company, notes, email, phone_primary, \
                phone_secondary, phone_primary_type, phone_secondary_type, website, social_media, \
                address, birthday, avatar_path, last_contacted, favorite \
         FROM contacts ORDER BY display_name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ContactDetail {
            id: row.get(0)?,
            display_name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            nickname: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            company: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            notes: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            email: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            phone_primary: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            phone_secondary: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
            phone_primary_type: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
            phone_secondary_type: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
            website: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
            social_media: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
            address: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
            birthday: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
            avatar_path: row.get::<_, Option<String>>(14)?.unwrap_or_default(),
            last_contacted: row.get::<_, Option<i64>>(15)?,
            favorite: row.get::<_, Option<i64>>(16)?.unwrap_or(0) != 0,
            addresses: Vec::new(),
        })
    })?;
    let mut address_stmt = conn
        .prepare("SELECT address FROM contact_addresses WHERE contact_id = ?1 ORDER BY address")?;
    let mut out = Vec::new();
    for row in rows {
        let mut detail = row?;
        if let Ok(rows) = address_stmt.query_map(params![detail.id.clone()], |row| row.get(0)) {
            for addr in rows.flatten() {
                detail.addresses.push(addr);
            }
        }
        out.push(detail);
    }
    Ok(out)
}

fn blank_contact_detail() -> ContactDetail {
    ContactDetail {
        id: String::new(),
        display_name: String::new(),
        nickname: String::new(),
        company: String::new(),
        notes: String::new(),
        email: String::new(),
        phone_primary: String::new(),
        phone_secondary: String::new(),
        phone_primary_type: "mobile".to_string(),
        phone_secondary_type: "home".to_string(),
        website: String::new(),
        social_media: String::new(),
        address: String::new(),
        birthday: String::new(),
        avatar_path: String::new(),
        last_contacted: None,
        favorite: false,
        addresses: Vec::new(),
    }
}

fn normalize_contact_for_import(mut contact: ContactDetail) -> ContactDetail {
    if contact.id.trim().is_empty() {
        contact.id = uuid::Uuid::new_v4().to_string();
    }
    if contact.display_name.trim().is_empty() {
        if !contact.nickname.trim().is_empty() {
            contact.display_name = contact.nickname.clone();
        } else if !contact.email.trim().is_empty() {
            contact.display_name = contact.email.clone();
        } else if !contact.phone_primary.trim().is_empty() {
            contact.display_name = contact.phone_primary.clone();
        } else {
            contact.display_name = "Unknown".to_string();
        }
    }
    if contact.phone_primary_type.trim().is_empty() {
        contact.phone_primary_type = "mobile".to_string();
    }
    if contact.phone_secondary_type.trim().is_empty() {
        contact.phone_secondary_type = "home".to_string();
    }
    contact
}

fn contact_addresses(detail: &ContactDetail) -> Vec<String> {
    let mut addresses = detail.addresses.clone();
    if !detail.phone_primary.is_empty() {
        addresses.push(detail.phone_primary.clone());
    }
    if !detail.phone_secondary.is_empty() {
        addresses.push(detail.phone_secondary.clone());
    }
    if !detail.address.is_empty() {
        addresses.push(detail.address.clone());
    }
    addresses.sort();
    addresses.dedup();
    addresses
}

fn find_duplicate_groups(db_path: &str) -> Vec<Vec<ContactSummary>> {
    let db = match Database::open(std::path::Path::new(db_path), ResourceProfile::detect()) {
        Ok(db) => db,
        Err(_) => return Vec::new(),
    };
    let conn = db.connection();
    let mut groups = Vec::new();

    let mut collect_by = |column: &str| {
        let sql = format!(
            "SELECT {col} FROM contacts \
             WHERE {col} IS NOT NULL AND {col} != '' \
             GROUP BY {col} HAVING COUNT(*) > 1",
            col = column
        );
        let values: Vec<String> = match conn.prepare(&sql) {
            Ok(mut stmt) => stmt
                .query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        for value in values {
            let mut group = Vec::new();
            let sql = format!(
                "SELECT id, display_name, phone_primary FROM contacts \
                 WHERE {col} = ?1 ORDER BY display_name",
                col = column
            );
            if let Ok(mut stmt) = conn.prepare(&sql) {
                if let Ok(rows) = stmt.query_map(params![value], |row| {
                    Ok(ContactSummary {
                        id: row.get(0)?,
                        display_name: row.get(1)?,
                        primary: row.get(2)?,
                    })
                }) {
                    for row in rows.flatten() {
                        group.push(row);
                    }
                }
            }
            if group.len() > 1 {
                groups.push(group);
            }
        }
    };

    collect_by("phone_primary");
    collect_by("email");

    groups
}

fn build_csv_header_map(headers: &csv::StringRecord) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    for (idx, name) in headers.iter().enumerate() {
        // First column wins when normalized names collide (e.g. "Phone 1" vs
        // "phone1", or duplicate headers) — the primary column usually comes
        // first, and silently letting a later duplicate shadow it drops data.
        map.entry(normalize_header_name(name)).or_insert(idx);
    }
    map
}

fn normalize_header_name(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
}

fn record_value(
    record: &csv::StringRecord,
    headers: &HashMap<String, usize>,
    keys: &[&str],
) -> String {
    for key in keys {
        let normalized = normalize_header_name(key);
        if let Some(idx) = headers.get(&normalized) {
            if let Some(value) = record.get(*idx) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    String::new()
}

fn parse_optional_timestamp(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = trimmed.parse::<i64>().ok()?;
    if parsed < 10_000_000_000 {
        Some(parsed * 1000)
    } else {
        Some(parsed)
    }
}

fn parse_bool_field(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y"
    )
}

fn run_ocr_tesseract(path: &Path, cmd_override: Option<&str>) -> Result<OcrPayload> {
    let tesseract_cmd = cmd_override
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("TESSERACT_CMD").ok())
        .or_else(|| std::env::var("TESSERACT_PATH").ok())
        .unwrap_or_else(|| "tesseract".to_string());
    let output = std::process::Command::new(tesseract_cmd)
        .arg(path)
        .arg("stdout")
        .output()
        .map_err(|e| sms_errors::AppError::External(format!("Tesseract error: {}", e)))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow::anyhow!("Tesseract failed: {}", stderr));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(OcrPayload {
        text,
        model: "tesseract".to_string(),
        timestamp: chrono::Utc::now().timestamp_millis(),
    })
    // #todo: add optional preprocessing (contrast/threshold) for harder scans.
}

fn check_ollama_model(base_url: &str, model: &str) -> String {
    if base_url.trim().is_empty() || model.trim().is_empty() {
        return "FAIL! Missing base URL or model".to_string();
    }
    let url = format!("{}/api/show", base_url.trim_end_matches('/'));
    let resp = ureq::post(&url).send_json(serde_json::json!({ "name": model }));
    let status = match resp {
        Ok(resp) => {
            let parsed: serde_json::Value = resp.into_json().unwrap_or_default();
            if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
                format!("FAIL! {}", err)
            } else {
                "OK!".to_string()
            }
        }
        Err(err) => format!("FAIL! {}", err),
    };
    // #todo: surface model capability checks (vision/tool support) once Ollama exposes them.
    status
}

fn check_tesseract(cmd_override: Option<&str>) -> String {
    let cmd = cmd_override
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("TESSERACT_CMD").ok())
        .or_else(|| std::env::var("TESSERACT_PATH").ok())
        .unwrap_or_else(|| "tesseract".to_string());
    match std::process::Command::new(cmd).arg("--version").output() {
        Ok(output) if output.status.success() => {
            let out = String::from_utf8_lossy(&output.stdout);
            let first = out.lines().next().unwrap_or("Tesseract OK");
            format!("OK! {}", first.trim())
        }
        Ok(output) => {
            let err = String::from_utf8_lossy(&output.stderr);
            format!("FAIL! {}", err.trim())
        }
        Err(err) => format!("FAIL! {}", err),
    }
    // #todo: surface OCR language pack availability checks.
}

fn run_vision_ollama(
    path: &Path,
    base_url: &str,
    model: &str,
    prompt: &str,
) -> Result<VisionPayload> {
    let started = Instant::now();
    let bytes = std::fs::read(path)?;
    let image_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let url = format!("{}/api/chat", base_url.trim_end_matches('/'));
    let payload = serde_json::json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": if prompt.trim().is_empty() { "Describe this image." } else { prompt },
            "images": [image_b64],
        }],
        "stream": false
    });
    let resp = ureq::post(&url)
        .send_json(payload)
        .map_err(|err| sms_errors::AppError::External(format!("Ollama error: {}", err)))?;
    let parsed: serde_json::Value = resp
        .into_json()
        .map_err(|err| sms_errors::AppError::External(format!("Ollama JSON error: {}", err)))?;
    if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
        return Err(sms_errors::AppError::External(format!("Ollama error: {}", err)).into());
    }
    let content = parsed
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .or_else(|| parsed.get("response").and_then(|c| c.as_str()))
        .unwrap_or("")
        .trim()
        .to_string();
    if content.is_empty() {
        return Err(
            sms_errors::AppError::External("Ollama response missing content".to_string()).into(),
        );
    }
    let duration_ms = started.elapsed().as_millis();
    let annotated = format!("⏱ {:.2}s\n{}", (duration_ms as f64) / 1000.0, content);
    Ok(VisionPayload {
        analysis: annotated,
        model: model.to_string(),
        timestamp: chrono::Utc::now().timestamp_millis(),
    })
}

fn check_ffmpeg_cli() -> String {
    match std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
    {
        Ok(output) if output.status.success() => {
            let out = String::from_utf8_lossy(&output.stdout);
            let first = out.lines().next().unwrap_or("ffmpeg OK");
            format!("OK! {}", first.trim())
        }
        Ok(output) => {
            let err = String::from_utf8_lossy(&output.stderr);
            format!("FAIL! {}", err.trim())
        }
        Err(err) => format!("FAIL! {}", err),
    }
    // #todo: surface ffmpeg path/location details for troubleshooting.
}

fn apply_ocr_update(rows: &mut [AttachmentRow], payload: &OcrPayload, attachment_id: &str) {
    if let Some(row) = rows.iter_mut().find(|row| row.id == attachment_id) {
        row.ocr_text = Some(payload.text.clone());
        row.ocr_model = Some(payload.model.clone());
        row.ocr_timestamp = Some(payload.timestamp);
    }
}

fn apply_vision_update(rows: &mut [AttachmentRow], payload: &VisionPayload, attachment_id: &str) {
    if let Some(row) = rows.iter_mut().find(|row| row.id == attachment_id) {
        row.vision_analysis = Some(payload.analysis.clone());
        row.vision_model = Some(payload.model.clone());
        row.vision_timestamp = Some(payload.timestamp);
    }
}

fn apply_nsfw_update(rows: &mut [AttachmentRow], payload: &NsfwPayload, attachment_id: &str) {
    if let Some(row) = rows.iter_mut().find(|row| row.id == attachment_id) {
        row.nsfw_label = Some(payload.label.clone());
        row.nsfw_score = Some(payload.score);
        row.nsfw_model = Some(payload.model.clone());
        row.nsfw_timestamp = Some(payload.timestamp);
    }
}

fn pick_merge(target: &str, source: &str, choice: MergeChoice) -> String {
    match choice {
        MergeChoice::Target => target.to_string(),
        MergeChoice::Source => source.to_string(),
    }
}

fn merge_contact_detail(mut existing: ContactDetail, incoming: ContactDetail) -> ContactDetail {
    let should_replace_name = existing.display_name.trim().is_empty()
        || existing.display_name == existing.phone_primary
        || existing.display_name == "Unknown"
        || is_likely_phone_label(&existing.display_name);
    if should_replace_name && !incoming.display_name.trim().is_empty() {
        existing.display_name = incoming.display_name;
    }
    if existing.nickname.trim().is_empty() && !incoming.nickname.trim().is_empty() {
        existing.nickname = incoming.nickname;
    }
    if existing.company.trim().is_empty() && !incoming.company.trim().is_empty() {
        existing.company = incoming.company;
    }
    if existing.notes.trim().is_empty() && !incoming.notes.trim().is_empty() {
        existing.notes = incoming.notes;
    }
    if existing.email.trim().is_empty() && !incoming.email.trim().is_empty() {
        existing.email = incoming.email;
    }
    if existing.phone_primary.trim().is_empty() && !incoming.phone_primary.trim().is_empty() {
        existing.phone_primary = incoming.phone_primary;
    }
    if existing.phone_secondary.trim().is_empty() && !incoming.phone_secondary.trim().is_empty() {
        existing.phone_secondary = incoming.phone_secondary;
    }
    if existing.phone_primary_type.trim().is_empty()
        && !incoming.phone_primary_type.trim().is_empty()
    {
        existing.phone_primary_type = incoming.phone_primary_type;
    }
    if existing.phone_secondary_type.trim().is_empty()
        && !incoming.phone_secondary_type.trim().is_empty()
    {
        existing.phone_secondary_type = incoming.phone_secondary_type;
    }
    if existing.website.trim().is_empty() && !incoming.website.trim().is_empty() {
        existing.website = incoming.website;
    }
    if existing.social_media.trim().is_empty() && !incoming.social_media.trim().is_empty() {
        existing.social_media = incoming.social_media;
    }
    if existing.address.trim().is_empty() && !incoming.address.trim().is_empty() {
        existing.address = incoming.address;
    }
    if existing.birthday.trim().is_empty() && !incoming.birthday.trim().is_empty() {
        existing.birthday = incoming.birthday;
    }
    if existing.avatar_path.trim().is_empty() && !incoming.avatar_path.trim().is_empty() {
        existing.avatar_path = incoming.avatar_path;
    }
    if existing.last_contacted.is_none() && incoming.last_contacted.is_some() {
        existing.last_contacted = incoming.last_contacted;
    }
    if !existing.favorite && incoming.favorite {
        existing.favorite = true;
    }
    for addr in incoming.addresses {
        if !addr.trim().is_empty() && !existing.addresses.contains(&addr) {
            existing.addresses.push(addr);
        }
    }
    existing
}

fn load_timeline_stats(
    db_path: &str,
    self_addrs: &[String],
    filters: &TimelineFilters,
) -> sms_errors::Result<TimelineStats> {
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();

    let (where_sql, params_base) = build_where_clause("messages", filters);
    let total_messages: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM messages {}", where_sql),
            rusqlite::params_from_iter(params_base.clone()),
            |row| row.get(0),
        )
        .unwrap_or(0);

    let total_threads: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(DISTINCT thread_id) FROM messages {}",
                where_sql
            ),
            rusqlite::params_from_iter(params_base.clone()),
            |row| row.get(0),
        )
        .unwrap_or(0);

    let (where_m, params_m) = build_where_clause("m", filters);
    let total_attachments: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM attachments a JOIN messages m ON m.id = a.message_id {}",
                where_m
            ),
            rusqlite::params_from_iter(params_m.clone()),
            |row| row.get(0),
        )
        .unwrap_or(0);

    let mut sent_messages = 0i64;
    if !self_addrs.is_empty() {
        let placeholders = std::iter::repeat_n("?", self_addrs.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut params_sent = params_base.clone();
        for addr in self_addrs {
            params_sent.push(addr.clone().into());
        }
        let mut sql = format!("SELECT COUNT(*) FROM messages {}", where_sql);
        if where_sql.is_empty() {
            sql = format!(
                "SELECT COUNT(*) FROM messages WHERE address IN ({})",
                placeholders
            );
        } else {
            sql.push_str(&format!(" AND address IN ({})", placeholders));
        }
        sent_messages = conn
            .query_row(&sql, rusqlite::params_from_iter(params_sent), |row| {
                row.get(0)
            })
            .unwrap_or(0);
    }
    let received_messages = total_messages.saturating_sub(sent_messages);

    let mut series = Vec::new();
    let series_sql = format!(
        "SELECT strftime('{}', timestamp/1000, 'unixepoch') as bucket, COUNT(*) \
         FROM messages {} GROUP BY bucket ORDER BY bucket",
        filters.granularity.strftime(),
        where_sql
    );
    if let Ok(mut stmt) = conn.prepare(&series_sql) {
        if let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(params_base.clone()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) {
            for row in rows.flatten() {
                series.push(row);
            }
        }
    }
    let busiest_bucket = series.iter().max_by_key(|(_, v)| *v).cloned();
    let series_text = series.clone();

    let mut series_media = Vec::new();
    let series_media_sql = format!(
        "SELECT strftime('{}', m.timestamp/1000, 'unixepoch') as bucket, COUNT(*) \
         FROM attachments a JOIN messages m ON m.id = a.message_id {} GROUP BY bucket ORDER BY bucket",
        filters.granularity.strftime(),
        where_m
    );
    if let Ok(mut stmt) = conn.prepare(&series_media_sql) {
        if let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(params_m.clone()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) {
            for row in rows.flatten() {
                series_media.push(row);
            }
        }
    }

    let mut busiest_hour = None;
    let hour_sql = format!(
        "SELECT strftime('%H', timestamp/1000, 'unixepoch') as h, COUNT(*) \
         FROM messages {} GROUP BY h ORDER BY COUNT(*) DESC LIMIT 1",
        where_sql
    );
    if let Ok(mut stmt) = conn.prepare(&hour_sql) {
        let row: Option<(String, i64)> = stmt
            .query_row(rusqlite::params_from_iter(params_base.clone()), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()
            .unwrap_or(None);
        if let Some((hour, count)) = row {
            if let Ok(h) = hour.parse::<i64>() {
                busiest_hour = Some((h, count));
            }
        }
    }

    let mut top_contacts = Vec::new();
    let top_sql = format!(
        "SELECT address, COUNT(*) as c FROM messages {} GROUP BY address ORDER BY c DESC LIMIT 10",
        where_sql
    );
    if let Ok(mut stmt) = conn.prepare(&top_sql) {
        if let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(params_base.clone()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) {
            for row in rows.flatten() {
                top_contacts.push(row);
            }
        }
    }

    Ok(TimelineStats {
        total_messages,
        sent_messages,
        received_messages,
        total_attachments,
        total_threads,
        busiest_hour,
        busiest_bucket,
        series_text,
        series_media,
        top_contacts,
    })
}

fn load_map_points(
    db_path: &str,
    _media_root: Option<PathBuf>,
    filters: &MapFilters,
) -> sms_errors::Result<Vec<MapPoint>> {
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    let (where_sql, params) = build_map_where_clause(filters);
    let where_core = where_sql
        .strip_prefix("WHERE ")
        .unwrap_or(where_sql.as_str())
        .trim();
    let where_sql = if where_core.is_empty() {
        "WHERE a.gps_lat IS NOT NULL AND a.gps_lon IS NOT NULL".to_string()
    } else {
        format!(
            "WHERE a.gps_lat IS NOT NULL AND a.gps_lon IS NOT NULL AND {}",
            where_core
        )
    };
    let sql = format!(
        "SELECT a.mime_type, a.file_path, m.id, m.thread_id, m.timestamp, m.address, \
                a.gps_lat, a.gps_lon \
         FROM attachments a \
         JOIN messages m ON m.id = a.message_id {} \
         ORDER BY m.timestamp DESC",
        where_sql
    );
    let mut points = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<f64>>(6)?,
                row.get::<_, Option<f64>>(7)?,
            ))
        })?;
        for row in rows.flatten() {
            let (mime, file_path, message_id, thread_id, timestamp, address, lat_opt, lon_opt) =
                row;
            if !mime.starts_with("image/") {
                continue;
            }
            if let (Some(lat), Some(lon)) = (lat_opt, lon_opt) {
                points.push(MapPoint {
                    lat,
                    lon,
                    file_path,
                    mime_type: mime,
                    message_id,
                    thread_id,
                    timestamp,
                    address,
                });
            }
        }
    }
    Ok(points)
}

fn tag_gps_cache(db_path: &str, media_root: Option<PathBuf>) -> sms_errors::Result<usize> {
    let db = Database::open(std::path::Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    let mut rows = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT id, file_path, mime_type FROM attachments \
         WHERE gps_checked = 0 AND mime_type LIKE 'image/%'",
    ) {
        let iter = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in iter.flatten() {
            rows.push(row);
        }
    }
    if rows.is_empty() {
        return Ok(0);
    }

    let update_with_gps_sql =
        "UPDATE attachments SET gps_lat = ?1, gps_lon = ?2, gps_checked = 1 WHERE id = ?3";
    let update_checked_sql = "UPDATE attachments SET gps_checked = 1 WHERE id = ?1";

    let mut tagged = 0usize;
    for (id, file_path, _mime) in rows {
        let abs_path = resolve_media_path_with_root(db_path, media_root.as_ref(), &file_path);
        if let Some(path) = abs_path {
            if let Some((lat, lon)) = extract_gps_from_image(&path) {
                let _ = conn.execute(update_with_gps_sql, params![lat, lon, id]);
                tagged += 1;
                continue;
            }
        }
        let _ = conn.execute(update_checked_sql, params![id]);
    }
    Ok(tagged)
}

fn clamp_lat(lat: f64) -> f64 {
    lat.clamp(-85.0511, 85.0511)
}

fn lonlat_to_pixel(lat: f64, lon: f64, zoom: u8) -> (f64, f64) {
    let lat = clamp_lat(lat);
    let n = 2f64.powi(zoom as i32);
    let x = (lon + 180.0) / 360.0 * n * 256.0;
    let lat_rad = lat.to_radians();
    let y =
        (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n * 256.0;
    (x, y)
}

fn choose_map_zoom(
    lat_min: f64,
    lat_max: f64,
    lon_min: f64,
    lon_max: f64,
    width: f32,
    height: f32,
) -> u8 {
    if width <= 0.0 || height <= 0.0 {
        return 2;
    }
    for z in (0..=18).rev() {
        let (x1, y1) = lonlat_to_pixel(lat_min, lon_min, z);
        let (x2, y2) = lonlat_to_pixel(lat_max, lon_max, z);
        let w = (x2 - x1).abs() as f32;
        let h = (y2 - y1).abs() as f32;
        if w <= width && h <= height {
            return z;
        }
    }
    0
}

fn fetch_map_tile_image(key: MapTileKey) -> Option<egui::ColorImage> {
    let url = format!(
        "https://tile.openstreetmap.org/{}/{}/{}.png",
        key.z, key.x, key.y
    );
    let response = ureq::get(&url).call().ok()?;
    let mut bytes = Vec::new();
    response.into_reader().read_to_end(&mut bytes).ok()?;
    let image = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, &image))
}

fn build_map_where_clause(filters: &MapFilters) -> (String, Vec<rusqlite::types::Value>) {
    let mut clauses = Vec::new();
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(since_ms) = parse_date_bound(&filters.since, false) {
        clauses.push("m.timestamp >= ?".to_string());
        params.push(since_ms.into());
    }
    if let Some(until_ms) = parse_date_bound(&filters.until, true) {
        clauses.push("m.timestamp <= ?".to_string());
        params.push(until_ms.into());
    }
    if !filters.address.trim().is_empty() {
        clauses.push("m.address LIKE ?".to_string());
        params.push(format!("%{}%", filters.address.trim()).into());
    }
    if !filters.mime_prefix.trim().is_empty() {
        clauses.push("a.mime_type LIKE ?".to_string());
        params.push(format!("{}%", filters.mime_prefix.trim()).into());
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (where_sql, params)
}

fn extract_gps_from_image(path: &Path) -> Option<(f64, f64)> {
    let mut parser = MediaParser::new();
    let ms = MediaSource::file_path(path).ok()?;
    if !ms.has_exif() {
        return None;
    }
    let iter: ExifIter = parser.parse(ms).ok()?;
    let gps = iter.parse_gps_info().ok().flatten()?;

    let lat = gps.latitude.0.as_float()
        + gps.latitude.1.as_float() / 60.0
        + gps.latitude.2.as_float() / 3600.0;
    let lon = gps.longitude.0.as_float()
        + gps.longitude.1.as_float() / 60.0
        + gps.longitude.2.as_float() / 3600.0;

    let lat = if gps.latitude_ref == 'S' { -lat } else { lat };
    let lon = if gps.longitude_ref == 'W' { -lon } else { lon };
    Some((lat, lon))
}

fn checkpoint_path_for(db_path: &str) -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(db_path);
    p.set_extension("checkpoint.json");
    p
}

impl MessageTypeFilter {
    fn as_i32(self) -> Option<i32> {
        match self {
            MessageTypeFilter::All => None,
            MessageTypeFilter::Sms => Some(sms_types::MessageType::Sms as i32),
            MessageTypeFilter::Mms => Some(sms_types::MessageType::Mms as i32),
            MessageTypeFilter::Rcs => Some(sms_types::MessageType::Rcs as i32),
        }
    }

    fn label(self) -> &'static str {
        match self {
            MessageTypeFilter::All => "All",
            MessageTypeFilter::Sms => "SMS",
            MessageTypeFilter::Mms => "MMS",
            MessageTypeFilter::Rcs => "RCS",
        }
    }
}

/// Parse a since/until filter that accepts either a raw epoch-millisecond
/// value or a human `YYYY-MM-DD` date (interpreted in local time; `end_of_day`
/// snaps a bare date to 23:59:59.999 so an "until" bound is inclusive).
fn parse_filter_ms(value: &str, end_of_day: bool) -> Option<i64> {
    use chrono::TimeZone;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(ms) = trimmed.parse::<i64>() {
        return Some(ms);
    }
    let date = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").ok()?;
    let naive = if end_of_day {
        date.and_hms_milli_opt(23, 59, 59, 999)?
    } else {
        date.and_hms_opt(0, 0, 0)?
    };
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.timestamp_millis())
}

/// Developer diagnostics (like the workspace XML-sync test button) are hidden
/// from the normal UI unless `SMS_DEBUG_TOOLS=1` is set.
fn debug_tools_enabled() -> bool {
    std::env::var("SMS_DEBUG_TOOLS").ok().as_deref() == Some("1")
}

fn parse_date_bound(value: &str, is_end: bool) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(ms) = trimmed.parse::<i64>() {
        return Some(ms);
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let time = if is_end {
            chrono::NaiveTime::from_hms_milli_opt(23, 59, 59, 999)?
        } else {
            chrono::NaiveTime::from_hms_opt(0, 0, 0)?
        };
        let dt = chrono::NaiveDateTime::new(date, time);
        let local = chrono::Local.from_local_datetime(&dt).single()?;
        return Some(local.timestamp_millis());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M") {
        let local = chrono::Local.from_local_datetime(&dt).single()?;
        return Some(local.timestamp_millis());
    }
    None
}

fn parse_address_list(value: &str) -> Vec<String> {
    let raw = value.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(['|', ',', ';'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn build_where_clause(
    alias: &str,
    filters: &TimelineFilters,
) -> (String, Vec<rusqlite::types::Value>) {
    let prefix = if alias.is_empty() {
        "".to_string()
    } else {
        format!("{}.", alias)
    };
    let mut clauses = Vec::new();
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(since_ms) = parse_date_bound(&filters.since, false) {
        clauses.push(format!("{}timestamp >= ?", prefix));
        params.push(since_ms.into());
    }
    if let Some(until_ms) = parse_date_bound(&filters.until, true) {
        clauses.push(format!("{}timestamp <= ?", prefix));
        params.push(until_ms.into());
    }
    if !filters.address.trim().is_empty() {
        let addresses = parse_address_list(&filters.address);
        if addresses.len() > 1 {
            let placeholders = std::iter::repeat_n("?", addresses.len())
                .collect::<Vec<_>>()
                .join(",");
            clauses.push(format!("{}address IN ({})", prefix, placeholders));
            for addr in addresses {
                params.push(addr.into());
            }
        } else if let Some(addr) = addresses.first() {
            if addr.contains('*') || addr.contains('%') {
                clauses.push(format!("{}address LIKE ?", prefix));
                params.push(addr.replace('*', "%").into());
            } else {
                clauses.push(format!("{}address LIKE ?", prefix));
                params.push(format!("%{}%", addr).into());
            }
        }
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (where_sql, params)
}

fn format_timestamp(timestamp_ms: i64) -> String {
    if let Some(dt) = chrono::Local.timestamp_millis_opt(timestamp_ms).single() {
        dt.format("%b %d %Y %I:%M %p").to_string()
    } else {
        timestamp_ms.to_string()
    }
}

fn summarize_body(body: &str, max_len: usize) -> String {
    let trimmed = body.trim();
    let mut chars = trimmed.chars();
    if trimmed.chars().count() <= max_len {
        return trimmed.to_string();
    }
    let mut out = String::with_capacity(max_len + 1);
    for _ in 0..max_len {
        if let Some(ch) = chars.next() {
            out.push(ch);
        } else {
            break;
        }
    }
    out.push('…');
    out
}

fn split_vision_analysis(text: &str) -> (Option<String>, String) {
    let mut lines = text.lines();
    if let Some(first) = lines.next() {
        if first.trim_start().starts_with('⏱') {
            let label = first.trim().to_string();
            let rest = lines.collect::<Vec<_>>().join("\n");
            return (Some(label), rest);
        }
    }
    (None, text.to_string())
}

fn sanitize_ollama_embed_input(caption: &str, fallback: &str) -> String {
    let mut text = caption.trim().to_string();
    if text.is_empty() {
        let (_, fallback_body) = split_vision_analysis(fallback);
        text = fallback_body.trim().to_string();
    }
    let mut sanitized = if text.trim().is_empty() {
        "image".to_string()
    } else {
        text
    };
    // #todo: chunk long captions and average embeddings for better recall on verbose descriptions.
    const MAX_CHARS: usize = 4000;
    if sanitized.len() > MAX_CHARS {
        sanitized = sanitized.chars().take(MAX_CHARS).collect();
    }
    sanitized
}

fn hex_hash_backfill(hash: &blake3::Hash) -> String {
    let mut out = String::with_capacity(64);
    for b in hash.as_bytes() {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn is_amr_attachment(att: &AttachmentRow) -> bool {
    if att.mime_type.eq_ignore_ascii_case("audio/amr") {
        return true;
    }
    att.file_path.to_lowercase().ends_with(".amr")
}

fn parse_nsfw_response(text: &str) -> Option<(String, f64)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return parse_nsfw_json(&value);
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if start < end {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&trimmed[start..=end]) {
                return parse_nsfw_json(&value);
            }
        }
    }
    let upper = trimmed.to_uppercase();
    if upper.contains("NSFW") {
        return Some(("NSFW".to_string(), 1.0));
    }
    if upper.contains("SAFE") || upper.contains("SFW") {
        return Some(("SAFE".to_string(), 0.0));
    }
    None
}

fn parse_nsfw_json(value: &serde_json::Value) -> Option<(String, f64)> {
    let label = value
        .get("label")
        .or_else(|| value.get("classification"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let score = value
        .get("score")
        .or_else(|| value.get("confidence"))
        .and_then(|v| v.as_f64())
        .unwrap_or(-1.0);
    if label.is_empty() && score < 0.0 {
        return None;
    }
    let norm_label = normalize_nsfw_label(&label);
    let clamped = score.clamp(0.0, 1.0);
    Some((norm_label, clamped))
}

fn normalize_nsfw_label(label: &str) -> String {
    let upper = label.trim().to_uppercase();
    if upper.contains("NSFW") || upper.contains("NUDE") || upper.contains("EXPLICIT") {
        "NSFW".to_string()
    } else if upper.contains("SUGGEST") || upper.contains("QUESTION") {
        "SUGGESTIVE".to_string()
    } else if upper.contains("SAFE") || upper.contains("SFW") {
        "SAFE".to_string()
    } else if upper.is_empty() {
        "UNKNOWN".to_string()
    } else {
        upper
    }
}

fn media_semantic_search_ollama(
    db_path: &str,
    embed_base: &str,
    embed_model: &str,
    query: &str,
    limit: usize,
) -> sms_errors::Result<Vec<MediaSemanticHit>> {
    let db = Database::open(Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    let query_vec = SmsArchiveApp::ollama_embed(embed_base, embed_model, query)?;
    if query_vec.is_empty() {
        return Ok(Vec::new());
    }
    let model_id: Option<String> = conn
        .query_row(
            "SELECT id FROM ml_models WHERE name = ?1 AND version = 'ollama-media' ORDER BY created_at DESC LIMIT 1",
            params![embed_model],
            |row| row.get(0),
        )
        .optional()?;
    let Some(model_id) = model_id else {
        return Ok(Vec::new());
    };
    media_semantic_search_with_vector(db_path, &model_id, &query_vec, limit)
}

fn media_semantic_search_local(
    db_path: &str,
    config: EmbeddingConfig,
    query: &str,
    limit: usize,
) -> sms_errors::Result<Vec<MediaSemanticHit>> {
    let mut service = EmbeddingService::new(config)?;
    let query_vec = service.embed(query)?;
    if query_vec.is_empty() {
        return Ok(Vec::new());
    }
    let db = Database::open(Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    let model_name = service.model_info().name.clone();
    let model_id: Option<String> = conn
        .query_row(
            "SELECT id FROM ml_models WHERE name = ?1 AND version = 'local-media' ORDER BY created_at DESC LIMIT 1",
            params![model_name],
            |row| row.get(0),
        )
        .optional()?;
    let Some(model_id) = model_id else {
        return Ok(Vec::new());
    };
    media_semantic_search_with_vector(db_path, &model_id, &query_vec, limit)
}

fn media_semantic_search_with_vector(
    db_path: &str,
    model_id: &str,
    query_vec: &[f32],
    limit: usize,
) -> sms_errors::Result<Vec<MediaSemanticHit>> {
    if query_vec.is_empty() {
        return Ok(Vec::new());
    }
    let db = Database::open(Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    let mut stmt = conn.prepare(
        "SELECT media_embeddings.attachment_id, media_embeddings.frame_index, media_embeddings.frame_time_ms, \
                media_embeddings.caption, media_embeddings.vector, media_embeddings.dims, \
                attachments.mime_type, attachments.file_path, attachments.thumbnail_path, attachments.message_id, \
                messages.thread_id, messages.timestamp, messages.address, \
                attachments.nsfw_label, attachments.nsfw_score, attachments.nsfw_model, attachments.nsfw_timestamp \
         FROM media_embeddings \
         JOIN attachments ON attachments.id = media_embeddings.attachment_id \
         LEFT JOIN messages ON messages.id = attachments.message_id \
         WHERE media_embeddings.model_id = ?1",
    )?;
    let mut rows = stmt.query(params![model_id])?;
    let query_norm = l2_norm(query_vec);
    if query_norm == 0.0 {
        return Ok(Vec::new());
    }
    let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<MediaScoreEntry>> =
        std::collections::BinaryHeap::with_capacity(limit + 1);
    while let Some(row) = rows.next()? {
        let attachment_id: String = row.get(0)?;
        let frame_index: i64 = row.get(1)?;
        let frame_time_ms: Option<i64> = row.get(2)?;
        let caption: Option<String> = row.get(3)?;
        let bytes: Vec<u8> = row.get(4)?;
        let dims: i64 = row.get(5)?;
        let embedding = match decode_f32_vec(&bytes, dims as usize) {
            Some(v) => v,
            None => continue,
        };
        let score = cosine_similarity(query_vec, query_norm, &embedding);
        let stats = summarize_embedding(&embedding);
        let hit = MediaSemanticHit {
            score,
            attachment: AttachmentRow {
                id: attachment_id,
                mime_type: row.get(6)?,
                file_path: row.get(7)?,
                thumbnail_path: row.get(8)?,
                message_id: row.get(9)?,
                thread_id: row.get(10)?,
                timestamp: row.get(11)?,
                address: row.get(12)?,
                ocr_text: None,
                ocr_model: None,
                ocr_timestamp: None,
                vision_analysis: None,
                vision_model: None,
                vision_timestamp: None,
                nsfw_label: row.get(13)?,
                nsfw_score: row.get(14)?,
                nsfw_model: row.get(15)?,
                nsfw_timestamp: row.get(16)?,
            },
            frame_index,
            frame_time_ms,
            caption,
            embedding_stats: Some(stats),
        };
        let entry = std::cmp::Reverse(MediaScoreEntry { score, hit });
        if heap.len() < limit {
            heap.push(entry);
        } else if let Some(std::cmp::Reverse(min_entry)) = heap.peek() {
            if score > min_entry.score {
                heap.pop();
                heap.push(entry);
            }
        }
    }
    let mut hits: Vec<MediaSemanticHit> = heap
        .into_sorted_vec()
        .into_iter()
        .map(|std::cmp::Reverse(entry)| entry.hit)
        .collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(hits)
}

fn media_semantic_search_clip(
    db_path: &str,
    config: EmbeddingConfig,
    query: &str,
    limit: usize,
) -> sms_errors::Result<Vec<MediaSemanticHit>> {
    let mut service = EmbeddingService::new(config)?;
    let query_vec = service.embed(query)?;
    if query_vec.is_empty() {
        return Ok(Vec::new());
    }
    let db = Database::open(Path::new(db_path), ResourceProfile::detect())?;
    let conn = db.connection();
    let model_id: Option<String> = conn
        .query_row(
            "SELECT id FROM ml_models WHERE name = ?1 AND version = 'clip-media' ORDER BY created_at DESC LIMIT 1",
            params!["clip-vit-l-14-224"],
            |row| row.get(0),
        )
        .optional()?
        .or_else(|| {
            conn.query_row(
                "SELECT id FROM ml_models WHERE name = ?1 AND version = 'clip-media' ORDER BY created_at DESC LIMIT 1",
                params!["clip-vit-l-14-336"],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
        });
    let Some(model_id) = model_id else {
        return Ok(Vec::new());
    };
    media_semantic_search_with_vector(db_path, &model_id, &query_vec, limit)
}

fn decode_f32_vec(bytes: &[u8], dims: usize) -> Option<Vec<f32>> {
    let expected = dims.saturating_mul(4);
    if bytes.len() < expected || dims == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(dims);
    for i in 0..dims {
        let start = i * 4;
        let chunk = bytes.get(start..start + 4)?;
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Some(out)
}

fn l2_norm(vec: &[f32]) -> f32 {
    vec.iter().map(|v| v * v).sum::<f32>().sqrt()
}

fn cosine_similarity(query: &[f32], query_norm: f32, vec: &[f32]) -> f32 {
    if query.len() != vec.len() {
        return 0.0;
    }
    let dot = query
        .iter()
        .zip(vec.iter())
        .map(|(a, b)| a * b)
        .sum::<f32>();
    let denom = query_norm * l2_norm(vec);
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

fn truncate_filename(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= 10 {
        return name.to_string();
    }
    let start: String = chars.iter().take(4).collect();
    let end: String = chars
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{}…{}", start, end)
}

fn vertical_label(text: &str) -> String {
    text.chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn summarize_embedding(vec: &[f32]) -> EmbeddingStats {
    if vec.is_empty() {
        return EmbeddingStats {
            dims: 0,
            min: 0.0,
            max: 0.0,
            mean: 0.0,
            norm: 0.0,
            head: Vec::new(),
        };
    }
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0.0;
    for v in vec {
        min = min.min(*v);
        max = max.max(*v);
        sum += *v;
    }
    let mean = sum / vec.len() as f32;
    let norm = l2_norm(vec);
    let head = vec.iter().take(8).copied().collect::<Vec<_>>();
    EmbeddingStats {
        dims: vec.len(),
        min,
        max,
        mean,
        norm,
        head,
    }
}

#[derive(Debug)]
struct MediaScoreEntry {
    score: f32,
    hit: MediaSemanticHit,
}

impl PartialEq for MediaScoreEntry {
    fn eq(&self, other: &Self) -> bool {
        self.score.to_bits() == other.score.to_bits()
    }
}

impl Eq for MediaScoreEntry {}

impl PartialOrd for MediaScoreEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MediaScoreEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score.total_cmp(&other.score)
    }
}

fn classify_nsfw_frames(
    frames: &[Keyframe],
    base_url: &str,
    model: &str,
    prompt: &str,
    threshold: f64,
) -> Result<NsfwPayload> {
    // #todo: cache NSFW results to avoid reprocessing already-classified keyframes.
    let mut best_score = -1.0;
    for frame in frames {
        let payload = run_vision_ollama(&frame.path, base_url, model, prompt)?;
        let (_, body) = split_vision_analysis(&payload.analysis);
        let (label, score) =
            parse_nsfw_response(&body).unwrap_or_else(|| ("UNKNOWN".to_string(), 0.0));
        if score > best_score {
            best_score = score;
            let _ = normalize_nsfw_label(&label);
        }
    }
    let final_label = if best_score >= threshold {
        "NSFW"
    } else {
        "SAFE"
    };
    Ok(NsfwPayload {
        label: final_label.to_string(),
        score: best_score.max(0.0),
        model: model.to_string(),
        timestamp: chrono::Utc::now().timestamp_millis(),
    })
}

fn nullable(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn load_attachments(db_path: &str, message_id: &uuid::Uuid) -> Vec<AttachmentRow> {
    let db = match Database::open(Path::new(db_path), ResourceProfile::detect()) {
        Ok(db) => db,
        Err(_) => return Vec::new(),
    };
    let conn = db.connection();
    let mut stmt = match conn.prepare(
        "SELECT id, mime_type, file_path, thumbnail_path, ocr_text, ocr_model, ocr_timestamp, \
            vision_analysis, vision_model, vision_timestamp, nsfw_label, nsfw_score, nsfw_model, nsfw_timestamp \
         FROM attachments WHERE message_id = ?1",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([message_id.to_string()], |row| {
        Ok(AttachmentRow {
            id: row.get(0)?,
            mime_type: row.get(1)?,
            file_path: row.get(2)?,
            thumbnail_path: row.get(3)?,
            message_id: Some(message_id.to_string()),
            thread_id: None,
            timestamp: None,
            address: None,
            ocr_text: row.get(4)?,
            ocr_model: row.get(5)?,
            ocr_timestamp: row.get(6)?,
            vision_analysis: row.get(7)?,
            vision_model: row.get(8)?,
            vision_timestamp: row.get(9)?,
            nsfw_label: row.get(10)?,
            nsfw_score: row.get(11)?,
            nsfw_model: row.get(12)?,
            nsfw_timestamp: row.get(13)?,
        })
    }) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for row in rows.flatten() {
        out.push(row);
    }
    out
}

fn load_attachments_for_messages(
    db_path: &str,
    messages: &[Message],
) -> HashMap<uuid::Uuid, Vec<AttachmentRow>> {
    let mut map: HashMap<uuid::Uuid, Vec<AttachmentRow>> = HashMap::new();
    if messages.is_empty() {
        return map;
    }
    let db = match Database::open(Path::new(db_path), ResourceProfile::detect()) {
        Ok(db) => db,
        Err(_) => return map,
    };
    let conn = db.connection();
    let ids: Vec<String> = messages.iter().map(|m| m.id.to_string()).collect();
    let chunk_size = 200;
    for chunk in ids.chunks(chunk_size) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
                "SELECT id, message_id, mime_type, file_path, thumbnail_path, ocr_text, ocr_model, ocr_timestamp, \
                    vision_analysis, vision_model, vision_timestamp, nsfw_label, nsfw_score, nsfw_model, nsfw_timestamp \
                 FROM attachments WHERE message_id IN ({})",
            placeholders
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(_) => continue,
        };
        let params_vec: Vec<rusqlite::types::Value> =
            chunk.iter().map(|id| id.clone().into()).collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec), |row| {
            Ok(AttachmentRow {
                id: row.get(0)?,
                message_id: row.get(1)?,
                mime_type: row.get(2)?,
                file_path: row.get(3)?,
                thumbnail_path: row.get(4)?,
                thread_id: None,
                timestamp: None,
                address: None,
                ocr_text: row.get(5)?,
                ocr_model: row.get(6)?,
                ocr_timestamp: row.get(7)?,
                vision_analysis: row.get(8)?,
                vision_model: row.get(9)?,
                vision_timestamp: row.get(10)?,
                nsfw_label: row.get(11)?,
                nsfw_score: row.get(12)?,
                nsfw_model: row.get(13)?,
                nsfw_timestamp: row.get(14)?,
            })
        });
        if let Ok(rows) = rows {
            for row in rows.flatten() {
                if let Some(id) = row
                    .message_id
                    .as_ref()
                    .and_then(|v| uuid::Uuid::parse_str(v).ok())
                {
                    map.entry(id).or_default().push(row);
                }
            }
        }
    }
    map
}

#[allow(dead_code)]
fn resolve_media_path(db_path: &str, rel_path: &str) -> Option<std::path::PathBuf> {
    if db_path.trim().is_empty() {
        return None;
    }
    let root = resolve_media_dir(db_path);
    resolve_media_path_candidates(&root, rel_path)
        .into_iter()
        .find(|p| p.exists())
        .or_else(|| {
            resolve_media_path_candidates(&root, rel_path)
                .into_iter()
                .next()
        })
}

fn resolve_media_path_with_root(
    db_path: &str,
    media_root: Option<&PathBuf>,
    rel_path: &str,
) -> Option<PathBuf> {
    if rel_path.trim().is_empty() {
        return None;
    }
    let raw = Path::new(rel_path);
    if raw.is_absolute() {
        return Some(raw.to_path_buf());
    }
    let mut candidates = Vec::new();
    if let Some(root) = media_root {
        candidates.extend(resolve_media_path_candidates(root, rel_path));
    }
    let default_root = resolve_media_dir(db_path);
    candidates.extend(resolve_media_path_candidates(&default_root, rel_path));
    candidates.into_iter().find(|p| p.exists())
}

fn normalize_media_rel(
    raw_path: &str,
    db_path: &str,
    media_root: Option<&PathBuf>,
) -> Option<String> {
    if raw_path.trim().is_empty() {
        return None;
    }
    let raw = Path::new(raw_path);
    let root = media_root
        .cloned()
        .unwrap_or_else(|| resolve_media_dir(db_path));
    if raw.is_absolute() {
        if let Ok(rel) = raw.strip_prefix(&root) {
            return Some(rel.to_string_lossy().replace('\\', "/").to_lowercase());
        }
        return raw
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase());
    }
    let trimmed = raw_path.replace('\\', "/");
    let stripped = strip_media_prefix(&trimmed).unwrap_or(trimmed.as_str());
    Some(stripped.to_lowercase())
}

fn resolve_media_path_candidates(root: &Path, rel_path: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(root.join(rel_path));
    if let Some(stripped) = strip_media_prefix(rel_path) {
        candidates.push(root.join(stripped));
    }
    if root.ends_with("media") {
        if let Some(parent) = root.parent() {
            candidates.push(parent.join(rel_path));
            if let Some(stripped) = strip_media_prefix(rel_path) {
                candidates.push(parent.join(stripped));
            }
        }
    }
    candidates
}

fn strip_media_prefix(rel_path: &str) -> Option<&str> {
    let trimmed = rel_path.trim_start_matches(['.', '/', '\\']);
    trimmed
        .strip_prefix("media/")
        .or_else(|| trimmed.strip_prefix("media\\"))
}

fn pick_existing_relative(candidates: &[&str]) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    for candidate in candidates {
        let path = cwd.join(candidate);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn resolve_media_dir(db_path: &str) -> std::path::PathBuf {
    let db = std::path::Path::new(db_path);
    let base = if db.is_dir() {
        db
    } else {
        db.parent().unwrap_or_else(|| std::path::Path::new("."))
    };
    base.join("media")
}

fn open_file(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", path.to_string_lossy().as_ref()])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

fn open_file_location(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let mut target = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        if let Ok(canon) = target.canonicalize() {
            target = canon;
        }
        if !target.exists() {
            if let Some(parent) = target.parent() {
                target = parent.to_path_buf();
            }
        }
        let _ = if target.is_dir() {
            std::process::Command::new("explorer")
                .arg(target.to_string_lossy().as_ref())
                .spawn()
        } else {
            std::process::Command::new("explorer")
                .arg(format!("/select,{}", target.to_string_lossy()))
                .spawn()
        };
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(parent) = path.parent() {
            let _ = std::process::Command::new("open").arg(parent).spawn();
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(parent) = path.parent() {
            let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
        }
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the Embeddings tab's form fields
fn run_embed_job(
    db_path: std::path::PathBuf,
    model_path: Option<std::path::PathBuf>,
    tokenizer_path: Option<std::path::PathBuf>,
    model_name: String,
    model_version: String,
    dims: usize,
    batch_size: usize,
    max_length: usize,
    normalize: bool,
    device: DevicePreference,
    progress: Arc<EmbedProgress>,
) -> sms_errors::Result<EmbedStats> {
    let db = Database::open(&db_path, ResourceProfile::detect())?;
    let conn = db.connection();

    let mut service = EmbeddingService::new(EmbeddingConfig {
        model_path,
        tokenizer_path,
        model_name,
        model_version,
        dimensions: dims,
        device,
        max_length,
        normalize,
        input_ids_name: None,
        attention_mask_name: None,
        token_type_ids_name: None,
        output_name: None,
    })?;
    let info = service.model_info().clone();
    let meta = service.model_meta().clone();
    let model_meta = sms_db::ModelMeta {
        dims: Some(meta.dimensions as i64),
        max_length: Some(meta.max_length as i64),
        normalize: Some(meta.normalize),
        tokenizer_path: meta.tokenizer_path.clone(),
        input_ids_name: meta.input_ids_name.clone(),
        attention_mask_name: meta.attention_mask_name.clone(),
        token_type_ids_name: meta.token_type_ids_name.clone(),
        output_name: meta.output_name.clone(),
    };
    let model_id = sms_db::upsert_ml_model_with_meta(
        conn,
        &info.name,
        &info.version,
        info.sha256.as_deref(),
        &model_meta,
    )?;

    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages \
         WHERE body_searchable != '' \
           AND NOT EXISTS ( \
               SELECT 1 FROM embeddings \
               WHERE embeddings.message_id = messages.id \
                 AND embeddings.model_id = ?1 \
           )",
        [model_id.as_str()],
        |row| row.get(0),
    )?;
    progress.total.store(total.max(0) as u64, Ordering::Relaxed);

    let start = Instant::now();
    let mut last_rowid = 0i64;
    let mut embedded = 0u64;

    loop {
        if progress.cancelled.load(Ordering::Relaxed) {
            break;
        }
        let mut stmt = conn.prepare(
            "SELECT rowid, id, body_searchable FROM messages \
             WHERE rowid > :last \
               AND body_searchable != '' \
               AND NOT EXISTS ( \
                   SELECT 1 FROM embeddings \
                   WHERE embeddings.message_id = messages.id \
                     AND embeddings.model_id = :model_id \
               ) \
             ORDER BY rowid ASC \
             LIMIT :limit",
        )?;
        let rows = stmt.query_map(
            named_params! {
                ":last": last_rowid,
                ":model_id": model_id.as_str(),
                ":limit": batch_size as i64,
            },
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;

        let mut batch = Vec::new();
        for row in rows {
            let (rowid, id, body) = row?;
            last_rowid = rowid;
            batch.push((id, body));
        }
        if batch.is_empty() {
            break;
        }

        conn.execute_batch("BEGIN IMMEDIATE")?;
        let mut batch_inserted = 0u64;
        let batch_result: sms_errors::Result<()> = (|| {
            let mut insert = conn.prepare_cached(
                "INSERT OR REPLACE INTO embeddings (message_id, model_id, dims, vector) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (id, body) in batch {
                if progress.cancelled.load(Ordering::Relaxed) {
                    break;
                }
                let embedding = service.embed(&body)?;
                let bytes = encode_f32_vec(&embedding);
                insert.execute(params![id, model_id.as_str(), dims as i64, bytes])?;
                batch_inserted += 1;
            }
            Ok(())
        })();
        if let Err(err) = batch_result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(err);
        }
        conn.execute_batch("COMMIT")?;
        embedded = embedded.saturating_add(batch_inserted);
        progress.done.store(embedded, Ordering::Relaxed);
    }

    Ok(EmbedStats {
        embedded,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

fn run_embed_job_ollama(
    db_path: std::path::PathBuf,
    base_url: String,
    ollama_model: String,
    model_name: String,
    model_version: String,
    batch_size: usize,
    progress: Arc<EmbedProgress>,
) -> sms_errors::Result<EmbedStats> {
    let db = Database::open(&db_path, ResourceProfile::detect())?;
    let conn = db.connection();

    let model_meta = sms_db::ModelMeta::default();
    let model_id =
        sms_db::upsert_ml_model_with_meta(conn, &model_name, &model_version, None, &model_meta)?;

    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages \
         WHERE body_searchable != '' \
           AND NOT EXISTS ( \
               SELECT 1 FROM embeddings \
               WHERE embeddings.message_id = messages.id \
                 AND embeddings.model_id = ?1 \
           )",
        [model_id.as_str()],
        |row| row.get(0),
    )?;
    progress.total.store(total.max(0) as u64, Ordering::Relaxed);

    let start = Instant::now();
    let mut last_rowid = 0i64;
    let mut embedded = 0u64;
    let mut dims_updated = false;

    loop {
        if progress.cancelled.load(Ordering::Relaxed) {
            break;
        }
        let mut stmt = conn.prepare(
            "SELECT rowid, id, body_searchable FROM messages \
             WHERE rowid > :last \
               AND body_searchable != '' \
               AND NOT EXISTS ( \
                   SELECT 1 FROM embeddings \
                   WHERE embeddings.message_id = messages.id \
                     AND embeddings.model_id = :model_id \
               ) \
             ORDER BY rowid \
             LIMIT :limit",
        )?;
        let rows = stmt.query_map(
            named_params! { ":last": last_rowid, ":model_id": model_id, ":limit": batch_size as i64 },
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;

        let mut batch = Vec::new();
        for row in rows.flatten() {
            batch.push(row);
        }
        if batch.is_empty() {
            break;
        }

        for (rowid, message_id, body) in batch {
            if progress.cancelled.load(Ordering::Relaxed) {
                break;
            }
            let embedding = SmsArchiveApp::ollama_embed(&base_url, &ollama_model, &body)?;
            if !dims_updated {
                let meta = sms_db::ModelMeta {
                    dims: Some(embedding.len() as i64),
                    max_length: None,
                    normalize: None,
                    tokenizer_path: None,
                    input_ids_name: None,
                    attention_mask_name: None,
                    token_type_ids_name: None,
                    output_name: None,
                };
                let _ = sms_db::upsert_ml_model_with_meta(
                    conn,
                    &model_name,
                    &model_version,
                    None,
                    &meta,
                );
                dims_updated = true;
            }
            sms_db::insert_embedding(conn, &message_id, &model_id, &embedding)?;
            embedded += 1;
            progress.done.store(embedded, Ordering::Relaxed);
            last_rowid = rowid;
        }
    }

    Ok(EmbedStats {
        embedded,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

fn encode_f32_vec(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for v in vector {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

fn global_settings_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("config")
        .join("app_global_settings.json")
}

fn ui_settings_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("config")
        .join("app_ui_settings.json")
}

fn load_global_settings() -> Option<GlobalSettings> {
    let path = global_settings_path();
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<GlobalSettings>(&data).ok()
}

fn persist_global_settings(settings: &GlobalSettings) -> Result<()> {
    let path = global_settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(settings)?;
    std::fs::write(path, data)?;
    Ok(())
}

fn load_ui_settings() -> Option<UiSettings> {
    let path = ui_settings_path();
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<UiSettings>(&data).ok()
}

fn persist_ui_settings_raw(data: &str) -> Result<()> {
    let path = ui_settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, data)?;
    Ok(())
}

fn device_to_string(device: DevicePreference) -> String {
    match device {
        DevicePreference::Cpu => "cpu",
        DevicePreference::Gpu => "gpu",
    }
    .to_string()
}

fn device_from_string(value: &str) -> DevicePreference {
    match value.trim().to_lowercase().as_str() {
        "gpu" | "cuda" => DevicePreference::Gpu,
        _ => DevicePreference::Cpu,
    }
}

fn main() {
    let log_dir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("logs");
    let guard = init_logging(&log_dir).ok();
    tracing::info!("sms-archive started");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SMS Archive")
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([760.0, 500.0])
            .with_icon(std::sync::Arc::new(theming::app_icon())),
        ..Default::default()
    };
    if let Err(err) = eframe::run_native(
        "SMS Archive",
        options,
        Box::new(|cc| {
            theming::configure_style(&cc.egui_ctx);
            let app = SmsArchiveApp {
                log_guard: guard,
                ..Default::default()
            };
            Box::new(app)
        }),
    ) {
        eprintln!("App error: {}", err);
    }
}
