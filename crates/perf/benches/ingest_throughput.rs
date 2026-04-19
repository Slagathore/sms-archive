use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use sms_ingest::{ingest_file, IngestOptions};
use std::io::Write;

fn build_xml(count: usize) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(file, "<smses>").unwrap();
    for i in 0..count {
        writeln!(file, "<sms address=\"+1\" date=\"{}\" body=\"hi\" />", i).unwrap();
    }
    writeln!(file, "</smses>").unwrap();
    file
}

fn bench_ingest(c: &mut Criterion) {
    let xml = build_xml(50_000);
    let mut group = c.benchmark_group("ingest_throughput");

    for &use_boundary_scan in &[false, true] {
        group.bench_with_input(
            BenchmarkId::new(
                "ingest",
                if use_boundary_scan {
                    "boundary"
                } else {
                    "stream"
                },
            ),
            &use_boundary_scan,
            |b, &mode| {
                b.iter_batched(
                    || tempfile::NamedTempFile::new().unwrap(),
                    |db| {
                        let opts = IngestOptions {
                            batch_size: 5_000,
                            queue_bytes: 64 * 1024 * 1024,
                            read_buffer_bytes: 2 * 1024 * 1024,
                            use_boundary_scan: mode,
                            parser_threads: 4,
                            recover_on_error: true,
                            defer_thumbnails: true,
                            thumbnail_workers: 1,
                            thumbnail_queue_capacity: 32,
                            resume: false,
                            media_dir: None,
                            write_attachments: false,
                            thumbnail_size: 64,
                            writer_mode: sms_db::ConnectionMode::Import,
                            progress: None,
                        };
                        let _ = ingest_file(xml.path(), db.path(), &opts).unwrap();
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_ingest);
criterion_main!(benches);
