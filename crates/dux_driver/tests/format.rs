use dux_diag::Diagnostic;
use dux_driver::format_text;
use dux_source::SourceMap;
use std::path::{Path, PathBuf};

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/e2e");
const EXAMPLE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../example");

fn dux_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{dir:?} is readable: {e}"))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "dux"))
        .collect();
    files.sort();
    files
}

fn parse_dump(name: &str, text: &str) -> Result<String, Vec<Diagnostic>> {
    let mut sources = SourceMap::new();
    let file = sources.add(name, text.to_string());
    let lexed = dux_lexer::lex(&sources, file);
    let mut diagnostics = lexed.diagnostics;
    let parsed = dux_parser::parse(&sources, &lexed.tokens);
    diagnostics.extend(parsed.diagnostics);
    if dux_diag::has_errors(&diagnostics) {
        return Err(diagnostics);
    }
    Ok(dux_ast::dump::dump(&parsed.program))
}

fn assert_round_trips(path: &Path) -> bool {
    let name = path.display().to_string();
    let source = std::fs::read_to_string(path).expect("corpus file is readable");
    let Ok(formatted) = format_text(&name, &source) else {
        return false;
    };
    assert_eq!(
        formatted,
        format_text(&name, &formatted).expect("formatted output re-formats"),
        "{name}: formatting is not idempotent"
    );
    let original_dump = parse_dump(&name, &source).expect("corpus file parses before formatting");
    let formatted_dump =
        parse_dump(&name, &formatted).expect("formatter output parses without errors");
    assert_eq!(
        original_dump, formatted_dump,
        "--- formatted {name} ---\n{formatted}"
    );
    true
}

