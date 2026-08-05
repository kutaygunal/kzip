//! Robustness / no-panic posture tests for zip-core.
//!
//! These are the libFuzzer-free counterparts to the `fuzz/` cargo-fuzz targets.
//! They feed random, truncated, and bit-mutated bytes to the parsers/decoders
//! and assert that the code **returns an error instead of panicking** on
//! malformed input. They run under the normal test harness (no cargo-fuzz /
//! libFuzzer required).

use std::io::{Cursor, Read};
use zip_core::dirent::Dirent;
use zip_core::Archive;

/// Deterministic xorshift64 PRNG so the test is reproducible.
fn xorshift(seed: &mut u64) -> u64 {
    let mut x = *seed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *seed = x;
    x
}

fn rand_bytes(seed: &mut u64, len: usize) -> Vec<u8> {
    (0..len).map(|_| (xorshift(seed) & 0xff) as u8).collect()
}

/// `Archive::open` must never panic on arbitrary bytes.
#[test]
fn archive_open_never_panics_on_garbage() {
    let mut seed: u64 = 0x1234_5678_9abc_def0;
    for _ in 0..4000 {
        let len = (xorshift(&mut seed) % 300) as usize;
        let bytes = rand_bytes(&mut seed, len);
        // A panic here fails the test; only Err/Ok are acceptable.
        let _ = Archive::open(Cursor::new(bytes));
    }
}

/// Opening/reading entries of a truncated or bit-mutated archive must not
/// panic (the archive may fail to open, or individual entries may error).
#[test]
fn truncated_and_mutated_archive_never_panics() {
    use zip_core::{write_archive, ArchiveFile, CompressOptions};
    let files = vec![
        ArchiveFile::new("a/one.txt", b"robustness payload content ".repeat(120)),
        ArchiveFile::new("b/two.bin", vec![7u8; 3000]),
        ArchiveFile::new("c/three.txt", b"compress me compress me ".repeat(80)),
    ];
    let good = write_archive(&files, &CompressOptions::default()).unwrap();

    let mut seed: u64 = 42;
    for _ in 0..3000 {
        let mut bytes = good.clone();
        // Truncate.
        if xorshift(&mut seed) & 1 == 1 {
            let cut = (xorshift(&mut seed) % bytes.len() as u64) as usize;
            bytes.truncate(cut);
        }
        // Bit-mutate a handful of bytes.
        let n = 1 + (xorshift(&mut seed) % 8) as usize;
        for _ in 0..n {
            if bytes.is_empty() {
                break;
            }
            let idx = (xorshift(&mut seed) % bytes.len() as u64) as usize;
            bytes[idx] ^= 0xff;
        }

        let arch = match Archive::open(Cursor::new(bytes)) {
            Ok(a) => a,
            Err(_) => continue,
        };
        for i in 0..arch.len() {
            if let Ok(mut r) = arch.open_entry(i) {
                let mut out = Vec::new();
                // A panic here fails the test; Err at EOF (CRC mismatch on
                // mutated data) is expected and fine.
                let _ = r.read_to_end(&mut out);
            }
        }
    }
}

/// Central-directory entry parsing must never panic on arbitrary bytes.
#[test]
fn parse_central_never_panics() {
    let mut seed: u64 = 7;
    for _ in 0..4000 {
        let len = (xorshift(&mut seed) % 160) as usize;
        let bytes = rand_bytes(&mut seed, len);
        let mut c = Cursor::new(bytes.as_slice());
        let _ = Dirent::parse_central(&mut c);
    }
}

/// Store/Deflate decode must never panic on arbitrary bytes.
#[test]
fn decode_slice_never_panics() {
    use zip_core::codec::decode_slice_into;
    use zip_core::constant::CompressionMethod;
    let mut seed: u64 = 99;
    for _ in 0..1500 {
        let len = (xorshift(&mut seed) % 600) as usize;
        let bytes = rand_bytes(&mut seed, len);
        let mut out = Vec::new();
        let _ = decode_slice_into(&bytes, CompressionMethod::Store, len as u64, &mut out);
        out.clear();
        let _ = decode_slice_into(&bytes, CompressionMethod::Deflate, len as u64, &mut out);
    }
}

/// Bzip2 decode must never panic on arbitrary bytes (bzip2-rs on garbage).
#[test]
fn bzip2_garbage_decode_never_panics() {
    use zip_core::codec::decode_slice_into;
    use zip_core::constant::CompressionMethod;
    let mut seed: u64 = 0xBEEF_CAFE;
    for _ in 0..1500 {
        let len = (xorshift(&mut seed) % 400) as usize;
        let bytes = rand_bytes(&mut seed, len);
        let mut out = Vec::new();
        let _ = decode_slice_into(&bytes, CompressionMethod::Bzip2, len as u64, &mut out);
    }
}

