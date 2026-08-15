// Cargo caches builds on source mtime alone, so a changed `.env` would
// otherwise be baked in only by accident. Declare every variable the crate
// reads via env!/option_env! so editing .env forces a rebuild.
fn main() {
    for var in [
        "AWS_BUCKET",
        "AWS_REGION",
        "AWS_BUCKET_SIZE",
        "AWS_TARGET_IP",
        "BENCH_WORKERS",
        "BENCH_CONNS_PER_WORKER",
        "BENCH_TLS_STUB",
        "BENCH_BLOCK_SIZE",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}
