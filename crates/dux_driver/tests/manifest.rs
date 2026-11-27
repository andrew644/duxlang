use dux_driver::{find_manifest, parse_manifest, Manifest};
use std::path::{Path, PathBuf};

#[test]
fn a_manifest_reads_its_three_sections() {
    let manifest = parse_manifest(
        "# the adventure project\n\
         [package]\nname = \"adventure\"\nversion = \"0.1.0\" # ignored for now\n\n\
         [deps]\ngreetings = \"../vendor/greetings\"\n\n\
         [cdeps]\ncompression = \"zlib\"\n",
    );
    assert_eq!(manifest.name, "adventure");
    assert_eq!(
        manifest.deps,
        vec![("greetings".to_string(), "../vendor/greetings".to_string())]
    );
    assert_eq!(
        manifest.cdeps,
        vec![("compression".to_string(), "zlib".to_string())]
    );
    let home = Path::new("/proj");
    assert_eq!(
        manifest.dep_roots(home),
        vec![PathBuf::from("/proj/../vendor/greetings")]
    );
    assert_eq!(manifest.cdep_packages(), vec!["zlib"]);
}

#[test]
fn keys_without_a_section_and_unknown_sections_are_left_alone() {
    assert_eq!(
        parse_manifest("stray = \"x\"\n[widget]\nsize = \"9\"\n"),
        Manifest::default()
    );
}

#[test]
fn the_nearest_manifest_above_the_program_is_the_one_that_governs() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("manifest-probe");
    let _ = std::fs::remove_dir_all(&root);
    let deep = root.join("a/b/c");
    std::fs::create_dir_all(&deep).expect("scratch is writable");
    std::fs::write(root.join("dux.mod"), "[package]\nname = \"far\"\n").expect("writable");
    std::fs::write(deep.join("dux.mod"), "[package]\nname = \"near\"\n").expect("writable");
    let file = deep.join("main.dux");
    std::fs::write(&file, "package main\nfn main() {}\n").expect("writable");
    let (at, manifest) = find_manifest(&file).expect("a manifest governs from above");
    assert_eq!(at, deep.join("dux.mod"));
    assert_eq!(manifest.name, "near");
    std::fs::remove_file(deep.join("dux.mod")).expect("removable");
    let (at, manifest) = find_manifest(&file).expect("the farther one governs instead");
    assert_eq!(at, root.join("dux.mod"));
    assert_eq!(manifest.name, "far");
}