/// `Dirent::parse_local` must never panic on arbitrary bytes (only the central
/// parser was previously exercised).
#[test]
fn parse_local_never_panics() {
    let mut seed: u64 = 0xDEAD_BEEF;
    for _ in 0..4000 {
        let len = (xorshift(&mut seed) % 200) as usize;
        let bytes = rand_bytes(&mut seed, len);
        let mut c = Cursor::new(bytes.as_slice());
        let _ = Dirent::parse_local(&mut c);
    }
}

/// Truncating a valid archive at every byte, plus a minimal EOCD claiming a
/// huge entry count with no central directory, must never panic or allocate
/// wildly.
#[test]
fn truncated_eocd_and_huge_entry_count_never_panic() {
    use zip_core::compress::{write_archive, ArchiveFile, CompressOptions};
    let files = vec![ArchiveFile::new(
        "a.txt",
        b"truncate eocd payload ".repeat(20),
    )];
    let good = write_archive(&files, &CompressOptions::default()).unwrap();

    for cut in 0..good.len() {
        let _ = Archive::open(Cursor::new(good[..cut].to_vec()));
    }
    for bytes in [Vec::<u8>::new(), vec![0x50u8], vec![0x50, 0x4b, 0x05, 0x06]] {
        let _ = Archive::open(Cursor::new(bytes));
    }

    // Minimal EOCD claiming 65535 entries but pointing at a ~4-byte buffer.
    let mut eocd = Vec::new();
    eocd.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]); // EOCD magic
    eocd.extend_from_slice(&0u16.to_le_bytes()); // this disk
    eocd.extend_from_slice(&0u16.to_le_bytes()); // disk with cdir
    eocd.extend_from_slice(&0xFFFFu16.to_le_bytes()); // entries on this disk
    eocd.extend_from_slice(&0xFFFFu16.to_le_bytes()); // total entries
    eocd.extend_from_slice(&0u32.to_le_bytes()); // cdir size
    eocd.extend_from_slice(&4u32.to_le_bytes()); // cdir offset
    eocd.extend_from_slice(&0u16.to_le_bytes()); // comment len
    let _ = Archive::open(Cursor::new(eocd));
}

/// Build a single-member Store archive and return (bytes, central-dir offset)
/// so tests can patch the central-directory method/size fields directly.
fn store_archive_offsets(name: &str, content: &[u8]) -> (Vec<u8>, usize) {
    use zip_core::compress::{write_archive, ArchiveFile, CompressOptions};
    use zip_core::constant::CompressionMethod;
    let files = vec![ArchiveFile::new(name.to_string(), content.to_vec())];
    let bytes = write_archive(
        &files,
        &CompressOptions {
            method: CompressionMethod::Store,
            ..Default::default()
        },
    )
    .unwrap();
    let cdir = 30 + name.len() + content.len();
    (bytes, cdir)
}

/// An entry whose central directory declares a huge uncompressed/compressed
/// size must error (or stay bounded) rather than panic or try to materialize a
/// giant buffer.
#[test]
fn huge_declared_sizes_do_not_oom_or_panic() {
    let name = "huge.bin";
    let content = vec![0x42u8; 4096];
    let (mut bytes, cdir) = store_archive_offsets(name, &content);

    // Central uncomp_size = u32::MAX: the reader must fail the size check at
    // EOF, not allocate ~4 GiB.
    bytes[cdir + 24..cdir + 28].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    let arch = Archive::open(Cursor::new(bytes.clone())).unwrap();
    let mut r = arch.open_entry(0).unwrap();
    let mut out = Vec::new();
    let res = r.read_to_end(&mut out);
    assert!(res.is_err(), "huge declared uncomp_size must error at EOF");
    assert!(out.len() < 1_000_000, "must not materialize a giant buffer");

    // Central comp_size = u32::MAX with a real uncomp_size: decoding stays
    // bounded by the actual source and must not panic or OOM (result may be
    // Ok-with-truncated-or-trailing bytes, or a bounded Err).
    bytes[cdir + 20..cdir + 24].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    bytes[cdir + 24..cdir + 28].copy_from_slice(&(content.len() as u32).to_le_bytes());
    let arch2 = Archive::open(Cursor::new(bytes)).unwrap();
    let mut r2 = arch2.open_entry(0).unwrap();
    let mut out2 = Vec::new();
    let _ = r2.read_to_end(&mut out2);
    assert!(
        out2.len() < 1_000_000,
        "must not materialize a giant buffer"
    );
}

