// Embeds Windows version-resource metadata (kzip branding) into the zip-sys cdylib.
fn main() {
    if !cfg!(target_os = "windows") {
        return;
    }
    let mut res = winres::WindowsResource::new();
    res.set("ProductName", "kzip");
    res.set(
        "FileDescription",
        "kzip - Rust port of libzip (C ABI, drop-in zip.dll)",
    );
    res.set("CompanyName", "kzip contributors");
    res.set(
        "LegalCopyright",
        "Copyright (c) kzip contributors. BSD-3-Clause.",
    );
    res.set("InternalName", "kzip");
    res.set("OriginalFilename", "kzip.dll");
    res.set("FileVersion", env!("CARGO_PKG_VERSION"));
    res.set("ProductVersion", env!("CARGO_PKG_VERSION"));
    res.compile().unwrap();
}
