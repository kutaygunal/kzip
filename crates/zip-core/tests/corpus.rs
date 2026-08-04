//! Corpus read-path integration tests (regress-style).
//!
//! These mirror what the differential harness and libzip's `regress/*.test`
//! files verify for the read path: open a corpus archive, enumerate entries,
//! and read each entry, asserting name/length/content. They run against the
//! committed `data/corpus` archives so the expected values are pinned in Rust
//! (in addition to the C-vs-Rust differential JSON).

use std::io::{Cursor, Read};
use std::path::PathBuf;
use zip_core::Archive;

fn corpus(name: &str) -> Vec<u8> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/corpus");
    std::fs::read(root.join(name)).unwrap_or_else(|e| {
        panic!("missing corpus file {name}: {e}");
    })
}

fn read_all(arch: &Archive, index: u64) -> (String, Vec<u8>) {
    let name = arch.name(index).unwrap_or("<none>").to_string();
    let mut r = arch.open_entry(index).unwrap();
    let mut out = Vec::new();
    r.read_to_end(&mut out).unwrap();
    (name, out)
}

#[test]
fn corpus_text_zip() {
    let bytes = corpus("text.zip");
    let arch = Archive::open(Cursor::new(bytes)).unwrap();
    assert_eq!(arch.len(), 3);
    let (n0, d0) = read_all(&arch, 0);
    assert_eq!(n0, "data.txt");
    assert_eq!(d0.len(), 5000);
    let (n1, d1) = read_all(&arch, 1);
    assert_eq!(n1, "empty.txt");
    assert_eq!(d1.len(), 2);
    let (n2, d2) = read_all(&arch, 2);
    assert_eq!(n2, "hello.txt");
    assert_eq!(d2.len(), 2700);
}

#[test]
fn corpus_nested_zip() {
    let bytes = corpus("nested.zip");
    let arch = Archive::open(Cursor::new(bytes)).unwrap();
    assert_eq!(arch.len(), 3);
    // Names use literal backslashes as stored by the C tooling on Windows.
    let (n0, d0) = read_all(&arch, 0);
    assert_eq!(n0, "sub\\a.txt");
    assert_eq!(d0.len(), 900);
    let (n1, d1) = read_all(&arch, 1);
    assert_eq!(n1, "sub\\deeper\\b.txt");
    assert_eq!(d1.len(), 20000);
    let (n2, _d2) = read_all(&arch, 2);
    assert_eq!(n2, "root.txt");
}

#[test]
fn corpus_binary_zip_matches_source() {
    let bytes = corpus("binary.zip");
    let arch = Archive::open(Cursor::new(bytes)).unwrap();
    assert_eq!(arch.len(), 1);
    let (name, data) = read_all(&arch, 0);
    assert_eq!(name, "tmp_bin.bin");
    assert_eq!(data.len(), 60000);

    // The binary entry must decompress to exactly the source data file.
    let src =
        std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/tmp_bin.bin"))
            .unwrap();
    assert_eq!(data, src);
}
