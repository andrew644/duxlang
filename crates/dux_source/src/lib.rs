use std::fmt;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct FileId(pub u32);

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "file#{}", self.0)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub len: u32,
}

impl Span {
    pub const fn new(file: FileId, start: u32, len: u32) -> Self {
        Self { file, start, len }
    }

    pub const fn empty_at(file: FileId, start: u32) -> Self {
        Self { file, start, len: 0 }
    }

    pub fn merge(self, other: Span) -> Span {
        let start = self.start.min(other.start);
        let end = (self.start + self.len).max(other.start + other.len);
        Span {
            file: self.file,
            start,
            len: end - start,
        }
    }

    pub fn end(self) -> u32 {
        self.start + self.len
    }
}

#[derive(Clone, Debug)]
struct SourceFile {
    name: String,
    text: String,
}

#[derive(Default, Debug)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: impl Into<String>, text: impl Into<String>) -> FileId {
        let name = name.into();
        if let Some(existing) = self
            .files
            .iter()
            .position(|f| f.name == name)
        {
            self.files[existing].text = text.into();
            return FileId(existing as u32);
        }
        self.files.push(SourceFile {
            name,
            text: text.into(),
        });
        FileId((self.files.len() - 1) as u32)
    }

    pub fn name_of(&self, file: FileId) -> &str {
        self.files
            .get(file.0 as usize)
            .map(|f| f.name.as_str())
            .unwrap_or("<unknown>")
    }

    pub fn text_of(&self, file: FileId) -> &str {
        self.files
            .get(file.0 as usize)
            .map(|f| f.text.as_str())
            .unwrap_or("")
    }

    pub fn line_col(&self, file: FileId, offset: u32) -> (usize, usize) {
        let text = self.text_of(file);
        let offset = (offset as usize).min(text.len());
        let mut line = 1;
        let mut col = 1;
        for byte in text.as_bytes().iter().take(offset) {
            if *byte == b'\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    pub fn line_at(&self, file: FileId, offset: u32) -> &str {
        let text = self.text_of(file);
        let offset = (offset as usize).min(text.len());
        let line_start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = text[offset..]
            .find('\n')
            .map(|i| offset + i)
            .unwrap_or(text.len());
        &text[line_start..line_end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_line_and_column() {
        let mut map = SourceMap::new();
        let file = map.add("a.dux", "one\ntwo\nthree");
        assert_eq!(map.line_col(file, 0), (1, 1));
        assert_eq!(map.line_col(file, 4), (2, 1));
        assert_eq!(map.line_col(file, 8), (3, 1));
        assert_eq!(map.line_at(file, 6), "two");
    }

    #[test]
    fn merging_spans_covers_both_ends() {
        let file = FileId(0);
        let a = Span::new(file, 3, 2);
        let b = Span::new(file, 10, 1);
        assert_eq!(a.merge(b), Span::new(file, 3, 8));
    }

    #[test]
    fn re_adding_a_name_replaces_the_text() {
        let mut map = SourceMap::new();
        let first = map.add("a.dux", "one");
        let second = map.add("a.dux", "two");
        assert_eq!(first, second);
        assert_eq!(map.text_of(first), "two");
    }
}
