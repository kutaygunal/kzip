//! Independent (throwaway) functional verification of Phase M2 `modify_archive`.
//! Written by the TESTING agent, deliberately independent of the helper code in
//! `src/modify.rs`'s own test module. It builds archives via the public
//! `write_archive` API and inspects them via the public `Archive` reader.

use std::io::{Cursor, Read};
use zip_core::{
    cdir::read_central_dir, modify_archive, write_archive, Archive, ArchiveFile, CompressOptions,
    Source,
};

fn build(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let af: Vec<ArchiveFile> = files
        .iter()
        .map(|(n, d)| ArchiveFile::new(n.to_string(), d.clone()))
        .collect();
    write_archive(&af, &CompressOptions::default()).expect("write_archive")
}

/// Read an entry by name; return None if absent.
fn read_entry(bytes: &[u8], name: &str) -> Option<Vec<u8>> {
    let arch = Archive::open(Cursor::new(bytes.to_vec())).ok()?;
    let mut r = arch.open_by_name(name).ok()?;
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn data_region_end(bytes: &[u8]) -> usize {
    let mut src: Box<dyn Source> = Box::new(Cursor::new(bytes.to_vec()));
    read_central_dir(&mut src)
        .expect("read_central_dir")
        .cdir_offset as usize
}

#[test]
fn m2_independent_functional_check() {
    // Build a corpus of 64 files with distinct content.
    let files: Vec<(String, Vec<u8>)> = (0..64)
        .map(|i| {
            let name = format!("f{i:02}.txt");
            let content: Vec<u8> =
                format!("content-of-file-{i}-{}", "x".repeat(i as usize)).into_bytes();
            (name, content)
        })
        .collect();
    let original = build(&files);
    let orig_data_end = data_region_end(&original);
    let orig_data = &original[..orig_data_end].to_vec();

    // Snapshot original content of every entry (needed for byte-identical checks).
    let orig_contents: Vec<Vec<u8>> = files
        .iter()
        .map(|(n, _)| read_entry(&original, n).unwrap())
        .collect();

    // Renames at indices 5, 15, 25; deletes at indices 50, 40, 30, 20, 10.
    let renames = vec![
        (5u64, "renamed_05.txt".to_string()),
        (15u64, "renamed_15.txt".to_string()),
        (25u64, "renamed_25.txt".to_string()),
    ];
    let deletes = vec![50u64, 40u64, 30u64, 20u64, 10u64];

    let out = modify_archive(&original, &renames, &deletes).expect("modify_archive");

    // 1) Data region bytes[0..cdir_offset] identical to original.
    let new_data_end = data_region_end(&out);
    assert_eq!(
        &out[..new_data_end],
        orig_data.as_slice(),
        "data region differs"
    );

    // 2) EOCD entry counts: 64 - 5 deletes = 59 entries.
    let mut src: Box<dyn Source> = Box::new(Cursor::new(out.clone()));
    let cd = read_central_dir(&mut src).unwrap();
    assert_eq!(cd.entries.len(), 59, "EOCD entry count wrong");

    // 3) Renamed entries read under NEW names with correct content.
    for (idx, new_name) in &renames {
        let expect = &orig_contents[*idx as usize];
        let got = read_entry(&out, new_name).unwrap_or_else(|| panic!("{new_name} missing"));
        assert_eq!(&got, expect, "content mismatch after rename for {new_name}");
        // Old name should be gone.
        let old_name = &files[*idx as usize].0;
        assert!(
            read_entry(&out, old_name).is_none(),
            "old name {old_name} still present"
        );
    }

    // 4) Deleted entries absent.
    for d in &deletes {
        let name = &files[*d as usize].0;
        assert!(
            read_entry(&out, name).is_none(),
            "deleted {name} still present"
        );
    }

    // 5) Non-affected entries byte-identical, still present under same name.
    for i in 0..64u64 {
        let iu = i as usize;
        if renames.iter().any(|(r, _)| *r == i) || deletes.contains(&i) {
            continue;
        }
        let name = &files[iu].0;
        let expect = &orig_contents[iu];
        let got = read_entry(&out, name).unwrap_or_else(|| panic!("{name} unexpectedly missing"));
        assert_eq!(&got, expect, "non-affected entry {name} content changed");
    }

    // 6) Round-trip: renamed indices map to correct new positions. Original
    //    indices 0..9 remain; 10 deleted; 11..14 remain; 15 renamed; etc.
    //    Spot-check the resulting order via the reader's name(index) API.
    let arch = Archive::open(Cursor::new(out.clone())).unwrap();
    assert_eq!(arch.len(), 59);
    // First surviving entry (idx 0) should still be f00.txt.
    assert_eq!(arch.name(0).unwrap(), "f00.txt");
    // After removing 10 (index 10 in old space), old index 11 -> new index 10.
    assert_eq!(arch.name(10).unwrap(), "f11.txt");
}
