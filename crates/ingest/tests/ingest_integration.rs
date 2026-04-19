use sms_config::ResourceProfile;
use sms_db::Database;
use sms_ingest::{ingest_file, IngestOptions};
use std::io::Write;

#[test]
fn ingest_small_xml_creates_rows() {
    let mut xml = tempfile::NamedTempFile::new().unwrap();
    write!(
        xml,
        "<smses><sms address=\"+1\" date=\"1\" body=\"hi\" /><mms address=\"+2\" date=\"2\"><part ct=\"text/plain\" text=\"hello\" /></mms></smses>"
    )
    .unwrap();

    let db = tempfile::NamedTempFile::new().unwrap();
    let opts = IngestOptions {
        batch_size: 2,
        queue_bytes: 1024 * 1024,
        read_buffer_bytes: 1024,
        use_boundary_scan: true,
        parser_threads: 2,
        recover_on_error: true,
        defer_thumbnails: true,
        thumbnail_workers: 1,
        thumbnail_queue_capacity: 16,
        resume: false,
        media_dir: None,
        write_attachments: false,
        thumbnail_size: 128,
        writer_mode: sms_db::ConnectionMode::Import,
        progress: None,
    };

    let stats = ingest_file(xml.path(), db.path(), &opts).unwrap();
    assert!(stats.messages_inserted >= 2);

    let db = Database::open(db.path(), ResourceProfile::Low).unwrap();
    let conn = db.connection();
    let msg_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(msg_count, 2);
}

#[test]
fn ingest_recovers_from_malformed_xml() {
    let mut xml = tempfile::NamedTempFile::new().unwrap();
    write!(
        xml,
        "<smses><sms address=\"+1\" date=\"1\" body=\"hi\" />\
         <sms address=\"+2\" date=\"2\" body=\"ok\" <sms address=\"+3\" date=\"3\" body=\"yo\" />\
         </smses>"
    )
    .unwrap();

    let db = tempfile::NamedTempFile::new().unwrap();
    let opts = IngestOptions {
        batch_size: 2,
        queue_bytes: 1024 * 1024,
        read_buffer_bytes: 256,
        use_boundary_scan: false,
        parser_threads: 2,
        recover_on_error: true,
        defer_thumbnails: true,
        thumbnail_workers: 1,
        thumbnail_queue_capacity: 16,
        resume: false,
        media_dir: None,
        write_attachments: false,
        thumbnail_size: 128,
        writer_mode: sms_db::ConnectionMode::Import,
        progress: None,
    };

    let stats = ingest_file(xml.path(), db.path(), &opts).unwrap();
    assert!(stats.messages_inserted >= 1);
    assert!(stats.parse_errors >= 1);
}
