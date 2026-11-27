use std::path::{Path, PathBuf};

/// A project's `dux.mod`. The manifest answers two questions a
/// single-file program cannot: where do modules that are not in this
/// directory live, and which C libraries does the whole program assume.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    /// `[deps]`: a name and the directory it lives in, written as the file
    /// wrote it — relative paths mean relative to the manifest.
    pub deps: Vec<(String, String)>,
    /// `[cdeps]`: a name and the pkg-config package that lends its flags.
    pub cdeps: Vec<(String, String)>,
}

impl Manifest {
    /// The directories `[deps]` names, resolved against the manifest's own
    /// home — a dependency is a place, and the module roots grow to include it.
    pub fn dep_roots(&self, home: &Path) -> Vec<PathBuf> {
        self.deps.iter().map(|(_, at)| home.join(at)).collect()
    }

    pub fn cdep_packages(&self) -> Vec<&str> {
        self.cdeps
            .iter()
            .map(|(_, package)| package.as_str())
            .collect()
    }
}

/// The subset of TOML `dux.mod` speaks today: sections in brackets,
/// `key = "value"` lines, `#` comments. Anything else is left alone —
/// the manifest is small on purpose until dependency resolution is real.
pub fn parse_manifest(text: &str) -> Manifest {
    let mut manifest = Manifest::default();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        match section.as_str() {
            "package" if key == "name" => manifest.name = value.to_string(),
            "deps" => manifest.deps.push((key.to_string(), value.to_string())),
            "cdeps" => manifest.cdeps.push((key.to_string(), value.to_string())),
            _ => {}
        }
    }
    manifest
}

/// The manifest a program is built under: the nearest `dux.mod` at or above
/// the file that was named on the command line, the way the module roots
/// already read upward.
pub fn find_manifest(entry: &Path) -> Option<(PathBuf, Manifest)> {
    let start = if entry.is_dir() {
        entry.to_path_buf()
    } else {
        entry.parent()?.to_path_buf()
    };
    start.ancestors().find_map(|dir| {
        let at = dir.join("dux.mod");
        let text = std::fs::read_to_string(&at).ok()?;
        Some((at, parse_manifest(&text)))
    })
}
