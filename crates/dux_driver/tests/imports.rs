use dux_ast::{Decl, Program};
use dux_diag::{Diagnostic, Severity};
use dux_driver::analyze;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A directory of `.dux` files that disappears when the test is done with it.
struct Scratch(PathBuf);

impl Scratch {
    fn new(files: &[(&str, &str)]) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "dux-import-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().expect("a module directory")).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Scratch(dir)
    }

    fn entry(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn errors(diagnostics: &[Diagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .map(|diagnostic| diagnostic.message.clone())
        .collect()
}

fn modules(program: &Program) -> Vec<(String, usize)> {
    program
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Module(decl) => Some((decl.name.name.clone(), decl.decls.len())),
            _ => None,
        })
        .collect()
}

#[test]
fn an_import_brought_the_file_it_named_in_beside_the_one_read() {
    let scratch = Scratch::new(&[
        (
            "main.dux",
            "package main\nimport \"std/helpers\"\nfn main() { println(\"hi\") }\n",
        ),
        (
            "std/helpers.dux",
            "package helpers\nfn note(x: i32) -> i32 {\n    return x\n}\n",
        ),
    ]);
    let analyzed = analyze(&scratch.entry("main.dux")).expect("reads");
    assert_eq!(errors(&analyzed.diagnostics), Vec::<String>::new());
    assert_eq!(modules(&analyzed.program), vec![("helpers".to_string(), 1)]);
}

#[test]
fn a_module_two_files_import_is_read_once() {
    let scratch = Scratch::new(&[
        (
            "main.dux",
            "package main\nimport \"std/a\"\nimport \"std/b\"\nfn main() { println(\"hi\") }\n",
        ),
        ("std/a.dux", "import \"std/shared\"\nfn one() { }\n"),
        ("std/b.dux", "import \"std/shared\"\nfn two() { }\n"),
        ("std/shared.dux", "fn common() { }\n"),
    ]);
    let analyzed = analyze(&scratch.entry("main.dux")).expect("reads");
    assert_eq!(errors(&analyzed.diagnostics), Vec::<String>::new());
    let names: Vec<String> = modules(&analyzed.program)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        names.iter().filter(|name| **name == "shared").count(),
        1,
        "one file, however many asked for it: {names:?}"
    );
}

#[test]
fn a_module_that_is_not_there_is_said_where_it_was_asked_for() {
    let scratch = Scratch::new(&[(
        "main.dux",
        "package main\nimport \"std/nothing\"\nfn main() { println(\"hi\") }\n",
    )]);
    let analyzed = analyze(&scratch.entry("main.dux")).expect("reads");
    let reported = errors(&analyzed.diagnostics);
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert!(
        reported[0].contains("nothing"),
        "the complaint names the module: {reported:?}"
    );
    let span = analyzed.diagnostics[0].span.expect("the import statement");
    assert_eq!(
        analyzed.sources.name_of(span.file),
        scratch.entry("main.dux").display().to_string(),
        "the complaint is in the file that asked"
    );
}

#[test]
fn a_module_the_standard_library_does_not_ship_yet_is_a_milestone_gap() {
    let scratch = Scratch::new(&[(
        "main.dux",
        "package main\nimport \"std/fs\"\nfn main() { println(\"hi\") }\n",
    )]);
    let analyzed = analyze(&scratch.entry("main.dux")).expect("reads");
    let blocking: Vec<String> = analyzed
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error && d.code != dux_diag::code::UNSUPPORTED_IN_BUILD)
        .map(|d| d.message.clone())
        .collect();
    assert_eq!(
        blocking,
        Vec::<String>::new(),
        "an unwritten piece of `std` is a gap in this build, not a verdict: {blocking:?}"
    );
    assert!(analyzed
        .diagnostics
        .iter()
        .any(|d| d.message.contains("std/fs")));
}

#[test]
fn a_module_is_searched_for_nearest_first() {
    let scratch = Scratch::new(&[("main.dux", "package main\nfn main() { }\n")]);
    let roots = dux_driver::module_roots(&scratch.entry("main.dux"));
    assert_eq!(
        roots.first(),
        Some(&scratch.0),
        "the directory of the file that was named answers first: {roots:?}"
    );
}

#[test]
fn the_prelude_arrives_bare_in_every_program() {
    let prelude = "fn shipped() -> i32 {\n    return 6\n}\n";
    let main = "package main\n\nfn main() {\n    println(shipped())\n}\n";
    let scratch = Scratch::new(&[("std/prelude.dux", prelude), ("main.dux", main)]);
    let analyzed = analyze(&scratch.entry("main.dux")).expect("the program reads");
    assert_eq!(errors(&analyzed.diagnostics), Vec::<String>::new());
    assert!(
        analyzed
            .program
            .functions()
            .any(|function| function.name.name == "shipped"),
        "the prelude's declarations arrive as the program's own, not under a name"
    );
    assert!(
        !modules(&analyzed.program)
            .iter()
            .any(|(name, _)| name == "prelude"),
        "the prelude is not a namespace"
    );

    let without = Scratch::new(&[("main.dux", main)]);
    let missed = analyze(&without.entry("main.dux")).expect("the program reads");
    assert!(
        errors(&missed.diagnostics)
            .iter()
            .any(|message| message.contains("`shipped`")),
        "without a prelude the name is the program's to declare: {:?}",
        errors(&missed.diagnostics)
    );
}