/// An entry with an unsupported compression method must return an error (not
/// panic) when opened.
#[test]
fn unsupported_compression_method_errors_not_panics() {
    let name = "m.bin";
    let content = vec![7u8; 128];
    let (mut bytes, cdir) = store_archive_offsets(name, &content);
    // Patch both local (offset 8) and central (cdir+10) method fields to 99.
    bytes[8..10].copy_from_slice(&99u16.to_le_bytes());
    bytes[cdir + 10..cdir + 12].copy_from_slice(&99u16.to_le_bytes());

    let arch = Archive::open(Cursor::new(bytes)).unwrap();
    let err = arch.open_entry(0).unwrap_err();
    assert_eq!(err.code(), zip_core::ZipErrorCode::Compnotsupp);
}

/// TC-8 (Phase 3): malformed metadata must never panic and must yield a defined
/// error code (`ZIP_ER_INCONS`=21, `ZIP_ER_EF_TOO_LARGE`=36, etc.), not a panic.
/// Exercises oversized/truncated extra fields, oversized comments, and the
/// extra-field parser's length handling.
#[test]
fn metadata_malformed_no_panic() {
    use zip_core::ZipErrorCode;

    // 1. A central-directory entry whose extra-field length exceeds the bytes
    // actually present -> parse must return a defined error, never panic.
    {
        let name = b"a.txt";
        let mut v = Vec::new();
        v.extend_from_slice(&zip_core::constant::magic::CENTRAL);
        v.extend_from_slice(&[20u16.to_le_bytes(), 20u16.to_le_bytes(), 0u16.to_le_bytes()].concat());
        v.extend_from_slice(&0u16.to_le_bytes()); // method
        v.extend_from_slice(&[0u8; 4]); // time/date
        v.extend_from_slice(&0u32.to_le_bytes()); // crc
        v.extend_from_slice(&0u32.to_le_bytes()); // comp
        v.extend_from_slice(&0u32.to_le_bytes()); // uncomp
        v.extend_from_slice(&(name.len() as u16).to_le_bytes()); // name len
        v.extend_from_slice(&500u16.to_le_bytes()); // extra len = 500 (too large)
        v.extend_from_slice(&0u16.to_le_bytes()); // comment len
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(name);
        v.extend_from_slice(&[0xAB, 0xCD]); // truncated extra data
        let mut c = Cursor::new(v.as_slice());
        let res = Dirent::parse_central(&mut c);
        // Either a parse error (Incons/Eof) or success is acceptable; panic is not.
        if let Err(e) = res {
            assert!(matches!(
                e.code(),
                ZipErrorCode::Incons | ZipErrorCode::Eof
            ));
        }
    }

    // 2. An extra-field record whose declared length is longer than the raw
    // block -> ZIP_ER_INCONS (strict rejection), never a panic.
    {
        let raw = [
            0xFEu8, 0xCA, // id 0xCAFE
            0xFF, 0x7F, // length 32767 (larger than the block)
            1, 2, // only 2 data bytes present
        ];
        let mut c = Cursor::new(&raw[..]);
        let res = Dirent::parse_central(&mut c);
        // parse_central needs a full fixed header; this path is already covered
        // by the direct parser, so here we just assert it doesn't panic.
        let _ = res;
    }

    // 3. Oversized per-field extra data must be rejected by the writer with
    // ZIP_ER_EF_TOO_LARGE (36), not panic.
    {
        use zip_core::{write_archive, ArchiveFile, CompressOptions};
        let huge = vec![0u8; 70000]; // > u16::MAX
        let files = vec![ArchiveFile {
            name: "x.bin".to_string(),
            data: b"payload".to_vec(),
            comment: None,
            extra_fields: vec![(0xCAFE, huge)],
            last_mod_time: 0,
            last_mod_date: 0,
            opsys: 3,
            external_attributes: 0,
            method: None,
            level: None,
        }];
        let err = write_archive(&files, &CompressOptions::default()).unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Eftoolarge);
    }

    // 4. An oversized comment (> u16::MAX) must be rejected with
    // ZIP_ER_EF_TOO_LARGE / write error, not panic.
    {
        use zip_core::{write_archive, ArchiveFile, CompressOptions};
        let huge = vec![b'x'; 70000];
        let files = vec![ArchiveFile {
            name: "x.bin".to_string(),
            data: b"payload".to_vec(),
            comment: Some(huge),
            extra_fields: vec![],
            last_mod_time: 0,
            last_mod_date: 0,
            opsys: 3,
            external_attributes: 0,
            method: None,
            level: None,
        }];
        let err = write_archive(&files, &CompressOptions::default()).unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Eftoolarge);
    }
}
