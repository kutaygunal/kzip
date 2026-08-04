//! Optional build-time generation of `include/zip.h` via cbindgen.
//!
//! `cbindgen` is NOT a required dependency: it is not installed in the standard
//! environment. When it is absent, we keep the committed, hand-maintained
//! header (crates/zip-sys/include/zip.h) and emit a note. When present, we
//! regenerate it from `src/lib.rs`. The header can also be regenerated manually
//! with `scripts/gen-zip-h.sh`.

fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=include/zip.h");

    // Probe for cbindgen without hard-requiring it.
    let have_cbindgen = std::process::Command::new("cbindgen")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !have_cbindgen {
        println!(
            "cargo:warning=cbindgen not installed; using committed crates/zip-sys/include/zip.h"
        );
        return;
    }

    match std::process::Command::new("cbindgen")
        .args(["--crate", "zip", "--output", "include/zip.h"])
        .status()
    {
        Ok(s) if s.success() => {
            println!("cargo:warning=regenerated include/zip.h via cbindgen");
        }
        _ => {
            println!("cargo:warning=cbindgen regeneration failed; using committed zip.h");
        }
    }
}