#[test]
fn every_formatable_example_survives_a_format_round_trip() {
    let mut formatted = Vec::new();
    let mut rejected = Vec::new();
    for path in dux_files(Path::new(EXAMPLE_DIR)) {
        if assert_round_trips(&path) {
            formatted.push(path.file_name().unwrap().to_string_lossy().into_owned());
        } else {
            rejected.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    for must_format in [
        "hello.dux",
        "fizzbuzz.dux",
        "structs.dux",
        "pointers.dux",
        "strings.dux",
        "sum_types.dux",
        "generics.dux",
        "types.dux",
    ] {
        assert!(
            formatted.contains(&must_format.to_string()),
            "{must_format} must format"
        );
    }
}

#[test]
fn every_clean_e2e_fixture_survives_a_format_round_trip() {
    let mut formatted = 0;
    let mut rejected = Vec::new();
    for path in dux_files(Path::new(FIXTURE_DIR)) {
        if assert_round_trips(&path) {
            formatted += 1;
        } else {
            rejected.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    assert_eq!(
        rejected,
        vec!["unterminated_string.dux".to_string()],
        "only the fixture that cannot lex may refuse to format"
    );
    assert!(formatted >= 4, "the runnable fixtures must format");
}

#[test]
fn a_broken_file_is_refused_with_its_diagnostics() {
    let failure = format_text("broken.dux", "fn main() { let x = }").unwrap_err();
    assert!(failure.has_errors());
    failure.report(&mut std::io::sink());
}

#[test]
fn layout_is_canonical_regardless_of_source_layout() {
    let source = "package   main\nimport \"std/mem\"\nfn  main( ) {\nprintln(  \"hi\" ) ;\nx:=1;\n}\nstruct P{x:i32,y:i32}\n";
    let formatted = format_text("app.dux", source).expect("the snippet parses");
    assert_eq!(
        formatted,
        concat!(
            "package main\n",
            "import \"std/mem\"\n",
            "\n",
            "fn main() {\n",
            "    println(\"hi\")\n",
            "    x := 1\n",
            "}\n",
            "\n",
            "struct P {\n",
            "    x: i32,\n",
            "    y: i32,\n",
            "}\n",
        )
    );
}

#[test]
fn literals_keep_their_canonical_spelling() {
    let source = concat!(
        "fn main() {\n",
        "    a := 1.0\n",
        "    b := \"quote:\\\" tab:\\t\"\n",
        "    c := 'q'\n",
        "    d := r\"raw\\path\"\n",
        "    e := c\"cstr\"\n",
        "    println($\"a={} b { 1 + 2 } c{{d}}\", a)\n",
        "}\n",
    );
    let formatted = format_text("literals.dux", source).expect("the snippet parses");
    assert_eq!(
        formatted,
        concat!(
            "fn main() {\n",
            "    a := 1.0\n",
            "    b := \"quote:\\\" tab:\\t\"\n",
            "    c := 'q'\n",
            "    d := r\"raw\\path\"\n",
            "    e := c\"cstr\"\n",
            "    println($\"a={} b {1 + 2} c{{d}}\", a)\n",
            "}\n",
        )
    );
}

#[test]
fn declarations_keep_their_shapes() {
    let source = concat!(
        "const KI: usize = 1024\n",
        "global hits: i32 = 0\n",
        "type Pair = (i32, i32)\n",
        "mod legacy;\n",
        "#[c_include(\"stb_image.h\")]\n",
        "extern \"C\" {\n",
        "    #[c_link(name = \"stb_image\", lib = \"st\")]\n",
        "    fn load(path: *c_char, w: *i32, h: *i32) -> *rawptr\n",
        "}\n",
        "enum Shape {\n",
        "    Empty\n",
        "    Pixel { x: i32, y: i32 }\n",
        "    Line(i32, i32)\n",
        "}\n",
        "struct Counter { value: i64 }\n",
        "impl Counter {\n",
        "    fn bump(self) -> i64 {\n",
        "        self.value += 1\n",
        "        return self.value\n",
        "    }\n",
        "}\n",
        "fn main() {\n",
        "    s := Shape.Empty\n",
        "    if s := load(0 as *c_char, 0 as *i32, 0 as *i32) { println(\"ok\") } else { println(\"no\") }\n",
        "    if hits < 3 {\n",
        "        hits += 1\n",
        "    } else if hits == 3 {\n",
        "        hits = 0\n",
        "    } else {\n",
        "        hits = 1\n",
        "    }\n",
        "    for i, v in items {\n",
        "        print($\"{i}\")\n",
        "    }\n",
        "    defer free(buf)\n",
        "    total := a * b + c - d / e % f\n",
        "    wide := (a as i64) * 2\n",
        "    ptr := &buf\n",
        "    neg := -x\n",
        "}\n",
    );
    let formatted = format_text("shapes.dux", source).expect("the snippet parses");
    assert_eq!(
        formatted,
        concat!(
            "const KI: usize = 1024\n",
            "\n",
            "global hits: i32 = 0\n",
            "\n",
            "type Pair = (i32, i32)\n",
            "\n",
            "mod legacy;\n",
            "\n",
            "#[c_include(\"stb_image.h\")]\n",
            "extern \"C\" {\n",
            "    #[c_link(name = \"stb_image\", lib = \"st\")]\n",
            "    fn load(path: *c_char, w: *i32, h: *i32) -> *rawptr\n",
            "}\n",
            "\n",
            "enum Shape {\n",
            "    Empty\n",
            "    Pixel { x: i32, y: i32 }\n",
            "    Line(i32, i32)\n",
            "}\n",
            "\n",
            "struct Counter {\n",
            "    value: i64,\n",
            "}\n",
            "\n",
            "impl Counter {\n",
            "    fn bump(self) -> i64 {\n",
            "        self.value += 1\n",
            "        return self.value\n",
            "    }\n",
            "}\n",
            "\n",
            "fn main() {\n",
            "    s := Shape.Empty\n",
            "    if s := load(0 as *c_char, 0 as *i32, 0 as *i32) {\n",
            "        println(\"ok\")\n",
            "    } else {\n",
            "        println(\"no\")\n",
            "    }\n",
            "    if hits < 3 {\n",
            "        hits += 1\n",
            "    } else if hits == 3 {\n",
            "        hits = 0\n",
            "    } else {\n",
            "        hits = 1\n",
            "    }\n",
            "    for i, v in items {\n",
            "        print($\"{i}\")\n",
            "    }\n",
            "    defer free(buf)\n",
            "    total := a * b + c - d / e % f\n",
            "    wide := (a as i64) * 2\n",
            "    ptr := &buf\n",
            "    neg := -x\n",
            "}\n",
        )
    );
}
