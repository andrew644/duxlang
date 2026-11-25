fn main() {
    let seen = dux_cinterop::import(&dux_cinterop::Import {
        header: std::env::args()
            .nth(1)
            .unwrap_or_else(|| "mini.h".to_string()),
        flags: std::env::args().skip(2).collect(),
        include_paths: vec![format!(
            "{}/tests/fixtures",
            std::env::var("CARGO_MANIFEST_DIR").unwrap()
        )],
    })
    .unwrap()
    .decls;
    print!("{}", dux_cinterop::spell(&seen));
}
