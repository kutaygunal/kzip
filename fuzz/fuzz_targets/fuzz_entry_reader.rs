//! Fuzz target: entry reader + decode.
//!
//! Tries to open every entry of a (possibly malformed) archive and read it to
//! the end. Any panic (during parsing, opening, or decoding) is a bug.

#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::{Cursor, Read};

fuzz_target!(|data: &[u8]| {
    if let Ok(arch) = zip_core::Archive::open(Cursor::new(data.to_vec())) {
        for i in 0..arch.len() {
            if let Ok(mut r) = arch.open_entry(i) {
                let mut out = Vec::new();
                // Result intentionally ignored: no panic allowed.
                let _ = r.read_to_end(&mut out);
            }
        }
    }
});
