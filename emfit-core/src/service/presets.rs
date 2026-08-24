//! Preset filters, in Everything's `Filters.csv` format (features.md sec 2).
//!
//! A preset is a named search fragment - `Audio` is `ext:mp3;flac;...` - that
//! the UI offers as a dropdown. The file format is kept so users can drop in
//! their own `Filters.csv`; the built-in set below matches what v1 shipped
//! (Everything's stock filters).
//!
//! The format is a CSV with a header row and two columns: `Name` and
//! `Search`. Any other column is ignored, which is what makes an Everything
//! export drop straight in - its `Case`, `Whole Word`, `Path`, `Diacritics`,
//! and `Regex` flags have no counterpart here because they need none. The
//! search grammar already says all of it: write `case:`, `ww:`, `path:`,
//! `diacritics:`, or `regex:` in the `Search` column and it governs the whole
//! query, the user's own text included.

/// One preset: a display name and the search it applies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preset {
    pub name: String,
    /// A query in the same grammar as the search box (`ext:...`, `folder:`,
    /// `case:`), prepended to whatever the user types while it is active.
    pub search: String,
}

/// The stock set: Everything's filters, in this file's two columns. Also what
/// gets written for the user to edit the first time they open the file.
pub const BUILTIN_FILTERS_CSV: &str = "\
Name,Search
Everything,
Audio,ext:aac;ac3;aif;aifc;aiff;au;cda;dts;fla;flac;it;m1a;m2a;m3u;m4a;mid;midi;mka;mod;mp2;mp3;mpa;ogg;opus;ra;rmi;spc;rmi;snd;umx;voc;wav;wma;xm
Compressed,ext:7z;ace;arj;bz2;cab;gz;gzip;jar;r00;r01;r02;r03;r04;r05;r06;rar;sfx;tar;tgz;z;zip
Document,ext:c;chm;cpp;csv;cxx;doc;docm;docx;dot;dotm;dotx;h;hpp;htm;html;hxx;ini;java;lua;mht;mhtml;odt;pdf;potx;potm;ppam;ppsm;ppsx;pps;ppt;pptm;pptx;rtf;sldm;sldx;thmx;txt;vsd;wpd;wps;wri;xlam;xls;xlsb;xlsm;xlsx;xltm;xltx;xml
Executable,ext:bat;cmd;exe;msi;msp;msu;scr
Folder,folder:
Picture,ext:ani;bmp;gif;ico;jpe;jpeg;jpg;pcx;png;psd;tga;tif;tiff;webp;wmf
Video,ext:3g2;3gp;3gp2;3gpp;amr;amv;asf;avi;bdmv;bik;d2v;divx;drc;dsa;dsm;dss;dsv;evo;f4v;flc;fli;flic;flv;hdmov;ifo;ivf;m1v;m2p;m2t;m2ts;m2v;m4b;m4p;m4v;mkv;mp2v;mp4;mp4v;mpe;mpeg;mpg;mpls;mpv2;mpv4;mov;mts;ogm;ogv;pss;pva;qt;ram;ratdvd;rm;rmm;rmvb;roq;rpm;smil;smk;swf;tp;tpr;ts;vob;vp3;wm;wmp;wmv
";

/// Parse a `Filters.csv`. Unreadable lines are skipped - a user-edited file
/// with one bad row should not lose the other seven presets.
pub fn parse(csv: &str) -> Vec<Preset> {
    parse_into(csv, &mut Vec::new())
}

