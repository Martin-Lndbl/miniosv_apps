//! Compile-time configuration, baked in from the environment by `just setup`.
//!
//! These are the benchmark's knobs, not the stack's: mininet takes its
//! configuration as values at run time. They stay `const fn` so a malformed
//! `.env` fails the build rather than a booted instance -- discovering a
//! missing bucket name on EC2 costs a VM launch to learn something the
//! compiler already knew.
//!
//! Defaults are the values the benchmark was tuned with, so an unset variable
//! still gives a sensible run.

/// Decimal integer, optionally with an IEC suffix (K/M/G/T, `i` and `B`
/// ignored) so `AWS_BUCKET_SIZE="10G"` parses directly.
const fn parse_size(s: Option<&str>, default: u64) -> u64 {
    let s = match s {
        Some(s) => s,
        None => return default,
    };
    let b = s.as_bytes();
    if b.is_empty() {
        return default;
    }
    let mut i = 0;
    let mut v: u64 = 0;
    while i < b.len() {
        let c = b[i];
        if c >= b'0' && c <= b'9' {
            v = v * 10 + (c - b'0') as u64;
            i += 1;
        } else {
            let mult: u64 = match c {
                b'K' | b'k' => 1024,
                b'M' | b'm' => 1024 * 1024,
                b'G' | b'g' => 1024 * 1024 * 1024,
                b'T' | b't' => 1024u64 * 1024 * 1024 * 1024,
                b'B' | b'b' | b'i' | b'I' => 1,
                _ => return default,
            };
            return v * mult;
        }
    }
    v
}

const fn parse_bool(s: Option<&str>, default: bool) -> bool {
    let s = match s {
        Some(s) => s,
        None => return default,
    };
    let b = s.as_bytes();
    if b.is_empty() {
        return default;
    }
    match b[0] {
        b'1' | b't' | b'T' | b'y' | b'Y' => true,
        b'0' | b'f' | b'F' | b'n' | b'N' => false,
        _ => default,
    }
}

const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// `BENCH_SCHEME`. Unset means https.
const fn is_http(s: Option<&str>) -> bool {
    let b = match s {
        Some(s) => s.as_bytes(),
        None => return false,
    };
    bytes_eq(b, b"http")
}

const fn scheme_ok(s: Option<&str>) -> bool {
    let b = match s {
        Some(s) => s.as_bytes(),
        None => return true,
    };
    b.is_empty() || bytes_eq(b, b"http") || bytes_eq(b, b"https")
}

/// Dotted-quad, e.g. "3.5.216.240". Falls back to 0.0.0.0, which the caller
/// treats as "not configured" and reports rather than silently misdialling.
const fn parse_ipv4(s: Option<&str>) -> [u8; 4] {
    let s = match s {
        Some(s) => s,
        None => return [0; 4],
    };
    let b = s.as_bytes();
    let mut out = [0u8; 4];
    let mut oct = 0;
    let mut acc: u32 = 0;
    let mut seen = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c >= b'0' && c <= b'9' {
            acc = acc * 10 + (c - b'0') as u32;
            if acc > 255 {
                return [0; 4];
            }
            seen = true;
        } else if c == b'.' {
            if !seen || oct >= 3 {
                return [0; 4];
            }
            out[oct] = acc as u8;
            oct += 1;
            acc = 0;
            seen = false;
        } else {
            return [0; 4];
        }
        i += 1;
    }
    if !seen || oct != 3 {
        return [0; 4];
    }
    out[3] = acc as u8;
    out
}

/// RSS queues (and worker threads) to ask for; clamped to the device maximum.
pub const N_WORKERS_REQ: u16 = parse_size(option_env!("BENCH_WORKERS"), 8) as u16;

/// Parallel TLS connections each worker drives.
pub const CONNS_PER_WORKER: usize = parse_size(option_env!("BENCH_CONNS_PER_WORKER"), 24) as usize;

/// Size of the object in the bucket. Bounds the offsets a Range may name; it
/// is no longer the amount transferred.
pub const OBJECT_SIZE: u64 = parse_size(option_env!("AWS_BUCKET_SIZE"), 10 * 1024 * 1024 * 1024);

/// Bytes each connection requests. Held fixed while worker count varies, so
/// that a throughput-vs-parallelism curve is not also a
/// throughput-vs-request-size curve: splitting a fixed total across workers
/// shrank each request as parallelism rose and confounded the two.
pub const BLOCK_SIZE: u64 = parse_size(option_env!("BENCH_BLOCK_SIZE"), 64 * 1024 * 1024);

/// `BENCH_SCHEME=http` drops TLS entirely and dials port 80, which isolates
/// the network stack from the record layer. Rows from the two schemes measure
/// different things and do not belong in one CSV.
pub const PLAIN_HTTP: bool = is_http(option_env!("BENCH_SCHEME"));

/// A misspelt scheme would silently run TLS and produce a row labelled https,
/// which is a wrong measurement rather than a failed one.
const _: () = assert!(
    scheme_ok(option_env!("BENCH_SCHEME")),
    "BENCH_SCHEME must be http or https"
);

/// Discard ciphertext after the handshake instead of decrypting it. Isolates
/// the network stack from the record layer; the transfer is then unverifiable
/// byte-for-byte, so the completeness check falls back to a range check.
/// Meaningless without a record layer, so `PLAIN_HTTP` forces it off.
pub const STUB_TLS_AFTER_HANDSHAKE: bool =
    parse_bool(option_env!("BENCH_TLS_STUB"), false) && !PLAIN_HTTP;

/// Resolved by `just setup` from the bucket endpoint and baked in. The guest
/// has no resolver by design, so this has to be decided at build time; if it
/// goes stale the run fails loudly with a SYN timeout rather than silently.
pub const TARGET_IP: [u8; 4] = parse_ipv4(option_env!("AWS_TARGET_IP"));

const _: () = assert!(
    !(TARGET_IP[0] == 0 && TARGET_IP[1] == 0 && TARGET_IP[2] == 0 && TARGET_IP[3] == 0),
    "AWS_TARGET_IP is unset or malformed - run `just setup smoltcp-s3`"
);

/// Endpoint, from $AWS_BUCKET / $AWS_REGION.
pub const TARGET_HOST: &str = concat!(
    env!("AWS_BUCKET", "AWS_BUCKET is not set — run `just setup`"),
    ".s3.",
    env!("AWS_REGION", "AWS_REGION is not set — run `just setup`"),
    ".amazonaws.com"
);

pub const TARGET_PATH: &str = "/blob.bin";

/// `BENCH_MODE=net` runs the mininet selftest instead of the benchmark: the
/// Service API, the parked-thread handoff and delivery into a buffer, none of
/// which the benchmark itself touches. Anything else runs the benchmark.
pub const SELFTEST: bool = is_net(option_env!("BENCH_MODE"));

const fn is_net(s: Option<&str>) -> bool {
    let b = match s {
        Some(s) => s.as_bytes(),
        None => return false,
    };
    bytes_eq(b, b"net")
}
