//! Benchmark crate for LibzipInRust.
//!
//! The criterion benchmarks live in `benches/`; this library target exists so
//! the crate is a normal workspace member that `cargo test --workspace` can
//! build cleanly. It also hosts the shared, deterministic benchmark corpus used
//! by the C-vs-Rust serial comparison harnesses (`c_serial` / `rust_serial`
//! binaries).

/// One member of a benchmark corpus.
#[derive(Debug, Clone)]
pub struct CorpusFile {
    /// Member name stored in the archive.
    pub name: String,
    /// Uncompressed content.
    pub data: Vec<u8>,
}

/// A tiny deterministic xorshift64 PRNG so both the C and Rust serial harnesses
/// derive the *same* corpus bytes from the same seed without any shared state.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Highly compressible, deterministic "log line" text of length `n`.
fn repetitive_text(seed: u64, n: usize) -> Vec<u8> {
    let line = format!(
        "[{seed}] libzip-in-rust benchmark payload line with repeating content to exercise the DEFLATE codec; line content repeated for compression.\n"
    );
    let line = line.as_bytes();
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let take = (n - out.len()).min(line.len());
        out.extend_from_slice(&line[..take]);
    }
    out
}

/// A block mixing compressible text with incompressible random bytes, sized
/// `n` bytes.
fn mixed_block(seed: u64, n: usize, rng: &mut XorShift) -> Vec<u8> {
    let line = format!("medium block {seed} compressible text payload ...\n");
    let line = line.as_bytes();
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let take = ((rng.next_u64() % 1024) as usize + 1).min(n - out.len());
        if take % 3 == 0 {
            for _ in 0..take {
                out.push((rng.next_u64() & 0xff) as u8);
            }
        } else {
            let mut c = 0usize;
            while c < take {
                out.push(line[c % line.len()]);
                c += 1;
            }
        }
    }
    out
}

/// Build the shared mixed benchmark corpus: small/medium/large, text + random.
///
/// This is the *same* corpus both the C libzip serial harness (`c_serial`) and
/// the Rust serial harness (`rust_serial`) compress, so throughput numbers are
/// directly comparable.
pub fn build_mixed_corpus() -> Vec<CorpusFile> {
    let mut rng = XorShift::new(0x9E37_79B9_7F4A_7C15);
    let mut files = Vec::new();

    // 40 small, compressible text files.
    for i in 0..40 {
        let n = 512 + (rng.next_u64() % 4096) as usize;
        files.push(CorpusFile {
            name: format!("small/f{i:03}.txt"),
            data: repetitive_text(i as u64, n),
        });
    }

    // 8 medium files mixing text and semi-random data.
    for i in 0..8 {
        let n = 262_144 + (rng.next_u64() % 1_500_000) as usize;
        files.push(CorpusFile {
            name: format!("medium/m{i:02}.dat"),
            data: mixed_block(i as u64, n, &mut rng),
        });
    }

    // 2 large, highly compressible text files.
    files.push(CorpusFile {
        name: "large/big.txt".into(),
        data: repetitive_text(999, 32 * 1024 * 1024),
    });
    files.push(CorpusFile {
        name: "large/log.txt".into(),
        data: repetitive_text(1000, 16 * 1024 * 1024),
    });

    files
}

/// Total uncompressed corpus size in bytes.
pub fn corpus_size_bytes(files: &[CorpusFile]) -> u64 {
    files.iter().map(|f| f.data.len() as u64).sum()
}

/// Median of a slice of `f64`s.
pub fn median(mut xs: Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = xs.len() / 2;
    if xs.len() % 2 == 0 {
        (xs[mid - 1] + xs[mid]) / 2.0
    } else {
        xs[mid]
    }
}

/// Format a CSV row for the serial benchmark results.
pub fn csv_row(
    impl_: &str,
    version: &str,
    run: usize,
    bytes: u64,
    secs: f64,
    mibps: f64,
) -> String {
    format!("{impl_},{version},{run},{bytes},{secs:.6},{mibps:.3}\n")
}