/// [`parse`], reporting what it had to skip. The user is editing this file by
/// hand, so a row that silently vanishes is a bug report waiting to happen.
pub fn parse_into(csv: &str, problems: &mut Vec<String>) -> Vec<Preset> {
    let mut lines = csv.lines().enumerate();
    let Some((_, header)) = lines.next() else {
        problems.push("the file is empty".to_string());
        return Vec::new();
    };

    let columns: Vec<String> = split_csv_line(header)
        .iter()
        .map(|c| c.trim().to_ascii_lowercase())
        .collect();
    let at = |want: &str| columns.iter().position(|c| c == want);
    let (Some(name_at), Some(search_at)) = (at("name"), at("search")) else {
        problems.push("the header row needs a Name column and a Search column".to_string());
        return Vec::new();
    };

    lines
        .filter_map(|(at, line)| {
            if line.trim().is_empty() {
                return None;
            }
            let fields = split_csv_line(line);
            let name = fields.get(name_at).map(|f| f.trim()).unwrap_or_default();
            if name.is_empty() {
                problems.push(format!("line {}: no name, skipped", at + 1));
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

/// A loaded filter set, and the story of where it came from - which the UI
/// needs in order to say anything useful when a hand-edited file is wrong.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub presets: Vec<Preset>,
    /// Where the file lives, whether or not it exists yet.
    pub path: Option<std::path::PathBuf>,
    /// True when `presets` came from that file rather than the built-in set.
    pub from_file: bool,
    /// What was wrong with the file, if anything. Never fatal: the built-ins
    /// stand in, so a mistyped header cannot leave the app with no filters.
    pub problems: Vec<String>,
}

/// Load the user's `Filters.csv` from the config directory when present,
/// falling back to the built-in set. The file's location follows the config
/// system (`%APPDATA%\<company>\<product>\config\Filters.csv` on Windows).
pub fn load() -> Filters {
    let fallback = |path, problems| Filters {
        presets: builtin(),
        path,
        from_file: false,
        problems,
    };

    let Some(path) = user_filters_path() else {
        return fallback(None, Vec::new());
    };

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // Not having one yet is the normal state, not a problem.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fallback(Some(path), Vec::new());
        }
        Err(e) => {
            tracing::warn!(?path, error = %e, "cannot read Filters.csv");
            return fallback(Some(path), vec![format!("cannot read the file: {e}")]);
        }
    };

    let mut problems = Vec::new();
    let presets = parse_into(&text, &mut problems);
    if presets.is_empty() {
        tracing::warn!(?path, "Filters.csv holds no filters; using built-ins");
        problems.push("no filters found, so the built-in set is in use".to_string());
        return fallback(Some(path), problems);
    }

    tracing::info!(count = presets.len(), "loaded Filters.csv");
    Filters {
        presets,
        path: Some(path),
        from_file: true,
        problems,
    }
}

/// The path to the user's `Filters.csv`, creating it from the built-in set if
/// it is not there yet.
///
/// Called before handing the file to an editor: a filter list you can only
/// customize by first knowing the format and the directory is not one anybody
/// customizes.
pub fn ensure_user_file() -> crate::Result<std::path::PathBuf> {
    let path = crate::service::config::config_dir()?.join(FILE_NAME);
    if path.exists() {
        return Ok(path);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| crate::Error::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, BUILTIN_FILTERS_CSV).map_err(|source| crate::Error::Io {
        path: path.clone(),
        source,
    })?;
    tracing::info!(?path, "wrote a starter Filters.csv");
    Ok(path)
}

/// The file name, beside the app config.
pub const FILE_NAME: &str = "Filters.csv";

/// Where a user's own `Filters.csv` lives, beside the app config.
pub fn user_filters_path() -> Option<std::path::PathBuf> {
    crate::service::config::config_dir()
        .ok()
        .map(|dir| dir.join(FILE_NAME))
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
    fn an_everything_export_drops_in_and_its_extra_columns_are_ignored() {
        // The nine-column shape Everything writes. Only Name and Search are
        // read; a filter that wants case sensitivity says so in the search.
        let csv = "\
Name,Case,Whole Word,Path,Diacritics,Regex,Search,Macro,Key
Logs,1,0,0,0,0,case: ext:log,,
Mine,0,0,0,0,0,ext:txt,,
";
        let presets = parse(csv);
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].search, "case: ext:log");
        assert_eq!(presets[1].search, "ext:txt");
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

    #[test]
    fn parsing_reports_what_it_skipped() {
        let mut problems = Vec::new();
        let presets = parse_into("Name,Search\nGood,ext:a\n,ext:b\n\n", &mut problems);
        assert_eq!(presets.len(), 1);
        assert_eq!(problems, vec!["line 3: no name, skipped"]);

        problems.clear();
        parse_into("Foo,Bar\n", &mut problems);
        assert_eq!(problems.len(), 1, "a header with no usable columns says so");

        problems.clear();
        parse_into("", &mut problems);
        assert_eq!(problems, vec!["the file is empty"]);
    }
}
