//! File-type classification: extension → category, for coloring and the
//! Type column (features.md §3).
//!
//! The category list is deliberately small — it feeds the eight-slot
//! `--category-N` palette (STANDARDS §2.3). The mapping lives here in Rust
//! and rides to the webview inside each row, so the frontend never grows its
//! own forked copy of this table.

/// What kind of thing a row is, as a user thinks of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileKind {
    Directory,
    Executable,
    Archive,
    Image,
    Video,
    Audio,
    Document,
    Code,
    Other,
}

impl FileKind {
    /// Classify by name. `is_directory` wins over any extension.
    pub fn classify(name: &str, is_directory: bool) -> Self {
        if is_directory {
            return Self::Directory;
        }
        match extension_of(name).to_ascii_lowercase().as_str() {
            "exe" | "msi" | "msu" | "msp" | "bat" | "cmd" | "scr" | "com" | "ps1" | "dll"
            | "sys" => Self::Executable,
            "zip" | "7z" | "rar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "cab" | "iso" | "tar"
            | "jar" | "wim" => Self::Archive,
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "ico" | "svg"
            | "heic" | "raw" | "psd" => Self::Image,
            "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg"
            | "ts" | "m2ts" | "vob" => Self::Video,
            "mp3" | "flac" | "wav" | "m4a" | "aac" | "ogg" | "opus" | "wma" | "mid" | "midi" => {
                Self::Audio
            }
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "rtf"
            | "txt" | "md" | "csv" | "log" | "eml" | "msg" => Self::Document,
            // `.ts` is claimed by Video above (MPEG transport stream) — on a
            // disk analyzer the multi-gigabyte reading wins over TypeScript.
            "rs" | "c" | "cpp" | "h" | "hpp" | "cs" | "java" | "py" | "js" | "tsx" | "svelte"
            | "go" | "rb" | "php" | "sh" | "sql" | "json" | "xml" | "yml" | "yaml" | "toml"
            | "html" | "css" => Self::Code,
            _ => Self::Other,
        }
    }

    /// Display string for the Type column.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Directory => "Folder",
            Self::Executable => "Executable",
            Self::Archive => "Archive",
            Self::Image => "Image",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Document => "Document",
            Self::Code => "Code",
            Self::Other => "File",
        }
    }

    /// Which `--category-N` token colors this kind (1–8; STANDARDS §2.3).
    /// `Other` gets 0: neutral, no category color.
    pub fn category_slot(&self) -> u8 {
        match self {
            Self::Directory => 1,
            Self::Executable => 2,
            Self::Archive => 3,
            Self::Image => 4,
            Self::Video => 5,
            Self::Audio => 6,
            Self::Document => 7,
            Self::Code => 8,
            Self::Other => 0,
        }
    }
}

/// The extension without its dot, or `""`. Dotfiles (`.gitignore`) have no
/// extension — the leading dot is a name, not a separator.
pub fn extension_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(0) | None => "",
        Some(at) => &name[at + 1..],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_win_over_extensions() {
        assert_eq!(FileKind::classify("videos.mp4", true), FileKind::Directory);
    }

    #[test]
    fn common_extensions_classify() {
        assert_eq!(
            FileKind::classify("setup.EXE", false),
            FileKind::Executable,
            "case-insensitive"
        );
        assert_eq!(FileKind::classify("a.tar", false), FileKind::Archive);
        assert_eq!(FileKind::classify("photo.jpeg", false), FileKind::Image);
        assert_eq!(FileKind::classify("clip.mkv", false), FileKind::Video);
        assert_eq!(FileKind::classify("song.flac", false), FileKind::Audio);
        assert_eq!(FileKind::classify("notes.pdf", false), FileKind::Document);
        assert_eq!(FileKind::classify("main.rs", false), FileKind::Code);
        assert_eq!(FileKind::classify("data.bin", false), FileKind::Other);
    }

    #[test]
    fn extensions_split_correctly() {
        assert_eq!(extension_of("report.pdf"), "pdf");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
        assert_eq!(extension_of("README"), "");
        assert_eq!(extension_of(".gitignore"), "", "a dotfile has no extension");
        assert_eq!(extension_of("ends-with-dot."), "");
    }

    #[test]
    fn category_slots_stay_in_the_palette() {
        for kind in [
            FileKind::Directory,
            FileKind::Executable,
            FileKind::Archive,
            FileKind::Image,
            FileKind::Video,
            FileKind::Audio,
            FileKind::Document,
            FileKind::Code,
            FileKind::Other,
        ] {
            assert!(kind.category_slot() <= 8);
        }
    }
}
