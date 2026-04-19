# Benchmarks

## Goals
- Measure ingest throughput, search latency, and resource usage.
- Track regressions across changes.

## Commands
- Unit tests: `cargo test --workspace`
- Lints: `cargo clippy --workspace -- -D warnings`
- Bench (boundary scan): `cargo bench -p sms-perf`

## Synthetic Data
Generate datasets with realistic patterns:
- `sms datagen --output test.xml --size 0.1 --seed 42 --mms-ratio 0.1 --burstiness 0.2`

## Ingest Runs
Example ingest run:
- `sms ingest --input test.xml --db bench.db --media-dir .\media --verify`

Capture metrics:
- messages/sec
- bytes/sec
- elapsed time
- WAL size after checkpoint
- peak RSS (use OS tools)

## Record Hardware
Always record:
- CPU model + core count
- RAM
- Disk type (HDD/SSD/NVMe)
- OS version

## Regression Policy
- Treat >10% slowdown in throughput as a regression unless explained by feature additions.
- Keep a baseline result file per release.
