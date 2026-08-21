//! Preset filters, in Everything's `Filters.csv` format (features.md sec 2).
//!
//! A preset is a named search fragment - `Audio` is `ext:mp3;flac;...` - that
//! the UI offers as a dropdown. The file format is kept so users can drop in
//! their own `Filters.csv`; the built-in set below matches what v1 shipped
//! (Everything's stock filters).
//!
//! The format is a CSV with a header row; the columns we read are `Name` and
//! `Search`. Other columns (`Case`, `Whole Word`, `Path`, `Diacritics`,
//! `Regex`, `Macro`, `Key`) are accepted and currently ignored - v1 parsed
//! and ignored them too, and honoring them is listed for later milestones.

/// One preset: a display name and the search fragment it applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preset {
    pub name: String,
    /// A query fragment in the same grammar as the search box (`ext:...`,
    /// `folder:`), prepended to the user's query when the preset is active.
    pub search: String,
}

/// The stock set, matching Everything's defaults (and v1's `Filters.csv`).
pub const BUILTIN_FILTERS_CSV: &str = "\
Name,Case,Whole Word,Path,Diacritics,Regex,Search,Macro,Key
Everything,0,0,0,0,0,,,
Audio,0,0,0,0,0,ext:aac;ac3;aif;aifc;aiff;au;cda;dts;fla;flac;it;m1a;m2a;m3u;m4a;mid;midi;mka;mod;mp2;mp3;mpa;ogg;opus;ra;rmi;spc;rmi;snd;umx;voc;wav;wma;xm,audio:,
Compressed,0,0,0,0,0,ext:7z;ace;arj;bz2;cab;gz;gzip;jar;r00;r01;r02;r03;r04;r05;r06;rar;sfx;tar;tgz;z;zip,zip:,
Document,0,0,0,0,0,ext:c;chm;cpp;csv;cxx;doc;docm;docx;dot;dotm;dotx;h;hpp;htm;html;hxx;ini;java;lua;mht;mhtml;odt;pdf;potx;potm;ppam;ppsm;ppsx;pps;ppt;pptm;pptx;rtf;sldm;sldx;thmx;txt;vsd;wpd;wps;wri;xlam;xls;xlsb;xlsm;xlsx;xltm;xltx;xml,doc:,
Executable,0,0,0,0,0,ext:bat;cmd;exe;msi;msp;msu;scr,exe:,
Folder,0,0,0,0,0,folder:,folder:,
Picture,0,0,0,0,0,ext:ani;bmp;gif;ico;jpe;jpeg;jpg;pcx;png;psd;tga;tif;tiff;webp;wmf,pic:,
Video,0,0,0,0,0,ext:3g2;3gp;3gp2;3gpp;amr;amv;asf;avi;bdmv;bik;d2v;divx;drc;dsa;dsm;dss;dsv;evo;f4v;flc;fli;flic;flv;hdmov;ifo;ivf;m1v;m2p;m2t;m2ts;m2v;m4b;m4p;m4v;mkv;mp2v;mp4;mp4v;mpe;mpeg;mpg;mpls;mpv2;mpv4;mov;mts;ogm;ogv;pss;pva;qt;ram;ratdvd;rm;rmm;rmvb;roq;rpm;smil;smk;swf;tp;tpr;ts;vob;vp3;wm;wmp;wmv,video:,
";

/// Parse a `Filters.csv`. Unreadable lines are skipped - a user-edited file
/// with one bad row should not lose the other seven presets.
pub fn parse(csv: &str) -> Vec<Preset> {
    let mut lines = csv.lines();
    let Some(header) = lines.next() else {
        return Vec::new();
    };

    let columns: Vec<String> = split_csv_line(header)
        .iter()
        .map(|c| c.trim().to_ascii_lowercase())
        .collect();
    let name_at = columns.iter().position(|c| c == "name");
    let search_at = columns.iter().position(|c| c == "search");
    let (Some(name_at), Some(search_at)) = (name_at, search_at) else {
        return Vec::new();
    };

    lines
        .filter_map(|line| {
            let fields = split_csv_line(line);
            let name = fields.get(name_at)?.trim();
            if name.is_empty() {
                return None;
            }
            Some(Preset {
                name: name.to_string(),
                search: fields
                    .get(search_at)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// The built-in presets.
pub fn builtin() -> Vec<Preset> {
    parse(BUILTIN_FILTERS_CSV)
}

/// Load the user's `Filters.csv` from the config directory when present,
/// falling back to the built-in set. The file's location follows the config
/// system (`%APPDATA%\<company>\<product>\config\Filters.csv` on Windows).
pub fn load() -> Vec<Preset> {
    match user_filters_path().and_then(|path| std::fs::read_to_string(path).ok()) {
        Some(text) => {
            let presets = parse(&text);
            if presets.is_empty() {
                tracing::warn!("user Filters.csv parsed to nothing; using built-ins");
                builtin()
            } else {
                tracing::info!(count = presets.len(), "loaded user Filters.csv");
                presets
            }
        }
        None => builtin(),
    }
}

/// Where a user's own `Filters.csv` lives, beside the app config.
pub fn user_filters_path() -> Option<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from(
        crate::app::QUALIFIER,
        crate::app::COMPANY,
        crate::app::PRODUCT,
    )?;
    Some(dirs.config_dir().join("Filters.csv"))
}

/// Minimal CSV field splitting with double-quote support. `Filters.csv`
/// values can contain semicolons but not (in practice) embedded newlines,
/// so a per-line splitter is enough.
fn split_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            other => field.push(other),
        }
    }
    fields.push(field);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_set_matches_v1s_eight() {
        let names: Vec<String> = builtin().into_iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            vec![
                "Everything",
                "Audio",
                "Compressed",
                "Document",
                "Executable",
                "Folder",
                "Picture",
                "Video"
            ]
        );
    }

    #[test]
    fn preset_search_strings_use_the_query_grammar() {
        let presets = builtin();
        let audio = presets.iter().find(|p| p.name == "Audio").unwrap();
        assert!(audio.search.starts_with("ext:"));
        assert!(audio.search.contains("mp3"));

        let folder = presets.iter().find(|p| p.name == "Folder").unwrap();
        assert_eq!(folder.search, "folder:");

        let everything = presets.iter().find(|p| p.name == "Everything").unwrap();
        assert_eq!(everything.search, "", "no constraint at all");
    }

    #[test]
    fn quoted_fields_and_bad_rows_survive_parsing() {
        let csv = "\
Name,Search
\"Weird, name\",ext:a;b
,ext:orphaned-row-without-name
Plain,\"ext:c;d\"
";
        let presets = parse(csv);
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].name, "Weird, name");
        assert_eq!(presets[0].search, "ext:a;b");
        assert_eq!(presets[1].search, "ext:c;d");
    }

    #[test]
    fn a_file_without_the_needed_columns_yields_nothing() {
        assert!(parse("Foo,Bar\n1,2\n").is_empty());
        assert!(parse("").is_empty());
    }
}
