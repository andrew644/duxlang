fn main() {
    let decls = dux_cinterop::import(&dux_cinterop::Import {
        header: "mini.h".to_string(),
        flags: Vec::new(),
        include_paths: vec![format!(
            "{}/tests/fixtures",
            std::env::var("CARGO_MANIFEST_DIR").unwrap()
        )],
    })
    .unwrap();
    print!("{}", dux_cinterop::spell(&decls));
}
