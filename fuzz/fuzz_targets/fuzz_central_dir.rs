//! Fuzz target: central-directory parser.
//!
//! Feeds arbitrary bytes to `read_central_dir` and discards the result. The
//! parser must never panic (only return `Err`) on malformed input.

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut src: Box<zip_core::Source> =
        Box::new(std::io::Cursor::new(data.to_vec()));
    // Result is intentionally ignored: any panic here is a bug.
    let _ = zip_core::cdir::read_central_dir(&mut src);
});
