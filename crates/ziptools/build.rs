// Embeds Windows version-resource metadata (kzip branding) into the ziptools binaries.
fn main() {
    if !cfg!(target_os = "windows") {
        return;
    }
    let mut res = winres::WindowsResource::new();
    res.set("ProductName", "kzip");
    res.set("FileDescription", "kzip zipcmp - ZIP archive comparison tool");
    res.set("CompanyName", "kzip contributors");
    res.set("LegalCopyright", "Copyright (c) kzip contributors. BSD-3-Clause.");
    res.set("InternalName", "kzipcmp");
    res.set("OriginalFilename", "kzipcmp.exe");
    res.set("FileVersion", env!("CARGO_PKG_VERSION"));
    res.set("ProductVersion", env!("CARGO_PKG_VERSION"));
    res.compile().unwrap();
}
