use criterion::{criterion_group, criterion_main, Criterion};
use sms_ingest::{scan_boundaries, scan_boundaries_full, scan_boundaries_naive};
use std::io::Write;

fn bench_boundary_scan(c: &mut Criterion) {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    for _ in 0..10_000 {
        file.write_all(b"<sms address=\"+1\" date=\"1\" body=\"hi\" />")
            .unwrap();
    }

    c.bench_function("boundary_scan_memchr", |b| {
        b.iter(|| {
            let _ = scan_boundaries(file.path()).unwrap();
        });
    });

    c.bench_function("boundary_scan_naive", |b| {
        b.iter(|| {
            let _ = scan_boundaries_naive(file.path()).unwrap();
        });
    });

    c.bench_function("boundary_scan_full", |b| {
        b.iter(|| {
            let _ = scan_boundaries_full(file.path()).unwrap();
        });
    });
}

criterion_group!(benches, bench_boundary_scan);
criterion_main!(benches);
