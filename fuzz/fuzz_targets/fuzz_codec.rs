//! Fuzz target: codec demux (Dirent::parse_central + decode_slice_into).
//!
//! Exercises both the entry-record parser and the Store/Deflate decoders with
//! arbitrary bytes. Nothing here may panic.

#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;
use zip_core::codec::decode_slice_into;
use zip_core::constant::CompressionMethod;
use zip_core::dirent::Dirent;

fuzz_target!(|data: &[u8]| {
    // Entry-record demux.
    let mut c = Cursor::new(data);
    let _ = Dirent::parse_central(&mut c);

    // Decode as Store and Deflate (bounded to avoid huge allocation).
    if data.len() <= 1 << 20 {
        let mut out = Vec::new();
        let _ = decode_slice_into(data, CompressionMethod::Store, data.len() as u64, &mut out);
        out.clear();
        let _ = decode_slice_into(data, CompressionMethod::Deflate, data.len() as u64, &mut out);
    }
});
